//! Hand-rolled minimal LSP client (multi-server): diagnostics only.
//!
//! Depends on `lsp-types` only for protocol type primitives (`InitializeParams`/`Diagnostic`,
//! etc.); JSON-RPC framing (Content-Length), the handshake, server process management, and the
//! `publishDiagnostics` cache are all hand-rolled — no heavy LSP client wrapper dependency.
//!
//! Architecture (high cohesion, low coupling):
//! - Pure functions (no I/O, unit-testable): `frame_message` / `decode_frames` frame
//!   encode/decode, `build_*` message construction, `parse_publish_diagnostics` /
//!   `flatten_diagnostic` parsing.
//! - `LspClient` (stateful): starts one server (argv given by the caller) and spawns
//!   reader / driver background threads for reading stdout / writing stdin; diagnostics are
//!   cached in `Arc<Mutex<HashMap<…>>>` for the tool layer to read. Startup or handshake
//!   failures are recorded in `state.error`; the tool layer sees them via `health()` and
//!   degrades to shell commands.
//! - `LspManager`: lazily routes each file's language to the matching server (processes are
//!   shared). Routing rules are not hard-coded — the constructor injects
//!   `impl Fn(&Path) -> Option<LspServerSpec>` (typically wrapping
//!   `lang::lsp_spec_for_path`); `ClientInfo` (the client identity in the initialize
//!   handshake) is likewise injected at construction.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use lsp_types::{
    ClientCapabilities, Diagnostic, DiagnosticSeverity, DidChangeTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, InitializeParams, InitializedParams,
    NumberOrString, PublishDiagnosticsParams, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, Url, VersionedTextDocumentIdentifier,
    WorkspaceFolder,
    notification::{
        DidChangeTextDocument, DidOpenTextDocument, DidSaveTextDocument, Initialized, Notification,
        PublishDiagnostics,
    },
    request::{Initialize, Request},
};
use serde_json::{Value, json};

/// Protocol type re-export: consumers can build `ClientInfo` without depending on lsp-types
/// directly.
pub use lsp_types::ClientInfo;

/// Handshake (initialize response) timeout.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(20);

/// Resolved LSP server launch spec (argv; the first element is the binary name).
///
/// `command` is a `Vec<String>` rather than the registry's `&'static [&'static str]`: the
/// resolver is injected, so specs may be constructed at runtime (not from a static table).
#[derive(Debug, Clone)]
pub struct LspServerSpec {
    pub command: Vec<String>,
}

/// Path → the LSP server spec for that language. `None` means the language has no server
/// (the tool layer degrades).
///
/// code-intel binds to no built-in language registry: routing rules are injected by the
/// consumer at `LspManager` construction.
pub type SpecResolver = dyn Fn(&Path) -> Option<LspServerSpec> + Send + Sync;

// ---------------- Flattened diagnostics ----------------

/// Flattened diagnostic severity (decoupled from lsp_types' `DiagnosticSeverity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

impl Severity {
    /// Label column in the summary (fallback when the diagnostic has no code).
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Information => "info",
            Severity::Hint => "hint",
        }
    }
}

/// Flattened diagnostic entry: 1-based line/column, message, optional error code. The tool
/// layer never sees protocol types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticEntry {
    pub severity: Severity,
    pub line: u32,
    pub column: u32,
    pub message: String,
    pub code: Option<String>,
}

// ---------------- Pure functions (no I/O, unit-testable) ----------------

/// Encode one JSON-RPC message into a Content-Length frame (body counted in **bytes**).
pub fn frame_message(body: &Value) -> String {
    let body = serde_json::to_string(body).expect("lsp message serializes");
    format!("Content-Length: {}\r\n\r\n{}", body.len(), body)
}

