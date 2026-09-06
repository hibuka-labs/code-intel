# code-intel

Language intelligence toolkit for coding agents: multi-server LSP diagnostics,
tree-sitter repo maps, ripgrep search, and a per-language registry — all as
plain, framework-free functions over `Path`/`&str`.

Part of the [buka-works](https://github.com/hibuka-labs) family; extracted from
the [phimint](https://github.com/hibuka-labs) coding agent.

[![Crates.io](https://img.shields.io/crates/v/code-intel.svg)](https://crates.io/crates/code-intel)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

## Capabilities

| Module | Feature | What it does |
|---|---|---|
| `lang` | — | Registry: extensions, manifests, LSP server specs, bounded language detection |
| `lsp` | `lsp` | Hand-rolled multi-server LSP client (diagnostics only): JSON-RPC framing, handshake, per-language process routing, `publishDiagnostics` cache |
| `diagnostics` | `lsp` | Cache → terse `file:line:col  code  message` summaries; workspace source collection |
| `repomap` | `repomap` | tree-sitter symbol map (Rust + 6 more languages) and Maven-aware module/package skeleton |
| `ripgrep` | — | `rg --json` content search wrapper |
| `walk` | — | User-path validation, shared `rg` glob excludes |

Design points:

- **Injection, not hardwiring.** `LspManager` takes the path → server routing
  (`impl Fn(&Path) -> Option<LspServerSpec>`) and the `ClientInfo` identity at
  construction — a product typically wraps `lang::lsp_spec_for_path`.
- **Fully synchronous.** No async runtime: std threads + channels inside, plain
  blocking calls outside. Wrap in `spawn_blocking` if you own a runtime.
- **Graceful degradation.** A server that won't spawn or handshake records the
  error in `health()`; nothing panics, snapshots just come back empty.
- **Zero framework dependencies.** No tool trait, no agent runtime — easy to
  bolt onto any LLM agent shell.

## Usage

```toml
[dependencies]
code-intel = "0.1"                       # default: lsp + repomap
# or just diagnostics without the 7 tree-sitter grammars:
code-intel = { version = "0.1", default-features = false, features = ["lsp"] }
```

```rust
use std::path::Path;
use code_intel::lsp::{ClientInfo, LspManager, LspServerSpec};
use code_intel::lang;

// Route by the built-in registry, identify as your product.
let manager = LspManager::new(
    workspace_root,
    ClientInfo { name: "my-agent".into(), version: Some("0.1.0".into()) },
    |p: &Path| {
        lang::lsp_spec_for_path(&p.to_string_lossy()).map(|s| LspServerSpec {
            command: s.command.iter().map(|c| c.to_string()).collect(),
        })
    },
);

let client = manager.client_for(&Path::new("src/main.rs")).unwrap();
client.sync(path, content, "rust");
// ... wait for health(), then:
for (file, diags) in client.snapshot() { /* ... */ }
```

Repo map, search, and diagnostics formatting need no setup:

```rust
let map = code_intel::repomap::build_repo_map(&root, Some("src"))?;
let hits = code_intel::ripgrep::search(&root, "fn main", None, 20)?;
let files = code_intel::diagnostics::collect_code_files(&root, None)?;
```

External prerequisites: `rg` (ripgrep) for `repomap`/`ripgrep`; a language
server (e.g. `rust-analyzer`) for live `lsp` diagnostics.

Run the live demo: `cargo run --example diagnostics-demo`.

## License

MIT — see [LICENSE](LICENSE).
