# Arrowhead Public API

This document captures the high-level API surface that Arrowhead exposes to
other applications and tooling. The implementation is currently being
developed; this file serves as the agreed plan of record.

## CLI Commands

- `arrowhead init` — bootstrap a vault and configuration.
- `arrowhead search` — execute FTS/semantic/hybrid searches.
- `arrowhead notes` — CRUD operations for working with notes.
- `arrowhead --mcp` — launch the stdio-based MCP server that exposes the same services to AI agents.
- `arrowhead graph` — inspect WikiLink graph relationships.
  - Default invocation (`arrowhead graph <NOTE_ID>`) returns a combined context view listing outbound links, backlinks, and unresolved edges in one response.
  - All graph subcommands accept `--json` to emit machine-readable payloads mirroring the CLI output.
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

`arrowhead-mcp` now ships a production-ready stdio transport that conforms to
the Model Context Protocol (MCP). `arrowhead --mcp` launches a long-running
process that reads newline-delimited JSON-RPC 2.0 frames from stdin and emits
responses on stdout. The transport surfaces a bounded in-flight queue, request
metrics (exposed via `StdioServer::metrics()`), and structured tracing.

Implemented tool surface:

- Graph: `mcp.graph.get_context`, `mcp.graph.get_backlinks`,
  `mcp.graph.get_forward_links`, `mcp.graph.find_orphans`,
  `mcp.graph.find_unresolved`
- Search: `mcp.search.fts`, `mcp.search.semantic`, `mcp.search.hybrid`
- Notes: `mcp.notes.list`, `mcp.notes.read`, `mcp.notes.metadata`,
  `mcp.notes.create`, `mcp.notes.update`, `mcp.notes.delete`
- Discovery: `mcp.discovery.get_related_notes`,
  `mcp.discovery.get_vault_stats`, `mcp.discovery.get_vault_conventions`
- Vault: `mcp.vault.status`
- Protocol: `mcp.protocol.initialize`, `mcp.protocol.tools/list`

Semantic and hybrid search remain feature-gated by the optional
Embeddings load by default via sqlite-vec; discovery handlers fall back to graph heuristics if embeddings are disabled at runtime.
when vectors are unavailable.

Further transport and schema details live in `docs/mcp_protocol.md`.
