//! Language registry: the single source of truth for per-language knowledge.
//!
//! The multi-language architecture is a "registry + generic fallback":
//!
//! - The **fallback** layer is language-agnostic — file listing, ripgrep, running
//!   any build command, and showing a raw stderr tail — so *any* language is
//!   covered at zero cost (the path Codex proves works).
//! - The **registry** layer adds per-language data on demand: extension →
//!   language mapping and manifest file names (what the verify gate needs).
//!   Tree-sitter grammar and LSP specs join in later phases.
//!
//! Everything here is pure data plus cheap bounded reads — no process is ever
//! spawned and nothing consults a model.

use std::collections::HashSet;
use std::path::Path;

use serde_json::Value;

/// Optional LSP diagnostics server for a language. `None` means no LSP support —
/// the `diagnostics` tool degrades gracefully. (Java's `jdtls` needs a project
/// import step, so it's left out for now.)
#[derive(Debug, Clone)]
pub struct LspSpec {
    /// Server argv (binary + args), e.g. `["rust-analyzer"]` or
    /// `["typescript-language-server", "--stdio"]`.
    pub command: &'static [&'static str],
}

/// Per-language knowledge. Pure data — no I/O.
#[derive(Debug, Clone)]
pub struct LanguageSpec {
    /// File extensions (no leading dot) mapped to this language.
    pub extensions: &'static [&'static str],
    /// Manifest/lock file *names* that count as code (editing them dirties the gate).
    pub manifests: &'static [&'static str],
    /// Compile-level verification command (the `verify` tool's default).
    ///
    /// `None` means the language has **no compile layer** — nothing to verify at
    /// L2 (see `notes/verify-gate-design.md` §4.3). Vanilla JS and Python are
    /// the honest cases: they are interpreted, so "does it compile" is not a
    /// question that has an answer. Do **not** substitute a linter here — a
    /// lint pass is not a compile pass, and recording both as the same green
    /// light would be a lie (the same rule xwcode applies when it refuses to
    /// let non-`cargo` commands into its evidence surface).
    ///
    /// A project whose build differs from this default outranks it — see
    /// [`resolve_verify_command`].
    pub verify_command: Option<&'static str>,
    /// LSP diagnostics server, if any (used by the `diagnostics` tool).
    pub lsp: Option<LspSpec>,
}

/// The registry, in detection-priority order ([`detect_languages`] returns
/// matches in this order; the first is the dominant language).
pub static LANGUAGES: &[LanguageSpec] = &[
    LanguageSpec {
        extensions: &["rs"],
        manifests: &["Cargo.toml", "Cargo.lock"],
        // Type/borrow/trait check without codegen — the L2 gate for Rust.
        verify_command: Some("cargo check"),
        lsp: Some(LspSpec {
            command: &["rust-analyzer"],
        }),
    },
    LanguageSpec {
        extensions: &["java"],
        manifests: &["pom.xml", "build.gradle", "build.gradle.kts"],
        verify_command: Some("mvn -q compile"),
        // jdtls needs a project import step; deferred.
        lsp: None,
    },
    LanguageSpec {
        extensions: &["ts", "tsx"],
        manifests: &["tsconfig.json"],
        verify_command: Some("npx tsc --noEmit"),
        lsp: Some(LspSpec {
            command: &["typescript-language-server", "--stdio"],
        }),
    },
    LanguageSpec {
        extensions: &["js", "jsx", "mjs", "cjs"],
        manifests: &["package.json"],
        // No compile layer: vanilla JS is not type-checked. A project that runs
        // `tsc` or a bundler over its JS declares that in its own verify config.
        verify_command: None,
        lsp: Some(LspSpec {
            command: &["typescript-language-server", "--stdio"],
        }),
    },
    LanguageSpec {
        extensions: &["c", "h"],
        manifests: &["Makefile"],
        verify_command: Some("make"),
        lsp: Some(LspSpec {
            command: &["clangd"],
        }),
    },
    LanguageSpec {
        extensions: &["cpp", "cc", "cxx", "hpp", "hh", "hxx"],
        manifests: &["CMakeLists.txt"],
        // `make` assumes a Makefile target; CMake projects override this
        // (typically `cmake --build build`).
        verify_command: Some("make"),
        lsp: Some(LspSpec {
            command: &["clangd"],
        }),
    },
    LanguageSpec {
        extensions: &["py"],
        manifests: &["requirements.txt", "setup.py", "pyproject.toml"],
        // No compile layer: Python is interpreted. `mypy` is a type check but
        // not a compile step — keeping it out of the evidence surface.
        verify_command: None,
        lsp: None,
    },
];

