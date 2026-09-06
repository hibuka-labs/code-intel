//! Language intelligence toolkit: the codebase-aware side of a coding agent.
//!
//! Four capabilities, each independently consumable:
//!
//! - [`lang`] — the per-language registry: extensions, manifests, LSP server
//!   specs, and a bounded directory scan for language detection.
//! - [`lsp`] *(feature `lsp`)* — a hand-rolled multi-server LSP client that only
//!   does diagnostics: JSON-RPC framing, handshake, per-language process
//!   routing, and a `publishDiagnostics` cache. Server routing and client
//!   identity are injected at construction, not hardwired.
//! - [`diagnostics`] *(feature `lsp`)* — formatting the diagnostics cache into
//!   terse `file:line:col  code  message` summaries, plus workspace file
//!   collection.
//! - [`repomap`] *(feature `repomap`)* — tree-sitter symbol extraction: a terse
//!   structural map (per-file symbols) and a module → package directory
//!   skeleton for large workspaces.
//! - [`ripgrep`] — thin wrapper over `rg --json` for content search.
//! - [`walk`] — shared path/exclusion helpers (`rg` glob excludes, user-path
//!   validation).
//!
//! Zero framework dependencies: every public entry point here is a plain
//! function or type over `Path`/`&str` — no agent runtime, no tool trait, no
//! async. Tools that expose these to an LLM stay in the product layer.
//!
//! # Example
//!
//! Detect the languages of a workspace and build a repo map of it:
//!
//! ```
//! use std::path::Path;
//!
//! let root = temp_dir_with_example_files();
//!
//! // 1) Which languages live here?
//! let langs = code_intel::lang::detect_languages(&root);
//! assert!(langs.iter().any(|l| l.extensions.contains(&"rs")));
//!
//! // 2) A structural map of the Rust file.
//! # #[cfg(feature = "repomap")]
//! # {
//! let map = code_intel::repomap::build_repo_map(&root, None).unwrap();
//! assert!(map.contains("hello.rs"), "{map}");
//! assert!(map.contains("fn greet"), "{map}");
//! # }
//!
//! # fn temp_dir_with_example_files() -> std::path::PathBuf {
//! #     let root = std::env::temp_dir().join("code_intel_doctest");
//! #     let _ = std::fs::remove_dir_all(&root);
//! #     std::fs::create_dir_all(&root).unwrap();
//! #     std::fs::write(root.join("hello.rs"), "fn greet(name: &str) -> String { format!(\"hi {name}\") }\n").unwrap();
//! #     root
//! # }
//! ```

pub mod lang;
pub mod ripgrep;
pub mod walk;

#[cfg(feature = "lsp")]
pub mod diagnostics;
#[cfg(feature = "lsp")]
pub mod lsp;
#[cfg(feature = "repomap")]
pub mod repomap;
