# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `verify`: compiler output parsing — `error_count` / `warning_count` plus a
  truncated summary, with the parser chosen by the command (`rustc` / `tsc` /
  `javac` / `gcc`). **Advisory only**: the verdict for a verify run is the
  process exit code, never the parsed counts.
- `lang`: `LanguageSpec::verify_command` — the per-language default — and
  `verify_command_for`, the single-file lookup used for hints.
- `lang`: `resolve_verify_command` — the workspace's compile-level
  verification command, resolved with **zero configuration**: `.phimint.toml`
  → a slot the project already fills in (Cargo `[workspace]`, Gradle wrapper,
  `package.json` scripts, Makefile `check` target) → the language table. The
  answer carries the source it came from, so a reader can tell "it ran your
  build" from "it ran a guess". No human is asked to configure anything and
  no model is consulted — a command the model could pick is a green light it
  could mint. The table is anchored to a build manifest **in the workspace
  root**: a command derived from the source files present cannot be run at a
  root that offers no entry point to run it against.

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
