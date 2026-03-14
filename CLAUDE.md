# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build --verbose       # Build the project
cargo build --release       # Release build
cargo test --verbose        # Run all tests
cargo test <test_name>      # Run a single test
cargo run -- <subcommand>   # Run the h5i CLI
```

CI runs `cargo build --verbose` then `cargo test --verbose` with Git user config pre-set (needed because tests perform Git operations).

## Architecture

**h5i** ("high-five") is a Git sidecar that extends version control with five semantic dimensions: temporal (Git history), structural (AST), intentional (AI provenance), empirical (test metrics), and associative (CRDT collaboration). It stores its data in `.git/.h5i/` with subdirectories `ast/`, `metadata/`, `crdt/`, and `delta/`.

### Module Overview

- **`repository.rs`** (67KB) — Core hub. `H5iRepository` wraps a `git2::Repository` and orchestrates all five dimensions. Key operations: `init`, `commit`, `log`, `blame`, `resolve`. Commit flow optionally captures AI metadata, AST snapshots, test metrics, and runs integrity audits.
- **`session.rs`** — `LocalSession` manages per-file Yrs (Y-CRDT) documents for collaborative editing. Writes append-only binary updates to `delta_store`. Enables concurrent agent edits with strong eventual consistency.
- **`delta_store.rs`** — Append-only binary log for CRDT updates. Files are keyed by `sha256(file_path)`. Format: `[length: u32][update bytes]`. Supports snapshots and archival on commit.
- **`metadata.rs`** — Data types: `H5iCommitRecord`, `AiMetadata` (model, agent ID, prompt, token count), `TestMetrics`, `IntegrityReport` (severity: Valid/Warning/Violation). Serialized as JSON in Git Notes.
- **`ast.rs`** — `SemanticAst` (S-expression based), `AstDiff` (additions/deletions/moves/unchanged), similarity scoring (0.0–1.0), SHA-256 structure hashing. Python files are parsed via `script/h5i-py-parser.py`.
- **`blame.rs`** — Two modes: `Line` (traditional) and `Ast` (semantic). Associates authorship with AI metadata and test results per commit.
- **`watcher.rs`** — Uses `notify` crate. Detects file changes and syncs to CRDT session.
- **`error.rs`** — Error categories mirror the five dimensions (Git/temporal, AST/structural, metadata/intentional, quality/empirical, CRDT/associative).
- **`main.rs`** — CLI via `clap`. Subcommands: `init`, `session`, `commit`, `log`, `blame`, `resolve`.

### Key CLI Subcommands

```
h5i init
h5i session --file <path>
h5i commit --message <msg> [--prompt <text>] [--model <name>] [--agent <id>] [--tests] [--ast] [--audit] [--force]
h5i log [--limit N]
h5i blame <file> [--mode line|ast]
h5i resolve <ours> <theirs> <file>
```

### Key Dependencies

- **git2** — Git operations
- **yrs** — Y-CRDT implementation for collaborative sessions
- **tokio** — Async runtime
- **tiktoken-rs** — Token counting for AI metadata
- **notify** — File system watching
- **clap** — CLI parsing
