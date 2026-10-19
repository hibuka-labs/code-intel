//! Shared file-walk helpers: user-path validation and `rg` glob excludes.

use std::path::Path;

/// Validate a user-supplied path and return it trimmed.
///
/// There is no workspace sandbox: absolute paths and `..` traversal are allowed,
/// matching the framework file tools and Claude Code's model — safety comes from
/// the approval layer (`auto`/`ask`/`deny`), not path boundaries. Callers may
/// pass the result to `rg` (with `current_dir` set to the workspace root) or to
/// `root.join(..)`; both treat an absolute path as absolute and a `..` path as
/// escaping the root. Only the empty path is rejected.
pub fn validate_workspace_path(_root: &Path, user_path: &str) -> Result<String, String> {
    let trimmed = user_path.trim();
    if trimmed.is_empty() {
        return Err("empty path".to_string());
    }
    Ok(trimmed.to_string())
}

/// Apply the exclusions shared by both tools (build output, vendored deps).
///
/// These are explicit `--glob` negations rather than relying on `.gitignore`:
/// ripgrep only honors `.gitignore` inside a git repository, but a coding agent
/// often runs on a bare directory (like a temp smoke workspace), where
/// `target/` etc. must still be ignored.
pub fn apply_common_excludes(cmd: &mut std::process::Command) {
    for glob in ["!target/**", "!node_modules/**"] {
        cmd.arg("--glob").arg(glob);
    }
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
    fn excludes_are_explicit_negation_globs() {
        let mut cmd = std::process::Command::new("rg");
        apply_common_excludes(&mut cmd);
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "--glob".to_string(),
                "!target/**".to_string(),
                "--glob".to_string(),
                "!node_modules/**".to_string()
            ]
        );
    }
}
