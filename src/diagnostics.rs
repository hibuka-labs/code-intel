//! Diagnostics formatting: LSP 诊断缓存 → `verify` 风格摘要。
//!
//! 与跑编译命令的 verify 门互补：读各语言 LSP server 的 `publishDiagnostics`
//! 缓存，边写边报错。输出 `file:line:col  code  message` 摘要，agent 无需读
//! 原始编译输出。这里只有纯格式化与文件收集；server 进程与缓存见 [`crate::lsp`]。

use std::path::{Path, PathBuf};

use crate::lang;
use crate::lsp::{DiagnosticEntry, Severity};
use crate::walk::validate_workspace_path;

/// 摘要截断上限（对齐 verify 门的摘要预算）。
const MAX_SUMMARY_CHARS: usize = 4000;

/// 收集要诊断的源码文件（任意已注册语言）。
///
/// `path`（可选，workspace 相对）收敛到单个文件或目录；省略则扫整个 workspace，
/// 跳过 build 产物（`lang::SKIP_DIRS`）与隐藏目录。返回 `(文件列表, 范围基准路径)`，
/// 范围用于最后过滤诊断快照。
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

/// 递归收集已注册语言的源码文件，跳过 build 产物与隐藏目录。
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

/// 把诊断缓存格式化成 `verify` 风格的摘要。
///
/// 路径以 workspace 相对形式呈现；只保留 error / warning（rust-analyzer 还会发
/// hint/info，如「expected due to this」，对「编不过」无意义，舍弃）；无 error/warning
/// 时返回单行「无诊断」。
pub fn format_diagnostics(entries: &[(PathBuf, Vec<DiagnosticEntry>)], root: &Path) -> String {
    let mut lines = Vec::new();
    let (mut errors, mut warnings) = (0usize, 0usize);

    for (file, diags) in entries {
        let rel = file.strip_prefix(root).unwrap_or(file);
        for d in diags {
            let code = d.code.clone().unwrap_or_else(|| d.severity.label().to_string());
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

    fn entry(severity: Severity, line: u32, column: u32, code: Option<&str>, message: &str) -> DiagnosticEntry {
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
                vec![entry(Severity::Error, 1, 1, Some("E0308"), "mismatched types")],
            ),
        ];
        let s = format_diagnostics(&entries, Path::new("/ws"));
        assert!(s.starts_with("2 error(s), 1 warning(s):"), "{s}");
        assert!(s.contains("src/main.rs:3:5  E0425  unresolved name"), "{s}");
        assert!(s.contains("src/main.rs:7:1  warning  unused variable"), "{s}");
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
            assert!(!f.to_string_lossy().contains("/target/"), "must skip target: {f:?}");
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