/// Parse complete frames out of a byte buffer.
///
/// Returns `(parsed JSON messages, consumed byte count)`. Stops on an incomplete frame
/// header/body and keeps the unconsumed bytes — `pos` points at the first unparsed byte so
/// the caller can truncate the buffer with `drain(..pos)`.
pub fn decode_frames(buf: &[u8]) -> (Vec<Value>, usize) {
    let mut frames = Vec::new();
    let mut pos = 0usize;

    while let Some(off) = buf[pos..].windows(4).position(|w| w == &b"\r\n\r\n"[..]) {
        let header_end = pos + off;
        let header = &buf[pos..header_end];

        let mut content_len = None;
        if let Ok(h) = std::str::from_utf8(header) {
            for line in h.split("\r\n") {
                if let Some(v) = line.strip_prefix("Content-Length:") {
                    content_len = v.trim().parse::<usize>().ok();
                }
            }
        }
        let Some(len) = content_len else {
            // Header missing Content-Length: skip one byte and continue to avoid an infinite loop.
            pos += 1;
            continue;
        };

        let body_start = header_end + 4;
        let body_end = body_start + len;
        if buf.len() < body_end {
            break; // Incomplete body; wait for more bytes.
        }
        if let Ok(v) = serde_json::from_slice::<Value>(&buf[body_start..body_end]) {
            frames.push(v);
        }
        pos = body_end;
    }

    (frames, pos)
}

/// JSON-RPC envelopes for requests / notifications / responses.
fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}
fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// Absolute path → `file://` URI (rust-analyzer identifies documents by URI).
pub fn uri_from_path(path: &Path) -> Url {
    Url::from_file_path(path).unwrap_or_else(|()| {
        // Fallback: in practice `from_file_path` always succeeds for absolute paths.
        Url::parse(&format!("file://{}", path.to_string_lossy())).expect("path converts to url")
    })
}

/// `file://` URI → absolute path; returns None for non-file schemes or invalid paths.
pub fn path_from_uri(uri: &Url) -> Option<PathBuf> {
    if uri.scheme() != "file" {
        return None;
    }
    uri.to_file_path().ok()
}

/// Build the `initialize` request (declares only `workspaceFolders`, avoiding the
/// deprecated `rootUri`).
///
/// `client_info` is the client identity (name/version) in the handshake, injected by the
/// consumer rather than hard-coded.
pub fn build_initialize_request(id: u64, workspace_root: &Path, client_info: ClientInfo) -> Value {
    let uri = uri_from_path(workspace_root);
    let name = workspace_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string();
    let params = InitializeParams {
        process_id: Some(std::process::id()),
        workspace_folders: Some(vec![WorkspaceFolder { uri, name }]),
        capabilities: ClientCapabilities::default(),
        client_info: Some(client_info),
        ..Default::default()
    };
    request(
        id,
        Initialize::METHOD,
        serde_json::to_value(params).expect("initialize params serialize"),
    )
}

/// Build the `initialized` notification (second handshake step).
pub fn build_initialized_notification() -> Value {
    notification(
        Initialized::METHOD,
        serde_json::to_value(InitializedParams {}).expect("initialized params serialize"),
    )
}

/// Build the `textDocument/didOpen` notification (version=1). `language_id` comes from the
/// registry (`lang::lsp_language_id`), e.g. "rust"/"typescript"/"cpp".
pub fn build_did_open(uri: &Url, text: &str, language_id: &str) -> Value {
    let params = DidOpenTextDocumentParams {
        text_document: TextDocumentItem::new(uri.clone(), language_id.into(), 1, text.to_string()),
    };
    notification(
        DidOpenTextDocument::METHOD,
        serde_json::to_value(params).expect("didOpen serialize"),
    )
}

/// Build the `textDocument/didChange` notification (full sync: omitting range = whole
/// document replacement).
pub fn build_did_change(uri: &Url, version: i32, text: &str) -> Value {
    let params = DidChangeTextDocumentParams {
        text_document: VersionedTextDocumentIdentifier::new(uri.clone(), version),
        content_changes: vec![TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: text.to_string(),
        }],
    };
    notification(
        DidChangeTextDocument::METHOD,
        serde_json::to_value(params).expect("didChange serialize"),
    )
}

/// Build the `textDocument/didSave` notification (with text, triggering rust-analyzer's
/// checkOnSave → cargo check → publishDiagnostics).
pub fn build_did_save(uri: &Url, text: &str) -> Value {
    let params = DidSaveTextDocumentParams {
        text_document: TextDocumentIdentifier { uri: uri.clone() },
        text: Some(text.to_string()),
    };
    notification(
        DidSaveTextDocument::METHOD,
        serde_json::to_value(params).expect("didSave serialize"),
    )
}

