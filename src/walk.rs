//! Shared file-walk helpers: user-path validation and the filters both tools
//! apply when they walk a workspace in-process.

use std::path::Path;

/// Validate a user-supplied path and return it trimmed.
///
/// There is no workspace sandbox: absolute paths and `..` traversal are allowed,
/// matching the framework file tools and Claude Code's model — safety comes from
/// the approval layer (`auto`/`ask`/`deny`), not path boundaries. Callers pass
/// the result to `root.join(..)`, which treats an absolute path as absolute and
/// a `..` path as escaping the root. Only the empty path is rejected.
pub fn validate_workspace_path(_root: &Path, user_path: &str) -> Result<String, String> {
    let trimmed = user_path.trim();
    if trimmed.is_empty() {
        return Err("empty path".to_string());
    }
    Ok(trimmed.to_string())
}

/// Workspace-relative, `/`-separated form of `path`.
///
/// Results are formatted as `file:line: ...` and their consumers split on `/`,
/// so the walker's platform separators are normalised here. A path outside the
/// root (an absolute `scope`) is kept as given.
pub(crate) fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Whether `rel` lives under build output or vendored dependencies.
///
/// `target/` and `node_modules/` must stay out even in a bare directory with no
/// `.gitignore` — a coding agent often runs on a temp workspace. The check is
/// on the top-level component, matching the previous `!target/**` globs.
pub(crate) fn is_excluded(rel: &str) -> bool {
    matches!(rel.split('/').next(), Some("target" | "node_modules"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_only_empty_paths() {
        assert!(validate_workspace_path(Path::new("/ws"), "").is_err());
        assert!(validate_workspace_path(Path::new("/ws"), "   ").is_err());
        assert_eq!(
            validate_workspace_path(Path::new("/ws"), " src/lib.rs ").unwrap(),
            "src/lib.rs"
        );
        // No sandbox: traversal and absolute paths pass through.
        assert_eq!(
            validate_workspace_path(Path::new("/ws"), "../etc/passwd").unwrap(),
            "../etc/passwd"
        );
        assert_eq!(
            validate_workspace_path(Path::new("/ws"), "/abs/path").unwrap(),
            "/abs/path"
        );
    }

    #[test]
    fn relative_paths_use_forward_slashes_and_exclude_build_output() {
        let root = Path::new("/ws");
        assert_eq!(
            relative_path(root, Path::new("/ws/src/lib.rs")),
            "src/lib.rs"
        );
        // Absolute scopes stay absolute.
        assert_eq!(
            relative_path(root, Path::new("/elsewhere/a.rs")),
            "/elsewhere/a.rs"
        );
        assert!(is_excluded("target/debug/build.rs"));
        assert!(is_excluded("node_modules/pkg/index.js"));
        assert!(!is_excluded("crates/foo/target/x.rs"));
        assert!(!is_excluded("src/main.rs"));
    }
}