/// Directories skipped by the bounded scan in [`detect_languages`] and the
/// diagnostics tool's file walk.
///
/// Public because "what is build output" is language-registry knowledge and
/// every consumer of [`is_code_path`] needs to agree on it — the verify tool
/// uses it to decide whether a scope contains code worth checking.
pub const SKIP_DIRS: &[&str] = &["target", "node_modules", "dist", "build", "out"];
/// Max files scanned before [`detect_languages`] gives up (bounds startup cost).
const MAX_SCAN_FILES: usize = 2000;

/// The language whose extensions include `path`'s extension, if any.
pub fn language_for_path(path: &str) -> Option<&'static LanguageSpec> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    LANGUAGES
        .iter()
        .find(|l| l.extensions.contains(&ext.as_str()))
}

/// LSP `languageId` for a path (per-extension, so finer than the language group:
/// `.tsx` → `typescriptreact`, `.jsx` → `javascriptreact`).
pub fn lsp_language_id(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "java" => "java",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        _ => return None,
    })
}

/// The LSP server spec for a path's language, if that language has one.
pub fn lsp_spec_for_path(path: &str) -> Option<&'static LspSpec> {
    language_for_path(path).and_then(|l| l.lsp.as_ref())
}

/// Whether a path is "code" for the verify gate: a known source extension, or a
/// manifest/lock file name (Cargo.toml, pom.xml, package.json, …).
pub fn is_code_path(path: &str) -> bool {
    if language_for_path(path).is_some() {
        return true;
    }
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);
    LANGUAGES.iter().any(|l| l.manifests.contains(&name))
}

/// The compile-level verification command for a single file's language.
///
/// This is a **lookup for display / hints** — a compile is a project-level act,
/// not a file-level one, so what actually runs comes from
/// [`resolve_verify_command`]. It is the table default, which is not always
/// what the project actually runs. Returns `None` for languages with no
/// compile layer (see [`LanguageSpec::verify_command`]).
pub fn verify_command_for(path: &str) -> Option<&'static str> {
    language_for_path(path)?.verify_command
}

/// The compile-level verification command the **language table** prescribes for
/// a workspace — the last of three sources, kept separate so nothing can
/// accidentally skip the project's own answer (see [`resolve_verify_command`]).
///
/// Anchored to a **manifest sitting directly in `root`**, checked in registry
/// order so a workspace with both `Cargo.toml` and `package.json` resolves to
/// the dominant language. There is deliberately no "dominant language by file
/// extension" fallback: a command derived from the source files present cannot
/// be run at a root that offers no entry point to run it against, and a
/// workspace with no root manifest is exactly that. Guessing there produces
/// `cargo check` in a directory with no `Cargo.toml` — loud, but useless, and
/// it would be nagged for as "you ran the check and it failed".
///
/// Returns `None` when the workspace has no compile layer at all (a vanilla JS
/// or Python project) — the caller must say so out loud rather than claim a
/// verification that did not happen.
pub fn table_verify_command(root: &Path) -> Option<&'static str> {
    for spec in LANGUAGES {
        for manifest in spec.manifests {
            if root.join(manifest).is_file() {
                if let Some(cmd) = spec.verify_command {
                    return Some(cmd);
                }
                // A manifest for a language with no compile layer: keep looking.
                // A JS `package.json` should not mask a `Cargo.toml` sitting next
                // to it, and it must not produce a fake green either.
            }
        }
    }
    None
}