/// Flatten lsp_types' `Diagnostic` into a `DiagnosticEntry` (0-based → 1-based).
pub fn flatten_diagnostic(d: &Diagnostic) -> DiagnosticEntry {
    let severity = if d.severity == Some(DiagnosticSeverity::ERROR) {
        Severity::Error
    } else if d.severity == Some(DiagnosticSeverity::WARNING) {
        Severity::Warning
    } else if d.severity == Some(DiagnosticSeverity::INFORMATION) {
        Severity::Information
    } else if d.severity == Some(DiagnosticSeverity::HINT) {
        Severity::Hint
    } else {
        // severity is optional per the spec; conservatively treat it as error to highlight it.
        Severity::Error
    };
    let code = match &d.code {
        Some(NumberOrString::Number(n)) => Some(n.to_string()),
        Some(NumberOrString::String(s)) => Some(s.clone()),
        None => None,
    };
    DiagnosticEntry {
        severity,
        line: d.range.start.line + 1,
        column: d.range.start.character + 1,
        // rust-analyzer sometimes emits multi-line messages (e.g.
        // `mismatched types\nexpected …`); collapse them to one line so each diagnostic is
        // one line in the summary (aligned with verify's `--> file:line:col`).
        message: d.message.split_whitespace().collect::<Vec<_>>().join(" "),
        code,
    }
}

/// Extract `(absolute path, diagnostic list)` from `textDocument/publishDiagnostics` params.
///
/// Returns None for non-file schemes or deserialization failure (the message is ignored).
pub fn parse_publish_diagnostics(params: &Value) -> Option<(PathBuf, Vec<DiagnosticEntry>)> {
    let p: PublishDiagnosticsParams = serde_json::from_value(params.clone()).ok()?;
    let path = path_from_uri(&p.uri)?;
    let entries = p.diagnostics.iter().map(flatten_diagnostic).collect();
    Some((path, entries))
}

// ---------------- Stateful client ----------------

/// Commands sent to the driver thread.
enum LspCommand {
    /// Open/update one file (didOpen or didChange, then didSave to trigger save-time checks).
    Sync {
        path: PathBuf,
        content: String,
        language_id: String,
    },
}

/// Observable state: read by the tool layer, written by the driver thread.
#[derive(Default)]
struct LspState {
    ready: bool,
    error: Option<String>,
}

/// Handle to an LSP server client. Cloning it shares the same process and cache.
pub struct LspClient {
    server_name: String,
    tx: Sender<LspCommand>,
    diagnostics: Arc<Mutex<HashMap<PathBuf, Vec<DiagnosticEntry>>>>,
    state: Arc<Mutex<LspState>>,
    child: Arc<Mutex<Option<Child>>>,
    reader: Mutex<Option<JoinHandle<()>>>,
    driver: Mutex<Option<JoinHandle<()>>>,
}

impl LspClient {
    /// Start one LSP server and complete the handshake in the background.
    ///
    /// `command` is the argv (first element is the binary name, the rest are args). Never
    /// panics: a spawn failure records the error in `state.error`, which `health()` reports;
    /// `sync` / `snapshot` become no-ops / empty, and the tool layer degrades accordingly.
    /// `client_info` reports the client identity in the initialize handshake.
    pub fn start(
        workspace_root: &Path,
        command: &[&str],
        client_info: ClientInfo,
    ) -> Arc<LspClient> {
        let server_name = command.first().copied().unwrap_or("lsp-server");
        let args = command.get(1..).unwrap_or(&[]);

        let diagnostics = Arc::new(Mutex::new(HashMap::new()));
        let state = Arc::new(Mutex::new(LspState::default()));
        let child = Arc::new(Mutex::new(None::<Child>));
        let (tx, rx) = mpsc::channel::<LspCommand>();

        let client = Arc::new(LspClient {
            server_name: server_name.to_string(),
            tx,
            diagnostics: diagnostics.clone(),
            state: state.clone(),
            child: child.clone(),
            reader: Mutex::new(None),
            driver: Mutex::new(None),
        });

        let mut proc = match std::process::Command::new(server_name)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(p) => p,
            Err(e) => {
                state.lock().unwrap().error = Some(format!("failed to spawn {server_name}: {e}"));
                return client;
            }
        };

        let stdin = proc.stdin.take();
        let stdout = proc.stdout.take();
        *child.lock().unwrap() = Some(proc);

        let (Some(mut stdin), Some(stdout)) = (stdin, stdout) else {
            state.lock().unwrap().error =
                Some(format!("{server_name} did not expose stdin/stdout"));
            return client;
        };

