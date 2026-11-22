//! Content search: regex lookup scoped to the workspace.
//!
//! The walk runs in-process (`ignore`) and matching uses `regex::bytes` — no
//! `rg --json` subprocess. That keeps the tool working where ripgrep is not
//! installed, and saves a 60 ms+ child process on Windows. Matching is
//! line-oriented like before: patterns do not span lines, and results are
//! `file:line: text`.

use std::path::Path;

use crate::walk::{is_excluded, relative_path, validate_workspace_path};

/// Hard cap on the formatted result so a broad search cannot flood the model.
const MAX_RESULT_CHARS: usize = 4000;
/// Files bigger than this are skipped, matching the previous `--max-filesize 1M`.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Search `pattern` under `workspace_root`, optionally scoped to `path`.
///
/// `max_matches` caps hits *per file* (the old `--max-count`); `0` means no cap.
/// A scope that is a file searches just that file.
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

    let regex = regex::bytes::Regex::new(pattern).map_err(|e| format!("invalid pattern: {e}"))?;
    let per_file_cap = if max_matches == 0 {
        usize::MAX
    } else {
        max_matches
    };

    let base = workspace_root.join(&scope);
    let mut out = String::new();
    let mut shown = 0usize;
    let mut full = false;

    for entry in ignore::WalkBuilder::new(&base).build() {
        // One unreadable directory must not abort the search: skip the entry.
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let file = entry.path();
        let rel = relative_path(workspace_root, file);
        if is_excluded(&rel) {
            continue;
        }
        if entry.metadata().map(|m| m.len()).unwrap_or(0) > MAX_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(file) else {
            continue;
        };
        // ripgrep treats any NUL byte as a binary file and skips it; so do we,
        // otherwise UTF-16 and blobs would produce garbage hits.
        if bytes.contains(&0) {
            continue;
        }

        let mut in_file = 0usize;
        for (idx, raw) in bytes.split(|b| *b == b'\n').enumerate() {
            if in_file >= per_file_cap {
                break;
            }
            let line = raw.strip_suffix(b"\r").unwrap_or(raw);
            if !regex.is_match(line) {
                continue;
            }
            let text = String::from_utf8_lossy(line);
            let hit = format!("{rel}:{}: {}\n", idx + 1, text.trim_end());
            if out.len() + hit.len() > MAX_RESULT_CHARS {
                out.push_str("...(truncated)\n");
                full = true;
                break;
            }
            out.push_str(&hit);
            shown += 1;
            in_file += 1;
        }
        if full {
            break;
        }
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

    #[test]
    fn max_matches_caps_each_file() {
        let r = search(&crate_root(), "fn ", Some("src/walk.rs"), 2).unwrap();
        assert!(r.starts_with("2 match(es)"), "{r}");
    }

    #[test]
    fn works_on_a_bare_directory_and_skips_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.rs"), "needle\n").unwrap();
        std::fs::write(dir.path().join("blob.bin"), b"needle\0needle").unwrap();
        // Build output stays out even without a .gitignore.
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::write(dir.path().join("target/debug/gen.rs"), "needle\n").unwrap();

        let r = search(dir.path(), "needle", None, 20).unwrap();
        assert!(r.contains("note.rs"), "{r}");
        assert!(!r.contains("blob.bin"), "{r}");
        assert!(!r.contains("target/"), "{r}");
    }
}