// ── Project-level resolution ────────────────────────────────────────────────
//
// "Which command does *this* project verify with?" cannot be answered from a
// five-row table keyed by file extension: a pnpm monorepo is not
// `npx tsc --noEmit`, a Gradle build is not `mvn`, and a Cargo workspace root
// must check every member or a green light says nothing about the crate the
// model just edited. So the answer is looked up in three places, most specific
// first, and **nobody is ever asked to configure anything**:
//
//   1. `.phimint.toml`  — the escape hatch, for a build the conventions miss.
//   2. a slot the project already fills in — a Cargo `[workspace]`, a Gradle
//      wrapper, `package.json` scripts, a Makefile `check` target.
//   3. [`table_verify_command`] — the language default.
//
// When all three come up empty the workspace has no compile layer and callers
// say so rather than nagging for a verification that cannot exist.
//
// Nothing here runs a command, and nothing here consults a model. A verify
// command the model could choose is a green light the model could mint.

/// Where a workspace's verify command came from. Rendered in the `verify`
/// tool's output so a human can see at a glance whether the project is being
/// checked the way it thinks it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyCommandSource {
    /// `.phimint.toml` said so, in so many words.
    ProjectConfig,
    /// Inferred from a slot the project itself already fills in.
    ProjectConvention,
    /// The language table's default for the dominant language.
    LanguageTable,
}

impl VerifyCommandSource {
    pub fn label(self) -> &'static str {
        match self {
            VerifyCommandSource::ProjectConfig => ".phimint.toml",
            VerifyCommandSource::ProjectConvention => "项目约定",
            VerifyCommandSource::LanguageTable => "语言命令表",
        }
    }
}

/// The command to spawn for a workspace, and where that answer came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyCommand {
    pub command: String,
    pub source: VerifyCommandSource,
}

/// The compile-level verification command to actually run for a workspace.
///
/// This is the entry point; [`table_verify_command`] is only its last resort.
/// Returns `None` when the workspace has no compile layer — the caller must say
/// so out loud rather than claim a verification that did not happen.
pub fn resolve_verify_command(root: &Path) -> Option<VerifyCommand> {
    let (command, source) = verify_command_from_config(root)
        .map(|c| (c, VerifyCommandSource::ProjectConfig))
        .or_else(|| {
            verify_command_from_convention(root)
                .map(|c| (c, VerifyCommandSource::ProjectConvention))
        })
        .or_else(|| {
            table_verify_command(root)
                .map(|c| (c.to_string(), VerifyCommandSource::LanguageTable))
        })?;
    Some(VerifyCommand { command, source })
}

/// The `verify_command` key of a project-local `.phimint.toml`.
///
/// Hand-parsed rather than pulling a TOML crate into this one: the file has a
/// single key and this crate is a leaf. A line that cannot be read is skipped
/// rather than guessed at — a wrong guess here is a wrong green light, and
/// falling through to the conventions is the safe direction.
fn verify_command_from_config(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join(".phimint.toml")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "verify_command" {
            continue;
        }
        if let Some(command) = config_value(value) {
            return Some(command.to_string());
        }
    }
    None
}

/// The value half of a `key = value` config line: a quoted string up to its
/// matching quote (so a `#` inside it survives), or bare text up to a comment.
fn config_value(value: &str) -> Option<&str> {
    let value = value.trim_start();
    if let Some(quote @ ('"' | '\'')) = value.chars().next() {
        let end = value[1..].find(quote)?;
        return Some(value[1..1 + end].trim()).filter(|s| !s.is_empty());
    }
    let value = match value.find(" #") {
        Some(i) => &value[..i],
        None => value,
    };
    Some(value.trim()).filter(|s| !s.is_empty())
}

/// A command read from a slot the project itself already fills in.
fn verify_command_from_convention(root: &Path) -> Option<String> {
    cargo_command(root)
        .or_else(|| gradle_command(root))
        .or_else(|| package_json_command(root))
        .or_else(|| makefile_command(root))
}

/// `cargo check` — but `--workspace` when `Cargo.toml` declares a workspace.
///
/// Without the flag a workspace root checks only its own package, so a green
/// light could say nothing about the member the model just edited. That is the
/// false-green shape this whole module exists to prevent.
fn cargo_command(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("Cargo.toml")).ok()?;
    if declares_workspace(&text) {
        return Some("cargo check --workspace".to_string());
    }
    Some("cargo check".to_string())
}

/// Whether a `Cargo.toml` has a `[workspace]` table — including
/// `[workspace.dependencies]` and friends, which mean the same thing here.
fn declares_workspace(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        line == "[workspace]" || line.starts_with("[workspace.")
    })
}

