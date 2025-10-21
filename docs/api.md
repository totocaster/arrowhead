# Arrowhead Public API

This document captures the high-level API surface that Arrowhead exposes to
other applications and tooling. The implementation is currently being
developed; this file serves as the agreed plan of record.

## CLI Commands

- `arrowhead init` — bootstrap a vault and configuration.
- `arrowhead index` — run indexing jobs over the vault.
- `arrowhead search` — execute FTS/semantic/hybrid searches.
- `arrowhead notes` — CRUD operations for working with notes.
- `arrowhead graph` — inspect WikiLink graph relationships.
- `arrowhead vault` — utility commands such as stats and integrity checks.

Each command is documented in greater detail within the command-specific Rust
modules under `crates/arrowhead-cli/src/commands/`.

## Library Crates

`arrowhead-core` exposes the programmatic surface for working with the vault,
indexer, graph, and search subsystems. Key entry points include:

- `Vault` and `VaultConfig` for resolving vault paths and filesystem concerns.
- `Indexer` for orchestrating indexing passes.
- `MetadataExtractor`, `EmbeddingGenerator`, and `SearchService` for their
  respective pipeline stages.
- `GraphService` for graph queries and WikiLink analytics.

All modules currently return `anyhow::Result<T>` and use Arrowhead-specific
types defined in `types.rs`.

## MCP Integration

`arrowhead-mcp` will provide both stdio and HTTP transports that conform to the
Model Context Protocol (MCP). The command-line interface exposes
`--mcp`/`--mcp-server` flags that will eventually initialise these transports.

Further transport and schema details live in `docs/mcp_protocol.md`.
