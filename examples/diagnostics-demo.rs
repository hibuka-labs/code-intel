//! diagnostics-demo: run real LSP diagnostics over an intentionally broken
//! Rust file. Shows the full wiring a product needs:
//!
//! 1. a temp workspace with a bad `main.rs`;
//! 2. an `LspManager` with the registry-based resolver + a `ClientInfo`;
//! 3. collect → sync → wait → snapshot → format, then print the summary.
//!
//! Run: `cargo run --example diagnostics-demo`
//! (requires `rust-analyzer` on PATH).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use code_intel::diagnostics::{collect_code_files, format_diagnostics};
use code_intel::lang;
use code_intel::lsp::{ClientInfo, LspManager, LspServerSpec};

fn main() {
    // 1) A workspace with one broken file — a real Cargo project so
    //    rust-analyzer discovers the workspace and checkOnSave runs cargo check.
    let root = std::env::temp_dir().join("code_intel_diagnostics_demo");
    let _ = std::fs::remove_dir_all(&root);
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        src.join("main.rs"),
        "fn main() {\n    let x: u32 = \"not a number\";\n    undefined_function(x);\n}\n",
    )
    .unwrap();
    println!("workspace: {}", root.display());

    // 2) The manager: registry-based routing (what a product typically injects)
    //    plus the product's client identity for the initialize handshake.
    let manager = Arc::new(LspManager::new(
        root.clone(),
        ClientInfo {
            name: "diagnostics-demo".to_string(),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        },
        |p: &Path| {
            lang::lsp_spec_for_path(&p.to_string_lossy()).map(|spec| LspServerSpec {
                command: spec.command.iter().map(|s| s.to_string()).collect(),
            })
        },
    ));

    // 3) Collect source files, route + sync each to its server.
    let (files, scope) = collect_code_files(&root, None).expect("collect");
    let mut clients: Vec<Arc<code_intel::lsp::LspClient>> = Vec::new();
    for f in &files {
        let Some(client) = manager.client_for(f) else {
            continue;
        };
        let Some(language_id) = lang::lsp_language_id(&f.to_string_lossy()) else {
            continue;
        };
        let content = std::fs::read_to_string(f).unwrap();
        if !clients.iter().any(|c| Arc::ptr_eq(c, &client)) {
            clients.push(client.clone());
        }
        client.sync(f, &content, language_id);
    }
    if clients.is_empty() {
        eprintln!("no LSP server for these files (is rust-analyzer installed?)");
        return;
    }

    // 4) Wait for the handshake, then poll for save-time checks to publish
    //    (first run pays cargo check's cold-start cost, so be generous). RA
    //    clears diagnostics with an empty publish on load — wait for entries,
    //    not just for the file to appear in the snapshot.
    wait_ready(&clients);
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut filtered: Vec<(PathBuf, Vec<code_intel::lsp::DiagnosticEntry>)>;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let snapshot: Vec<_> = clients.iter().flat_map(|c| c.snapshot()).collect();
        filtered = match &scope {
            Some(sp) => snapshot.into_iter().filter(|(p, _)| p.starts_with(sp)).collect(),
            None => snapshot,
        };
        if filtered.iter().any(|(_, d)| !d.is_empty()) || Instant::now() >= deadline {
            break;
        }
    }

    // 5) Format.
    print!("{}", format_diagnostics(&filtered, &root));

    let _ = std::fs::remove_dir_all(&root);
}

/// Block until every server is ready or the startup deadline passes.
fn wait_ready(clients: &[Arc<code_intel::lsp::LspClient>]) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if clients.iter().all(|c| c.health().is_ok()) || clients.iter().any(|c| c.failed_error().is_some()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