/// The Gradle build, which the language table would answer with `mvn` — a
/// command that does not exist in that project. Prefer the wrapper: it pins the
/// Gradle version the project actually builds with.
fn gradle_command(root: &Path) -> Option<String> {
    if !(root.join("build.gradle").is_file() || root.join("build.gradle.kts").is_file()) {
        return None;
    }
    if root.join("gradlew").is_file() {
        return Some("./gradlew -q compileJava".to_string());
    }
    Some("gradle -q compileJava".to_string())
}

/// A `package.json` script, run through the package manager.
///
/// Invoked as `npm run <name>` rather than by splitting the script's own text:
/// a script is a shell fragment (`tsc && vite build`), and spawning its words
/// directly would exec a program literally named `&&`. The package manager is
/// one plain program and propagates the script's exit code, so the verdict is
/// still a real one.
fn package_json_command(root: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(root.join("package.json")).ok()?;
    let pkg: Value = serde_json::from_str(&raw).ok()?;
    if let Some(scripts) = pkg.get("scripts").and_then(Value::as_object) {
        // Closest to "does it compile" first. `build` may also emit artifacts
        // (slower) but it is the project's own definition of "it compiles",
        // which beats our guess at one.
        for name in ["typecheck", "compile", "build"] {
            let script = scripts.get(name).and_then(Value::as_str);
            if script.is_some_and(|s| !s.trim().is_empty()) {
                return Some(format!("{} run {name}", package_manager(root)));
            }
        }
    }
    // No usable script, but the project does type-check.
    if root.join("tsconfig.json").is_file() {
        return Some("npx tsc --noEmit".to_string());
    }
    None
}

/// Which package manager this project already commits a lockfile for.
fn package_manager(root: &Path) -> &'static str {
    if root.join("pnpm-lock.yaml").is_file() {
        "pnpm"
    } else if root.join("yarn.lock").is_file() {
        "yarn"
    } else if root.join("bun.lockb").is_file() || root.join("bun.lock").is_file() {
        "bun"
    } else {
        "npm"
    }
}

/// A Makefile, preferring its `check` target if it has one.
fn makefile_command(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("Makefile")).ok()?;
    if text.lines().any(is_check_target) {
        return Some("make check".to_string());
    }
    // `make` with no goal runs the first target, which is the project's own
    // definition of "the default build".
    Some("make".to_string())
}

/// A `check:` rule — never `.PHONY: check`, and never `check := x`.
fn is_check_target(line: &str) -> bool {
    let line = line.trim_start();
    if line.starts_with('#') {
        return false;
    }
    let Some((head, rest)) = line.split_once(':') else {
        return false;
    };
    // `:=`, `::=`, `:::=` are *assignments*: there the colon belongs to the
    // operator, not to a target separator. `check := lint` defines a variable
    // and gives `make check` nothing to run.
    if rest.trim_start_matches(':').starts_with('=') {
        return false;
    }
    let head = head.trim();
    // Excludes `check = x` written with spaces before a later colon, and any
    // `a=1: foo` oddity. Excludes `.PHONY: check` (head is `.PHONY`) and
    // `check-all: x` by the equality below.
    if head.contains('=') {
        return false;
    }
    head == "check"
}

/// Languages present in `root`, in registry order. A bounded directory walk
/// collects the extension set (skipping build output / vendored deps / dotdirs),
/// then each language whose extensions show up is returned.
pub fn detect_languages(root: &Path) -> Vec<&'static LanguageSpec> {
    let mut exts: HashSet<String> = HashSet::new();
    let mut count = 0usize;
    collect_extensions(root, &mut exts, &mut count);
    LANGUAGES
        .iter()
        .filter(|l| l.extensions.iter().any(|e| exts.contains(*e)))
        .collect()
}

