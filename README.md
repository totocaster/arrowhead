# Arrowhead

**Obsidian vault indexing and search with MCP integration**

Arrowhead is a cross-platform CLI tool that provides Obsidian-aware indexing, note management, and (soon) search capabilities for Obsidian vaults, with AI integration via the Model Context Protocol (MCP).

## Name Origin

Arrowhead references the precision obsidian tools used by prehistoric humans for hunting and crafting—sharp, targeted instruments that point the way, just as this tool precisely finds and connects knowledge within your Obsidian vault.

## Features

### Available today

- **Background daemon**: `arrowhead init` provisions `.arrowhead/` scaffolding, launches `arrowheadd`, and keeps the SQLite + LanceDB indexes hot via filesystem watching.
- **Auto-start integration**: `arrowhead vault autostart enable|disable|status` manages launchd (macOS) or systemd --user (Linux) units so the daemon comes up automatically on login.
- **Status telemetry**: `arrowhead vault status` surfaces the daemon’s live activity, download progress, note/error counts, and log locations (`.arrowhead/logs/cli.log`, `.arrowhead/logs/daemon.log`).
- **Smart indexing**: Incremental reindexing with staleness detection so only changed notes are processed.
- **Obsidian-aware**: Automatically honours `.obsidian/app.json` settings (ignored folders, attachments) when scanning the vault.
- **Notes CLI**: `arrowhead notes read/list/create/update/delete` manages Markdown notes directly from the terminal.
- **Full-text search**: SQLite FTS5-based keyword search with `field:value` syntax and porter stemming.
- **Semantic + hybrid search**: fastembed models (build with `--features vector-lancedb`) deliver semantic and combined scoring with per-result reasoning snippets.
- **Model management**: Daemon coordinates Hugging Face downloads with progress surfaced in `vault status`.

### Coming soon

- **WikiLinks Graph**: Navigate backlinks, forward links, and find orphaned notes
- **MCP Server**: Dual-mode MCP integration (stdio for local, HTTP for remote)
- **Cross-Platform**: Works on macOS and Linux

## Project Status

🚧 **Under active development** - This is a complete rewrite of Synapse from Swift to Rust.

Current phase: **Phase 1 — Core foundation (vault/indexer)**

See [docs/predev_synapse_rust_rewrite.md](docs/predev_synapse_rust_rewrite.md) for the complete specification.

## Architecture

```
arrowhead/
├── crates/
│   ├── arrowhead-core/     # Core library (vault, indexing, search, graph)
│   ├── arrowhead-mcp/      # MCP protocol implementation
│   └── arrowhead-cli/      # CLI application
├── docs/                   # Documentation
└── tests/                  # Integration tests and fixtures
```

## Technology Stack

- **Language**: Rust 1.85+ (2024 edition)
- **CLI**: clap 4.5+
- **Database**: SQLite (rusqlite) with FTS5
- **Vectors**: fastembed (ONNX embeddings)
- **HTTP**: axum 0.7+
- **Async**: tokio 1.40+

## Building

```bash
# Build all crates
cargo build

# Build release version
cargo build --release

# Install CLI + daemon (installs both `arrowhead` and `arrowheadd` with LanceDB support)
make install PREFIX=$HOME/.local LOCKED=0 FORCE=1

# Run CLI
arrowhead --help

# Run tests
cargo test
```

## Usage overview

```bash
# Initialise a vault (creates .arrowhead/, starts the daemon, optional semantic preset)
arrowhead init --vault /path/to/vault [--embeddings fast|good|better]

# Check daemon status (activity, note counts, download progress, log paths)
arrowhead vault status

# Manage auto-start registration
arrowhead vault autostart enable
arrowhead vault autostart status
arrowhead vault autostart disable

# Read live logs (CLI + daemon)
tail -f /path/to/vault/.arrowhead/logs/cli.log
tail -f /path/to/vault/.arrowhead/logs/daemon.log

# Stop the daemon or clean up all Arrowhead artefacts
arrowhead vault stop
arrowhead vault cleanup
```

## Known issues

- **Sparse daemon logging**: Some environments only emit the start banner in `daemon.log`. A follow-up task is tracking the active tracing subscriber so indexing/download entries are preserved.
- **Watcher visibility**: Incremental reindexing currently updates status snapshots but may not log per-note progress yet.
- **Indexing failures**: A handful of notes can still fail to index without detailed diagnostics; richer error logging is on the roadmap.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contributing

This project is in early development. 

## Acknowledgments

Arrowhead is a Rust rewrite of [Synapse-Obsidian](https://github.com/yourusername/Synapse-Obsidian), originally written in Swift for macOS.
