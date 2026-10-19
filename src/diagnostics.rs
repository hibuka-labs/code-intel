//! Diagnostics formatting: LSP diagnostics cache → `verify`-style summary.
//!
//! Complements the verify gate that runs build commands: reads each language's LSP server
//! `publishDiagnostics` cache and reports errors as you write. Emits a
//! `file:line:col  code  message` summary so the agent never needs to read raw compiler
//! output. Only pure formatting and file collection live here; server processes and the
//! cache are in [`crate::lsp`].

use std::path::{Path, PathBuf};

use crate::lang;
use crate::lsp::{DiagnosticEntry, Severity};
use crate::walk::validate_workspace_path;

/// Summary truncation cap (aligned with the verify gate's summary budget).
const MAX_SUMMARY_CHARS: usize = 4000;

/// Collect source files to diagnose (any registered language).
///
/// `path` (optional, workspace-relative) narrows the scope to a single file or directory;
/// omitting it scans the whole workspace, skipping build artifacts (`lang::SKIP_DIRS`) and
/// hidden directories. Returns `(file list, scope base path)`; the scope is used to filter
/// the diagnostics snapshot at the end.
pub fn collect_code_files(
    root: &Path,
    path: Option<&str>,
) -> Result<(Vec<PathBuf>, Option<PathBuf>), String> {
    let scope = match path {
        Some(p) if !p.trim().is_empty() => {
            let rel = validate_workspace_path(root, p)?;
            Some(root.join(rel))
        }
        _ => None,
    };
    let base = scope.clone().unwrap_or_else(|| root.to_path_buf());

    let mut files = Vec::new();
    if base.is_file() {
        if lang::language_for_path(&base.to_string_lossy()).is_some() {
            files.push(base);
        }
        return Ok((files, scope));
    }
    walk_code_files(&base, &mut files);
    files.sort();
    Ok((files, scope))
}

/// Recursively collect source files of registered languages, skipping build artifacts and
/// hidden directories.
fn walk_code_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if lang::SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            walk_code_files(&path, out);
        } else if lang::language_for_path(&name).is_some() {
            out.push(path);
        }
    }
}

/// Format the diagnostics cache into a `verify`-style summary.
///
/// Paths are presented workspace-relative; only error / warning are kept (rust-analyzer
/// also emits hint/info, e.g. "expected due to this", which is meaningless for "does it
/// compile", so they are dropped); with no error/warning a single "no diagnostics" line is
/// returned.
pub fn format_diagnostics(entries: &[(PathBuf, Vec<DiagnosticEntry>)], root: &Path) -> String {
    let mut lines = Vec::new();
    let (mut errors, mut warnings) = (0usize, 0usize);

    for (file, diags) in entries {
        let rel = file.strip_prefix(root).unwrap_or(file);
        for d in diags {
            let code = d
                .code
                .clone()
                .unwrap_or_else(|| d.severity.label().to_string());
            match d.severity {
                Severity::Error => {
                    errors += 1;
                    lines.push(format!(
                        "  {}:{}:{}  {}  {}",
                        rel.display(),
                        d.line,
                        d.column,
                        code,
                        d.message
                    ));
                }
                Severity::Warning => {
                    warnings += 1;
                    lines.push(format!(
                        "  {}:{}:{}  {}  {}",
                        rel.display(),
                        d.line,
                        d.column,
                        code,
                        d.message
                    ));
                }
                Severity::Information | Severity::Hint => {}
            }
        }
    }

    if errors == 0 && warnings == 0 {
        return "✓ no diagnostics — rust-analyzer reports no errors or warnings".to_string();
    }

    let mut out = String::new();
    let mut head = String::new();
    if errors > 0 {
        head.push_str(&format!("{errors} error(s)"));
    }
    if warnings > 0 {
        if !head.is_empty() {
            head.push_str(", ");
        }
        head.push_str(&format!("{warnings} warning(s)"));
    }
    out.push_str(&format!("{head}:\n"));
    for l in lines {
        out.push_str(&l);
        out.push('\n');
    }

    if out.chars().count() > MAX_SUMMARY_CHARS {
        let mut truncated: String = out.chars().take(MAX_SUMMARY_CHARS).collect();
        truncated.push_str("...(truncated)\n");
        return truncated;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        severity: Severity,
        line: u32,
        column: u32,
        code: Option<&str>,
        message: &str,
    ) -> DiagnosticEntry {
        DiagnosticEntry {
            severity,
            line,
            column,
            message: message.to_string(),
            code: code.map(str::to_string),
        }
    }

    #[test]
    fn format_empty_is_a_clean_line() {
        let s = format_diagnostics(&[], Path::new("/ws"));
        assert!(s.contains("no diagnostics"), "{s}");
    }

    #[test]
    fn format_groups_errors_and_warnings() {
        let entries = vec![
            (
                PathBuf::from("/ws/src/main.rs"),
                vec![
                    entry(Severity::Error, 3, 5, Some("E0425"), "unresolved name"),
                    entry(Severity::Warning, 7, 1, None, "unused variable"),
                ],
            ),
            (
                PathBuf::from("/ws/src/lib.rs"),
                vec![entry(
                    Severity::Error,
                    1,
                    1,
                    Some("E0308"),
                    "mismatched types",
                )],
            ),
        ];
        let s = format_diagnostics(&entries, Path::new("/ws"));
        assert!(s.starts_with("2 error(s), 1 warning(s):"), "{s}");
        assert!(s.contains("src/main.rs:3:5  E0425  unresolved name"), "{s}");
        assert!(
            s.contains("src/main.rs:7:1  warning  unused variable"),
            "{s}"
        );
        assert!(s.contains("src/lib.rs:1:1  E0308  mismatched types"), "{s}");
    }

    #[test]
    fn format_skips_empty_files() {
        let entries = vec![(PathBuf::from("/ws/src/main.rs"), vec![])];
        let s = format_diagnostics(&entries, Path::new("/ws"));
        assert!(s.contains("no diagnostics"), "{s}");
    }

    #[test]
    fn collect_code_files_finds_sources_and_skips_target() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let (files, _) = collect_code_files(&root, None).unwrap();
        assert!(files.iter().any(|f| f.ends_with("src/lib.rs")), "{files:?}");
        for f in &files {
            assert!(
                lang::language_for_path(&f.to_string_lossy()).is_some(),
                "only registered source files: {f:?}"
            );
            assert!(
                !f.to_string_lossy().contains("/target/"),
                "must skip target: {f:?}"
            );
        }
    }

    #[test]
    fn collect_scopes_to_single_file() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let (files, scope) = collect_code_files(&root, Some("src/lib.rs")).unwrap();
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(files[0].ends_with("src/lib.rs"));
        assert!(scope.unwrap().ends_with("src/lib.rs"));
    }

    #[test]
    fn collect_code_files_includes_non_rust_languages() {
        let root = std::env::temp_dir().join("code_intel_diag_collect_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.ts"), "const x = 1;").unwrap();
        std::fs::write(root.join("b.cpp"), "int x;").unwrap();
        std::fs::write(root.join("c.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("README.md"), "hi").unwrap();

        let (files, _) = collect_code_files(&root, None).unwrap();
        assert_eq!(files.len(), 3, "{files:?}");
        assert!(files.iter().any(|f| f.ends_with("a.ts")));
        assert!(files.iter().any(|f| f.ends_with("b.cpp")));
        assert!(files.iter().any(|f| f.ends_with("c.rs")));

        let _ = std::fs::remove_dir_all(&root);
    }
}