        // Reader thread: blocking reads from stdout, decodes frames, sends JSON messages to
        // the driver.
        let (msg_tx, msg_rx) = mpsc::channel::<Value>();
        let reader = std::thread::spawn(move || {
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            let mut stdout = stdout;
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => break, // EOF: process exited or pipe closed.
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
                let (frames, consumed) = decode_frames(&buf);
                buf.drain(..consumed);
                for f in frames {
                    if msg_tx.send(f).is_err() {
                        return; // Driver already exited.
                    }
                }
            }
            // EOF: explicitly drop the sender so the driver's recv sees Disconnected.
            drop(msg_tx);
        });

        // Driver thread: handshake + handling server messages + consuming sync commands.
        let workspace = workspace_root.to_path_buf();
        let diag = diagnostics.clone();
        let st = state.clone();
        let name = server_name.to_string();
        let driver = std::thread::spawn(move || {
            // 1) initialize.
            if stdin_write(
                &mut stdin,
                &build_initialize_request(1, &workspace, client_info),
            )
            .is_err()
            {
                st.lock().unwrap().error = Some("failed to write initialize".into());
                return;
            }
            // 2) Wait for the initialize response (skipping leading window/logMessage
            // notifications).
            'init: loop {
                match msg_rx.recv_timeout(INITIALIZE_TIMEOUT) {
                    Ok(msg) => {
                        if msg.get("id").and_then(Value::as_u64) == Some(1) {
                            if msg.get("error").is_some() {
                                st.lock().unwrap().error =
                                    Some("initialize returned an error".into());
                                return;
                            }
                            break 'init;
                        }
                        // Ignore other pre-response messages (logMessage etc.).
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        st.lock().unwrap().error =
                            Some("timed out waiting for initialize response".into());
                        return;
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        st.lock().unwrap().error = Some(format!("{name} exited during handshake"));
                        return;
                    }
                }
            }
            // 3) initialized.
            if stdin_write(&mut stdin, &build_initialized_notification()).is_err() {
                st.lock().unwrap().error = Some("failed to write initialized".into());
                return;
            }
            st.lock().unwrap().ready = true;

            // Version numbers of open files (written exclusively by the driver).
            let mut open_versions: HashMap<PathBuf, i32> = HashMap::new();

            loop {
                // Drain sync commands first (non-blocking, so commands queued before
                // startup are handled too).
                loop {
                    match rx.try_recv() {
                        Ok(LspCommand::Sync {
                            path,
                            content,
                            language_id,
                        }) => {
                            let uri = uri_from_path(&path);
                            match open_versions.get(&path) {
                                Some(_) => {
                                    let v = open_versions.entry(path.clone()).or_insert(0);
                                    *v += 1;
                                    let _ = stdin_write(
                                        &mut stdin,
                                        &build_did_change(&uri, *v, &content),
                                    );
                                }
                                None => {
                                    open_versions.insert(path.clone(), 1);
                                    let _ = stdin_write(
                                        &mut stdin,
                                        &build_did_open(&uri, &content, &language_id),
                                    );
                                }
                            }
                            // didSave triggers save-time checks (e.g. rust-analyzer's
                            // checkOnSave → cargo check → publish).
                            let _ = stdin_write(&mut stdin, &build_did_save(&uri, &content));
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return, // Client was dropped.
                    }
                }

                // Handle one server message (block up to 100ms so sync commands can
                // interleave).
                match msg_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(msg) => handle_server_message(&msg, &diag, &mut stdin),
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return, // rust-analyzer exited.
                }
            }
        });

        *client.reader.lock().unwrap() = Some(reader);
        *client.driver.lock().unwrap() = Some(driver);
        client
    }

    /// Report whether the client is currently usable. Returns an error on startup/handshake
    /// failure so the tool layer can degrade.
    pub fn health(&self) -> Result<(), String> {
        let st = self.state.lock().unwrap();
        if let Some(e) = &st.error {
            return Err(e.clone());
        }
        if !st.ready {
            return Err(format!("{} is still starting", self.server_name));
        }
        Ok(())
    }

    /// Whether startup has already failed (as opposed to "still handshaking"), for
    /// `wait_ready` to check.
    pub fn failed_error(&self) -> Option<String> {
        self.state.lock().unwrap().error.clone()
    }

    /// Request diagnostics for one file (fire-and-forget, consumed by the driver thread).
    pub fn sync(&self, path: &Path, content: &str, language_id: &str) {
        let _ = self.tx.send(LspCommand::Sync {
            path: path.to_path_buf(),
            content: content.to_string(),
            language_id: language_id.to_string(),
        });
    }

    /// Snapshot the current diagnostics cache (cloned, sorted by path).
    pub fn snapshot(&self) -> Vec<(PathBuf, Vec<DiagnosticEntry>)> {
        let mut out: Vec<_> = self
            .diagnostics
            .lock()
            .unwrap()
            .iter()
            .map(|(p, d)| (p.clone(), d.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Kill the process first: the reader hits EOF and exits → channel closes → the
        // driver's recv sees Disconnected and exits. Then join both threads to finish up.
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(r) = self.reader.lock().unwrap().take() {
            let _ = r.join();
        }
        if let Some(d) = self.driver.lock().unwrap().take() {
            let _ = d.join();
        }
    }
}

/// Handle one message from the server: publishDiagnostics goes into the cache, server→client
/// requests always get a result (`workspace/configuration` gets an empty array), other
/// notifications are ignored.
fn handle_server_message(
    msg: &Value,
    diagnostics: &Arc<Mutex<HashMap<PathBuf, Vec<DiagnosticEntry>>>>,
    stdin: &mut ChildStdin,
) {
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        return; // Response message (we only send requests during the handshake; nothing to do here).
    };

    if method == PublishDiagnostics::METHOD {
        if let Some(params) = msg.get("params")
            && let Some((path, entries)) = parse_publish_diagnostics(params)
        {
            diagnostics.lock().unwrap().insert(path, entries);
        }
    } else if let Some(id) = msg.get("id") {
        // Server→client requests (workspace/configuration, client/registerCapability, etc.).
        let result = if method == "workspace/configuration" {
            json!([])
        } else {
            Value::Null
        };
        let resp = json!({ "jsonrpc": "2.0", "id": id.clone(), "result": result });
        let _ = stdin_write(stdin, &resp);
    }
    // Remaining pure notifications (window/logMessage, $/progress, etc.) are ignored.
}

