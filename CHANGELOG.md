# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-09-06

### Added

- `lang`: per-language registry (extensions, manifests, LSP server specs) with a
  bounded directory scan for language detection.
- `lsp` *(feature)*: hand-rolled multi-server LSP client for diagnostics —
  JSON-RPC framing, handshake, per-language process routing via an injected
  resolver, injected `ClientInfo`, and a `publishDiagnostics` cache. Fully
  synchronous (std threads + channels); spawns one process per server binary,
  shared across its languages.
- `diagnostics` *(feature `lsp`)*: `publishDiagnostics` cache → terse
  `file:line:col  code  message` summaries; workspace source-file collection.
- `repomap` *(feature)*: tree-sitter symbol extraction (hand-written precision
  extractor for Rust, generic extractor for Java/TS/JS/JSX/C/C++/Python) plus a
  Maven-aware module → package directory skeleton.
- `ripgrep`: `rg --json` wrapper for workspace content search.
- `walk`: shared user-path validation and `rg` glob excludes.
- `examples/diagnostics-demo`: live LSP diagnostics over an intentionally
  broken file (requires `rust-analyzer` on PATH).

[Unreleased]: https://github.com/hibuka-labs/code-intel/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/hibuka-labs/code-intel/releases/tag/v0.1.0
