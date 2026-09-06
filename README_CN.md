# code-intel

面向编码 agent 的语言智能工具箱:多 server LSP 诊断、tree-sitter 仓库地图、
ripgrep 内容搜索、语言注册表——全部是围绕 `Path`/`&str` 的普通函数,
不绑定任何框架。

[buka-works](https://github.com/hibuka-labs) 家族成员;从编码 agent
[phimint](https://github.com/hibuka-labs) 中沉淀而来。

[![Crates.io](https://img.shields.io/crates/v/code-intel.svg)](https://crates.io/crates/code-intel)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

## 能力一览

| 模块 | 特性 | 功能 |
|---|---|---|
| `lang` | — | 注册表:扩展名、manifest、LSP server 规格、有界语言探测 |
| `lsp` | `lsp` | 手写多 server LSP 客户端(只做诊断):JSON-RPC 帧、握手、按语言路由进程、`publishDiagnostics` 缓存 |
| `diagnostics` | `lsp` | 缓存 → 简洁的 `file:line:col  code  message` 摘要;工作区源码收集 |
| `repomap` | `repomap` | tree-sitter 符号地图(Rust + 6 门语言)与 Maven 感知的模块/包骨架 |
| `ripgrep` | — | `rg --json` 内容搜索封装 |
| `walk` | — | 用户路径校验、共享 `rg` glob 排除 |

设计要点:

- **注入而非硬编码。** `LspManager` 在构造时接收「路径 → server」路由
  (`impl Fn(&Path) -> Option<LspServerSpec>`)与 `ClientInfo` 客户端身份——
  产品侧通常包一层 `lang::lsp_spec_for_path`。
- **全同步。** 不依赖 async 运行时:内部 std 线程 + 通道,外部普通阻塞调用。
  拥有运行时的调用方可用 `spawn_blocking` 包裹。
- **优雅降级。** server 起不来或握手失败只记进 `health()`,绝不 panic,
  快照返回空。
- **零框架依赖。** 没有 Tool trait、没有 agent 运行时——任何 LLM agent
  壳都能挂。

## 用法

```toml
[dependencies]
code-intel = "0.1"                       # 默认:lsp + repomap
# 只要诊断、不背 7 个 tree-sitter grammar:
code-intel = { version = "0.1", default-features = false, features = ["lsp"] }
```

```rust
use std::path::Path;
use code_intel::lsp::{ClientInfo, LspManager, LspServerSpec};
use code_intel::lang;

// 按内置注册表路由,以自家产品身份握手。
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
// ... 等待 health(),然后:
for (file, diags) in client.snapshot() { /* ... */ }
```

仓库地图、搜索、诊断格式化无需任何装配:

```rust
let map = code_intel::repomap::build_repo_map(&root, Some("src"))?;
let hits = code_intel::ripgrep::search(&root, "fn main", None, 20)?;
let files = code_intel::diagnostics::collect_code_files(&root, None)?;
```

外部依赖:`repomap`/`ripgrep` 需要 `rg`(ripgrep);`lsp` 的实时诊断需要
对应语言 server(如 `rust-analyzer`)。

跑实时演示:`cargo run --example diagnostics-demo`。

## 许可证

MIT — 见 [LICENSE](LICENSE)。