/// Write one message to the server's stdin (written exclusively by the driver thread).
fn stdin_write(stdin: &mut ChildStdin, msg: &Value) -> std::io::Result<()> {
    let framed = frame_message(msg);
    stdin.write_all(framed.as_bytes())?;
    stdin.flush()
}

// ---------------- Multi-server routing ----------------

/// A lazily-started set of LSP servers, routed by file language.
///
/// Routing rules (which path uses which server) and the client identity (`ClientInfo`) are
/// both injected at construction; code-intel binds to no concrete language registry. All
/// files that resolve to the same argv first element share one server process (TS and JS
/// share typescript-language-server). Languages for which the resolver returns `None` have
/// no server, and the tool layer degrades accordingly.
pub struct LspManager {
    workspace_root: PathBuf,
    client_info: ClientInfo,
    resolve: Box<SpecResolver>,
    /// key = server binary name (first element): one server process shared by several
    /// languages.
    clients: Mutex<HashMap<String, Arc<LspClient>>>,
}

impl LspManager {
    /// Construct: inject the workspace root, handshake client identity, and a
    /// path→server-spec resolver.
    pub fn new(
        workspace_root: PathBuf,
        client_info: ClientInfo,
        resolve: impl Fn(&Path) -> Option<LspServerSpec> + Send + Sync + 'static,
    ) -> Self {
        Self {
            workspace_root,
            client_info,
            resolve: Box::new(resolve),
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// The LspClient for `path`'s language (lazily started, process-shared); returns None
    /// when the injected resolver returns None for the path.
    pub fn client_for(&self, path: &Path) -> Option<Arc<LspClient>> {
        let spec = (self.resolve)(path)?;
        let key = spec.command.first()?.clone();
        let argv: Vec<&str> = spec.command.iter().map(String::as_str).collect();
        let mut clients = self.clients.lock().unwrap();
        Some(
            clients
                .entry(key)
                .or_insert_with(|| {
                    LspClient::start(&self.workspace_root, &argv, self.client_info.clone())
                })
                .clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};

    fn test_client_info() -> ClientInfo {
        ClientInfo {
            name: "code-intel-test".into(),
            version: None,
        }
    }

    #[test]
    fn frame_message_uses_byte_length() {
        // The Chinese text in the body is counted in bytes (not characters).
        let body = json!({ "msg": "中文" });
        let framed = frame_message(&body);
        let body_len = serde_json::to_string(&body).unwrap().len();
        assert!(
            framed.starts_with(&format!("Content-Length: {body_len}\r\n\r\n")),
            "{framed:?}"
        );
    }

    #[test]
    fn decode_frames_parses_single_and_multiple() {
        let a = json!({ "method": "one" });
        let b = json!({ "method": "two" });
        let mut buf = frame_message(&a).into_bytes();
        buf.extend_from_slice(frame_message(&b).as_bytes());

        let (frames, consumed) = decode_frames(&buf);
        assert_eq!(frames.len(), 2, "{frames:?}");
        assert_eq!(frames[0]["method"], "one");
        assert_eq!(frames[1]["method"], "two");
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn decode_frames_handles_partial_body() {
        let full = frame_message(&json!({ "method": "hello" }));
        let bytes = full.as_bytes();
        // Cut off the tail of the body.
        let (frames, consumed) = decode_frames(&bytes[..bytes.len() - 3]);
        assert!(frames.is_empty());
        assert_eq!(consumed, 0, "incomplete body must not be consumed");
    }

    #[test]
    fn decode_frames_handles_partial_header() {
        let (frames, consumed) = decode_frames(b"Content-Length: 2\r\n");
        assert!(frames.is_empty());
        assert_eq!(consumed, 0);
    }

    #[test]
    fn decode_frames_skips_garbage_between_frames() {
        let good = frame_message(&json!({ "method": "ok" }));
        let mut buf = b"garbage\r\n".to_vec();
        buf.extend_from_slice(good.as_bytes());
        let (frames, _) = decode_frames(&buf);
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(frames[0]["method"], "ok");
    }

    #[test]
    fn roundtrip_frame_and_decode() {
        let body = json!({ "jsonrpc": "2.0", "id": 7, "result": [1, 2, 3] });
        let framed = frame_message(&body);
        let (frames, consumed) = decode_frames(framed.as_bytes());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], body);
        assert_eq!(consumed, framed.len());
    }

    #[test]
    fn build_did_open_has_correct_shape() {
        let uri = Url::parse("file:///tmp/a.rs").unwrap();
        let msg = build_did_open(&uri, "fn main() {}", "rust");
        assert_eq!(msg["method"], "textDocument/didOpen");
        assert_eq!(msg["params"]["textDocument"]["uri"], "file:///tmp/a.rs");
        assert_eq!(msg["params"]["textDocument"]["languageId"], "rust");
        assert_eq!(msg["params"]["textDocument"]["version"], 1);
    }

    #[test]
    fn build_did_open_respects_language_id() {
        let uri = Url::parse("file:///tmp/a.tsx").unwrap();
        let msg = build_did_open(&uri, "const x = 1;", "typescriptreact");
        assert_eq!(
            msg["params"]["textDocument"]["languageId"],
            "typescriptreact"
        );
    }

    #[test]
    fn build_did_change_is_full_sync() {
        let uri = Url::parse("file:///tmp/a.rs").unwrap();
        let msg = build_did_change(&uri, 3, "let x = 1;");
        assert_eq!(msg["params"]["textDocument"]["version"], 3);
        let cc = &msg["params"]["contentChanges"][0];
        assert!(cc["range"].is_null(), "full sync omits range");
        assert_eq!(cc["text"], "let x = 1;");
    }

    #[test]
    fn build_initialize_request_shape() {
        let msg = build_initialize_request(1, Path::new("/tmp/ws"), test_client_info());
        assert_eq!(msg["method"], "initialize");
        assert_eq!(msg["id"], 1);
        let folders = msg["params"]["workspaceFolders"].as_array().unwrap();
        assert_eq!(folders.len(), 1);
        assert!(msg["params"]["capabilities"].is_object());
    }

    #[test]
    fn client_info_flows_into_initialize_request() {
        // Injection equivalence: the client identity comes from the constructor args,
        // not hard-coded.
        let msg = build_initialize_request(
            1,
            Path::new("/tmp/ws"),
            ClientInfo {
                name: "my-agent".into(),
                version: Some("9.9".into()),
            },
        );
        assert_eq!(msg["params"]["clientInfo"]["name"], "my-agent");
        assert_eq!(msg["params"]["clientInfo"]["version"], "9.9");
    }

    #[test]
    fn manager_routes_through_injected_resolver() {
        // Injection equivalence: routing is decided by the resolver. A language returning
        // None gets no client (and no spawn); a language returning a fake binary name gets a
        // spawn failure recorded in health (graceful degradation). No panics anywhere, and no
        // dependency on a real LSP server.
        let mgr = LspManager::new(PathBuf::from("/tmp/ws"), test_client_info(), |p: &Path| {
            if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                Some(LspServerSpec {
                    command: vec!["definitely-not-a-real-lsp-binary".to_string()],
                })
            } else {
                None
            }
        });
        assert!(mgr.client_for(Path::new("/tmp/x/py.py")).is_none());
        let client = mgr.client_for(Path::new("/tmp/x/a.rs")).unwrap();
        assert!(
            client.health().is_err(),
            "spawn failure must surface in health()"
        );
        assert!(client.failed_error().is_some());
        assert!(client.snapshot().is_empty());
    }

    #[test]
    fn flatten_diagnostic_maps_severity_and_offsets() {
        let d = Diagnostic::new(
            Range::new(Position::new(2, 3), Position::new(2, 9)),
            Some(DiagnosticSeverity::ERROR),
            Some(NumberOrString::String("E0308".into())),
            None,
            "mismatched types".into(),
            None,
            None,
        );
        let e = flatten_diagnostic(&d);
        assert_eq!(e.severity, Severity::Error);
        assert_eq!(e.line, 3); // 0-based 2 → 1-based 3
        assert_eq!(e.column, 4); // 0-based 3 → 1-based 4
        assert_eq!(e.code.as_deref(), Some("E0308"));
        assert_eq!(e.message, "mismatched types");
    }

    #[test]
    fn flatten_diagnostic_numeric_code() {
        let d = Diagnostic::new(
            Range::new(Position::new(0, 0), Position::new(0, 1)),
            Some(DiagnosticSeverity::WARNING),
            Some(NumberOrString::Number(42)),
            None,
            "warn".into(),
            None,
            None,
        );
        assert_eq!(flatten_diagnostic(&d).code.as_deref(), Some("42"));
    }

    #[test]
    fn flatten_diagnostic_collapses_multiline_message() {
        let d = Diagnostic::new(
            Range::new(Position::new(0, 0), Position::new(0, 1)),
            Some(DiagnosticSeverity::ERROR),
            None,
            None,
            "mismatched types\nexpected `u32`, found `&str`".into(),
            None,
            None,
        );
        assert_eq!(
            flatten_diagnostic(&d).message,
            "mismatched types expected `u32`, found `&str`"
        );
    }

    #[test]
    fn parse_publish_diagnostics_extracts_path_and_entries() {
        // The path must be absolute **on the platform running the test**:
        // `Url::from_file_path` rejects a path it cannot express as a file
        // URL, and `/tmp/...` is not a Windows path. So the fixture is built
        // from the platform's own root — and the round-trip must give the
        // same path back.
        let path = if cfg!(windows) {
            Path::new(r"C:\ws\src\main.rs")
        } else {
            Path::new("/tmp/ws/src/main.rs")
        };
        let uri = Url::from_file_path(path).unwrap();
        let params = PublishDiagnosticsParams {
            uri,
            diagnostics: vec![Diagnostic::new(
                Range::new(Position::new(4, 5), Position::new(4, 6)),
                Some(DiagnosticSeverity::ERROR),
                Some(NumberOrString::String("E0425".into())),
                None,
                "unresolved name".into(),
                None,
                None,
            )],
            version: None,
        };
        let params = serde_json::to_value(params).unwrap();
        let (got, entries) = parse_publish_diagnostics(&params).unwrap();
        assert_eq!(got, path);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].line, 5);
        assert_eq!(entries[0].column, 6);
        assert_eq!(entries[0].code.as_deref(), Some("E0425"));
    }

    #[test]
    fn path_from_uri_rejects_non_file() {
        let uri = Url::parse("untitled:Untitled-1").unwrap();
        assert!(path_from_uri(&uri).is_none());
    }
}
