# Arrowhead

**Obsidian vault indexing and search with MCP integration**

Arrowhead is a cross-platform CLI tool that provides powerful indexing and search capabilities for Obsidian vaults, with AI integration via the Model Context Protocol (MCP).

## Name Origin

Arrowhead references the precision obsidian tools used by prehistoric humans for hunting and crafting—sharp, targeted instruments that point the way, just as this tool precisely finds and connects knowledge within your Obsidian vault.

## Features

- **Smart Indexing**: On-demand indexing with staleness detection (only reindex changed notes)
- **Full-Text Search**: SQLite FTS5-based keyword search with field:value syntax
- **Semantic Search**: Vector embeddings for conceptual similarity search
- **Hybrid Search**: Combined FTS + semantic search for best results
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

# Install the CLI (optional)
make install PREFIX=$HOME/.local

# Run CLI
arrowhead --help

# Run tests
cargo test
```

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Contributing

This project is in early development. 

## Acknowledgments

Arrowhead is a Rust rewrite of [Synapse-Obsidian](https://github.com/yourusername/Synapse-Obsidian), originally written in Swift for macOS.
