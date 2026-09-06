//! Content search: wrap ripgrep for symbol/string lookup.
//!
//! Runs `rg --json` scoped to a workspace root and formats matches as
//! `file:line: text`. Synchronous by design — callers that own an async
//! runtime wrap it in `spawn_blocking`.

use std::path::Path;
use std::process::Stdio;

use serde_json::Value;

use crate::walk::validate_workspace_path;

/// Hard cap on the formatted result so a broad search can't flood the LLM.
const MAX_RESULT_CHARS: usize = 4000;

/// Core search logic (sync, testable).
///
/// Runs `rg --json` scoped to `workspace_root` and formats matches as
/// `file:line: text`. `path` is an optional workspace-relative file/dir.
/// `max_matches` caps matches per file.
pub fn search(
    workspace_root: &Path,
    pattern: &str,
    path: Option<&str>,
    max_matches: usize,
) -> Result<String, String> {
    if pattern.is_empty() {
        return Err("empty pattern".to_string());
    }

    let scope = match path {
        Some(p) if !p.trim().is_empty() => validate_workspace_path(workspace_root, p)?,
        _ => ".".to_string(),
    };

    let mut cmd = std::process::Command::new("rg");
    cmd.arg("--json")
        .arg("--max-count")
        .arg(max_matches.to_string())
        .arg("--max-filesize")
        .arg("1M");
    crate::walk::apply_common_excludes(&mut cmd);
    cmd.arg("--")
        .arg(pattern)
        .arg(&scope)
        .current_dir(workspace_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = cmd.output().map_err(|e| format!("failed to run rg: {e}"))?;

    // Exit codes: 0 = matches, 1 = no matches, 2 = error (bad pattern, etc.).
    if output.status.code() == Some(2) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("rg error: {}", stderr.trim()));
    }

    let mut out = String::new();
    let mut shown = 0usize;
    for raw in output.stdout.split(|&b| b == b'\n') {
        if raw.is_empty() {
            continue;
        }
        let obj: Value = match serde_json::from_slice(raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if obj.get("type").and_then(Value::as_str) != Some("match") {
            continue;
        }
        let data = &obj["data"];
        let file = data["path"]["text"].as_str().unwrap_or("");
        let line_no = data["line_number"].as_u64().unwrap_or(0);
        let text = data["lines"]["text"].as_str().unwrap_or("").trim_end();

        let entry = format!("{file}:{line_no}: {text}\n");
        if out.len() + entry.len() > MAX_RESULT_CHARS {
            out.push_str("...(truncated)\n");
            break;
        }
        out.push_str(&entry);
        shown += 1;
    }

    if shown == 0 {
        return Ok(format!("No matches for '{pattern}' in '{scope}'."));
    }

    Ok(format!("{shown} match(es) for '{pattern}':\n{out}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crate_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn search_finds_matching_lines() {
        let r = search(&crate_root(), "pub fn search", None, 20).unwrap();
        assert!(r.contains("src/ripgrep.rs"), "{r}");
        assert!(r.contains("pub fn search"), "{r}");
    }

    #[test]
    fn search_scopes_to_file() {
        let r = search(&crate_root(), "fn search", Some("src/ripgrep.rs"), 20).unwrap();
        assert!(r.contains("fn search"), "{r}");
    }

    #[test]
    fn search_rejects_empty_pattern() {
        assert_eq!(
            search(&crate_root(), "", None, 20).unwrap_err(),
            "empty pattern"
        );
    }
}