/// Bounded recursive scan collecting file extensions into `exts`.
fn collect_extensions(dir: &Path, exts: &mut HashSet<String>, count: &mut usize) {
    if *count >= MAX_SCAN_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *count >= MAX_SCAN_FILES {
            return;
        }
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            collect_extensions(&path, exts, count);
        } else if ft.is_file() {
            *count += 1;
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                exts.insert(ext.to_ascii_lowercase());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty scratch directory under the temp dir, ready to write fixtures
    /// into. Callers clean it up when they care about disk.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn language_for_path_matches_extensions() {
        assert!(language_for_path("src/lib.rs").is_some());
        assert!(language_for_path("Foo.java").is_some());
        assert!(language_for_path("a.ts").is_some());
        assert!(language_for_path("b.tsx").is_some());
        assert!(language_for_path("c.js").is_some());
        assert!(language_for_path("d.mjs").is_some());
        assert!(
            language_for_path("e.cpp")
                .unwrap()
                .extensions
                .contains(&"cpp")
        );
        assert!(
            language_for_path("f.hpp")
                .unwrap()
                .extensions
                .contains(&"hpp")
        );
        assert!(language_for_path("g.h").unwrap().extensions.contains(&"h"));
        assert!(language_for_path("README.md").is_none());
        assert!(language_for_path("src/").is_none());
    }

    #[test]
    fn is_code_path_covers_source_and_manifests() {
        for p in [
            "src/lib.rs",
            "Foo.java",
            "a.tsx",
            "b.cpp",
            "Cargo.toml",
            "Cargo.lock",
            "pom.xml",
            "package.json",
            "tsconfig.json",
            "CMakeLists.txt",
            "Makefile",
            "docs/Cargo.toml",
        ] {
            assert!(is_code_path(p), "{p} should be code");
        }
        for p in ["README.md", "docs/notes.txt", "src/"] {
            assert!(!is_code_path(p), "{p} should not be code");
        }
    }

    #[test]
    fn docs_and_loose_config_are_not_code() {
        // Writing prose or editor config needs no compile check — see
        // `notes/verify-gate-design.md` D2. Only source and *build manifests*
        // count: a manifest decides what compiles, so editing it can break it.
        for p in [
            "README.md",
            "docs/design.md",
            "notes/a.txt",
            ".editorconfig",
            ".gitignore",
            ".eslintrc",
            "LICENSE",
            "CHANGELOG.md",
        ] {
            assert!(!is_code_path(p), "{p} must not require verification");
        }
    }

    #[test]
    fn verify_command_for_maps_source_files() {
        assert_eq!(verify_command_for("src/lib.rs"), Some("cargo check"));
        assert_eq!(verify_command_for("Foo.java"), Some("mvn -q compile"));
        assert_eq!(verify_command_for("a.ts"), Some("npx tsc --noEmit"));
        assert_eq!(verify_command_for("a.cpp"), Some("make"));
    }

    #[test]
    fn verify_command_is_none_for_languages_without_a_compile_layer() {
        // The honest gap: nothing to verify at L2, so nothing may be claimed.
        assert_eq!(verify_command_for("a.js"), None);
        assert_eq!(verify_command_for("b.jsx"), None);
        assert_eq!(verify_command_for("c.py"), None);
        // Non-code is not a compile-layer question at all.
        assert_eq!(verify_command_for("README.md"), None);
    }

    #[test]
    fn table_verify_command_prefers_root_manifest() {
        let root = scratch("code_intel_verify_ws_manifest");

        // No manifest, no sources → nothing to verify at all.
        assert_eq!(table_verify_command(&root), None);

        // A JS package.json alone has no compile layer: no command, so no
        // green light can ever be recorded for this workspace.
        std::fs::write(root.join("package.json"), "{}").unwrap();
        assert_eq!(table_verify_command(&root), None);

        // Cargo.toml beside it wins — a language with a compile layer must not
        // be masked by one without.
        std::fs::write(root.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        assert_eq!(table_verify_command(&root), Some("cargo check"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_workspace_with_no_root_manifest_is_not_guessed_at() {
        // Regression: the table used to fall back to "dominant language by file
        // extension", which answered `cargo check` for a directory whose source
        // files are Rust but whose root has no `Cargo.toml` — a command that
        // cannot run there. Manifests deeper than the root are not an entry
        // point for a root-level build; "推不出来就静默" applies.
        let root = scratch("code_intel_verify_ws_fallback");
        std::fs::create_dir_all(root.join("app/src")).unwrap();
        std::fs::write(root.join("app/src/main.ts"), "export {};").unwrap();
        assert_eq!(table_verify_command(&root), None);
        assert_eq!(resolve_verify_command(&root), None);

        std::fs::create_dir_all(root.join("rust/crates/x/src")).unwrap();
        std::fs::write(root.join("rust/crates/x/src/lib.rs"), "pub fn f() {}").unwrap();
        assert_eq!(table_verify_command(&root), None);
        assert_eq!(resolve_verify_command(&root), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn table_verify_command_says_no_for_a_compile_layer_free_project() {
        // A vanilla JS project: the caller must report "no compile-level
        // evidence" rather than claim a verification that did not happen.
        let root = scratch("code_intel_verify_ws_js");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("package.json"), "{}").unwrap();
        std::fs::write(root.join("src/index.js"), "console.log(1);").unwrap();

        assert_eq!(table_verify_command(&root), None);
        assert_eq!(resolve_verify_command(&root), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    // ── project-level resolution ──────────────────────────────────────────

    #[test]
    fn a_phimint_config_outranks_everything() {
        let root = scratch("code_intel_verify_cfg");
        std::fs::write(root.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        std::fs::write(
            root.join(".phimint.toml"),
            "verify_command = \"./scripts/build.sh --strict\"\n",
        )
        .unwrap();

        assert_eq!(
            resolve_verify_command(&root),
            Some(VerifyCommand {
                command: "./scripts/build.sh --strict".to_string(),
                source: VerifyCommandSource::ProjectConfig,
            })
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_config_line_that_cannot_be_read_falls_through_silently() {
        // A wrong guess here is a wrong green light. Falling through to the
        // conventions is the safe direction.
        let root = scratch("code_intel_verify_cfg_bad");
        std::fs::write(root.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();

        for bad in [
            "verify_command =\n",
            "verify_command = \"\"\n",
            "verify_command = \"   \"\n",
            "verify_command\n",
            "other_key = \"x\"\n",
        ] {
            std::fs::write(root.join(".phimint.toml"), bad).unwrap();
            let resolved = resolve_verify_command(&root).expect("falls back to cargo");
            assert_eq!(resolved.command, "cargo check");
            assert_eq!(resolved.source, VerifyCommandSource::ProjectConvention);
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_quoted_config_value_keeps_its_hash() {
        assert_eq!(config_value("\"make # all\""), Some("make # all"));
        assert_eq!(config_value("'make check'"), Some("make check"));
        assert_eq!(config_value("make check  # the real one"), Some("make check"));
        assert_eq!(config_value("   "), None);
    }

    #[test]
    fn a_cargo_workspace_checks_every_member() {
        // A bare `cargo check` at a workspace root checks only its own package,
        // so the green light could say nothing about the crate just edited.
        let root = scratch("code_intel_verify_cargo_ws");
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\"]\n",
        )
        .unwrap();
        let resolved = resolve_verify_command(&root).unwrap();
        assert_eq!(resolved.command, "cargo check --workspace");

        let plain = scratch("code_intel_verify_cargo_plain");
        std::fs::write(plain.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        assert_eq!(
            resolve_verify_command(&plain).unwrap().command,
            "cargo check"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&plain);
    }

    #[test]
    fn a_gradle_build_is_never_answered_with_maven() {
        // The language table's java row says `mvn`, which does not exist in a
        // Gradle project. The convention layer answers first.
        let root = scratch("code_intel_verify_gradle");
        std::fs::write(root.join("build.gradle.kts"), "plugins {}\n").unwrap();
        let resolved = resolve_verify_command(&root).unwrap();
        assert_eq!(resolved.command, "gradle -q compileJava");
        assert_eq!(resolved.source, VerifyCommandSource::ProjectConvention);

        std::fs::write(root.join("gradlew"), "#!/bin/sh\n").unwrap();
        assert_eq!(
            resolve_verify_command(&root).unwrap().command,
            "./gradlew -q compileJava"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_maven_project_is_answered_by_the_table() {
        // The one ecosystem the convention layer has no probe for. Java is
        // ahead of JavaScript in registry order, so a stray `package.json` with
        // nothing runnable in it cannot mask a `pom.xml` as "no compile layer".
        let root = scratch("code_intel_verify_maven");
        std::fs::write(root.join("pom.xml"), "<project/>\n").unwrap();
        std::fs::write(root.join("package.json"), "{}").unwrap();
        let resolved = resolve_verify_command(&root).unwrap();
        assert_eq!(resolved.command, "mvn -q compile");
        assert_eq!(resolved.source, VerifyCommandSource::LanguageTable);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_package_json_script_is_run_through_its_package_manager() {
        let root = scratch("code_intel_verify_pkg");
        std::fs::write(
            root.join("package.json"),
            r#"{"scripts":{"build":"tsc && vite build","typecheck":"tsc --noEmit"}}"#,
        )
        .unwrap();
        // `typecheck` is closer to "does it compile" than `build`, and the
        // script's own text is never split into argv — `tsc && vite build`
        // would exec a program named `&&`.
        assert_eq!(
            resolve_verify_command(&root).unwrap().command,
            "npm run typecheck"
        );

        std::fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: 9\n").unwrap();
        assert_eq!(
            resolve_verify_command(&root).unwrap().command,
            "pnpm run typecheck"
        );
        let _ = std::fs::remove_dir_all(&root);

        // No script to lean on, but the project does type-check.
        let root = scratch("code_intel_verify_pkg_tsc");
        std::fs::write(root.join("package.json"), r#"{"scripts":{}}"#).unwrap();
        std::fs::write(root.join("tsconfig.json"), "{}").unwrap();
        assert_eq!(
            resolve_verify_command(&root).unwrap().command,
            "npx tsc --noEmit"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_makefile_check_target_is_preferred_and_a_variable_is_not() {
        assert!(is_check_target("check: build"));
        assert!(is_check_target("\tcheck:"));
        // A double-colon rule is still a rule.
        assert!(is_check_target("check:: build"));
        assert!(!is_check_target(".PHONY: check"));
        assert!(!is_check_target("check := lint"));
        assert!(!is_check_target("check::= lint"));
        assert!(!is_check_target("check-all:"));
        assert!(!is_check_target("# check: commented out"));

        let root = scratch("code_intel_verify_make");
        std::fs::write(root.join("Makefile"), ".PHONY: check\ncheck: build\n").unwrap();
        assert_eq!(
            resolve_verify_command(&root).unwrap().command,
            "make check"
        );

        std::fs::write(root.join("Makefile"), "all:\n\techo hi\n").unwrap();
        assert_eq!(resolve_verify_command(&root).unwrap().command, "make");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn detect_languages_scans_extensions_and_skips_build_dirs() {
        let root = std::env::temp_dir().join("code_intel_lang_detect_test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/main/java/com/x")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("src/main/java/com/x/Foo.java"), "class Foo {}").unwrap();
        std::fs::write(root.join("target/Ignored.java"), "class Ignored {}").unwrap();

        let langs = detect_languages(&root);
        assert_eq!(langs.len(), 1);
        assert!(langs[0].extensions.contains(&"java"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn lsp_language_id_maps_extensions() {
        assert_eq!(lsp_language_id("a.rs"), Some("rust"));
        assert_eq!(lsp_language_id("a.java"), Some("java"));
        assert_eq!(lsp_language_id("a.ts"), Some("typescript"));
        assert_eq!(lsp_language_id("a.tsx"), Some("typescriptreact"));
        assert_eq!(lsp_language_id("a.js"), Some("javascript"));
        assert_eq!(lsp_language_id("a.jsx"), Some("javascriptreact"));
        assert_eq!(lsp_language_id("a.cpp"), Some("cpp"));
        assert_eq!(lsp_language_id("a.h"), Some("c"));
        assert_eq!(lsp_language_id("README.md"), None);
    }

    #[test]
    fn lsp_spec_for_path_routes_to_server() {
        assert_eq!(
            lsp_spec_for_path("a.rs").unwrap().command[0],
            "rust-analyzer"
        );
        assert_eq!(
            lsp_spec_for_path("a.ts").unwrap().command[0],
            "typescript-language-server"
        );
        assert_eq!(lsp_spec_for_path("a.cpp").unwrap().command[0], "clangd");
        // Java deferred: no LSP server registered.
        assert!(lsp_spec_for_path("a.java").is_none());
    }
}
