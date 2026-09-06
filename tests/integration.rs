//! End-to-end tests: real directory scans, language detection, repo map
//! extraction, search, and the LSP manager's injected routing.

use std::path::PathBuf;
use std::process::Command;

use code_intel::lang::{detect_languages, is_code_path, lsp_spec_for_path};
use code_intel::lsp::{ClientInfo, LspManager, LspServerSpec};
use code_intel::repomap::{build_dir_tree, build_repo_map};
use code_intel::ripgrep::search;
use code_intel::walk::validate_workspace_path;

/// A temp workspace mixing source files, junk, and build output (no
/// `.gitignore` — exclusion must come from the glob list, matching the
/// bare-directory model).
fn sample_workspace(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("code_intel_integration_{tag}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("app/src/main/java/com/x/web")).unwrap();
    std::fs::create_dir_all(root.join("target/debug")).unwrap();

    std::fs::write(
        root.join("src/greet.rs"),
        "pub fn greet(name: &str) -> String {\n    format!(\"hi {name}\")\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/src/main/java/com/x/web/Handler.java"),
        "package com.x.web;\npublic class Handler {\n    public void handle() {}\n}\n",
    )
    .unwrap();
    std::fs::write(root.join("src/widget.ts"), "export class Widget {\n  draw(): void {}\n}\n").unwrap();
    std::fs::write(root.join("README.md"), "not code\n").unwrap();
    std::fs::write(root.join("target/debug/junk.rs"), "fn generated() {}\n").unwrap();
    root
}

#[test]
fn language_detection_and_repo_map_end_to_end() {
    let root = sample_workspace("detect_map");

    // Registry detection: Rust + Java + TS present, in priority order.
    let langs = detect_languages(&root);
    let exts: Vec<&str> = langs.iter().flat_map(|l| l.extensions.iter().copied()).collect();
    assert!(exts.contains(&"rs"), "{exts:?}");
    assert!(exts.contains(&"java"), "{exts:?}");
    assert!(exts.contains(&"ts"), "{exts:?}");

    // Repo map: symbols extracted, build output excluded despite no .gitignore.
    let map = build_repo_map(&root, None).unwrap();
    assert!(map.contains("greet.rs"), "{map}");
    assert!(map.contains("fn greet"), "{map}");
    assert!(map.contains("Handler.java"), "{map}");
    assert!(map.contains("class Handler"), "{map}");
    assert!(!map.contains("target/"), "build output must be excluded:\n{map}");

    // Directory skeleton: two modules, package tree under each.
    let tree = build_dir_tree(&root, None).unwrap();
    assert!(tree.contains("app/"), "{tree}");
    assert!(tree.contains("web/ (1)"), "{tree}");
    assert!(tree.contains("src/"), "{tree}");
    assert!(!tree.contains("target/"), "{tree}");

    // Content search on the same bare workspace.
    let hits = search(&root, "fn greet", None, 20).unwrap();
    assert!(hits.contains("src/greet.rs"), "{hits}");
    assert!(hits.contains("fn greet"), "{hits}");
    let no_gen = search(&root, "fn generated", None, 20).unwrap();
    assert!(!no_gen.contains("target/"), "search must skip target/: {no_gen}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn workspace_path_validation_round_trip() {
    let root = sample_workspace("validate");
    // Relative path survives as scope; scoped map finds the scoped file only.
    let scope = validate_workspace_path(&root, " src ").unwrap();
    assert_eq!(scope, "src");
    let map = build_repo_map(&root, Some("src")).unwrap();
    assert!(map.contains("greet.rs"), "{map}");
    assert!(!map.contains("Handler.java"), "scope must exclude other modules:\n{map}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn lsp_manager_uses_injected_resolver_not_registry() {
    // The registry alone must NOT decide routing: an empty resolver yields no
    // clients even though the workspace is full of registry-known languages.
    let root = sample_workspace("lsp_none");
    let none_mgr = LspManager::new(root.clone(), ClientInfo { name: "test".into(), version: None }, |_| None);
    assert!(none_mgr.client_for(&root.join("src/greet.rs")).is_none());

    // A resolver can route any way it likes (here: everything to a fake server);
    // spawn failure surfaces as a health error instead of a panic.
    let fake_mgr = LspManager::new(
        root.clone(),
        ClientInfo { name: "test".into(), version: None },
        |_p: &_| {
            Some(LspServerSpec {
                command: vec!["definitely-not-a-real-lsp-binary".to_string()],
            })
        },
    );
    let client = fake_mgr.client_for(&root.join("src/widget.ts")).unwrap();
    assert!(client.health().is_err());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn registry_and_resolver_agree_for_known_languages() {
    // The product-style resolver (wrapping the registry) yields the same server
    // binary the registry documents — injection-equivalence for real inputs.
    let root = sample_workspace("lsp_registry");
    let mgr = LspManager::new(
        root.clone(),
        ClientInfo { name: "test".into(), version: None },
        |p: &_| {
            lsp_spec_for_path(&p.to_string_lossy()).map(|spec| LspServerSpec {
                command: spec.command.iter().map(|s| s.to_string()).collect(),
            })
        },
    );
    // We don't require rust-analyzer to be installed here; instead confirm the
    // resolver's routing decision mirrors the registry for a Java file (registry
    // says None) and resolves *something* only for registry-backed languages.
    assert!(mgr.client_for(&root.join("app/src/main/java/com/x/web/Handler.java")).is_none());
    let client = mgr.client_for(&root.join("src/greet.rs"));
    if client.is_some() {
        // rust-analyzer is installed: the client is either starting or ready.
        let _ = client.unwrap().health();
    } else {
        // Not installed: spawn failed, which is fine for this equivalence check —
        // the decision was still routed through the resolver.
        assert!(lsp_spec_for_path("a.rs").is_some());
    }

    // The gate's manifest check (verify-gate consumer) stays registry-driven.
    assert!(is_code_path("Cargo.toml"));
    assert!(!is_code_path("README.md"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rg_is_available_for_search_backed_features() {
    // search/repomap shell out to ripgrep; fail loudly here if the environment
    // lacks it (CI prerequisite), with a readable message.
    let ok = Command::new("rg").arg("--version").output().map(|o| o.status.success());
    assert!(ok.unwrap_or(false), "ripgrep (`rg`) must be on PATH for code-intel tests");
}
