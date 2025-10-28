# Arrowhead Public API

This document captures the high-level API surface that Arrowhead currently
exposes to other applications and tooling. Both the CLI and MCP transports
(stdio and HTTP) ship in the repository today.

## CLI Commands

- `arrowhead init` — bootstrap a vault and configuration.
- `arrowhead search` — execute FTS/semantic/hybrid searches.
- `arrowhead notes` — CRUD operations for working with notes.
- `arrowhead --mcp` — launch the stdio-based MCP server that exposes the same services to AI agents.
- `arrowhead --mcp-server` — start the HTTP MCP transport (Axum-based JSON-RPC 2.0) with bearer/link-token authentication, IP allowlists, and health reporting. Key flags:
  - `--bind <ADDR>` to override the bind address (defaults to `127.0.0.1:3911`).
  - `--auth-mode <bearer|link-token>` to choose authentication strategy.
  - `--token`, `--token-file`, `--token-hash` to provide raw or hashed credentials at runtime.
  - `--allow`/`--allow-file` to append CIDR ranges to the default localhost allowlist.
  - `--generate-token` to mint a new random token, persist its digest to the config, and print usable examples before exiting.
  - Environment overrides: `ARROWHEAD_MCP_BIND` (bind address) and `ARROWHEAD_MCP_TOKEN` (additional raw tokens).
- `arrowhead graph` — inspect WikiLink graph relationships.
  - Default invocation (`arrowhead graph <NOTE_ID>`) returns a combined context view listing outbound links, backlinks, and unresolved edges in one response.
  - All graph subcommands accept `--json` to emit machine-readable payloads mirroring the CLI output.
- `arrowhead status` — stream live daemon status frames or fall back to the latest snapshot.
- `arrowhead vault` — manage daemon lifecycle (init/start/stop/cleanup/autostart).

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

`arrowhead-mcp` now ships production-ready stdio *and* HTTP transports that
conform to the Model Context Protocol (MCP). `arrowhead --mcp` launches a
long-running process that reads newline-delimited JSON-RPC 2.0 frames from
stdin and emits responses on stdout. `arrowhead --mcp-server` serves the same
tool surface over `POST /rpc`, enforces bearer or link-token authentication,
filters incoming requests via configurable CIDR allowlists, exposes `GET /health`
for readiness probes, and mirrors the stdio backpressure semantics (429 when
the concurrency guard is saturated). Both transports reuse the shared handler
registry and structured tracing, ensuring identical behaviour regardless of
client transport.

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

Semantic and hybrid search remain feature-gated by the optional embeddings pipeline; discovery handlers fall back to graph heuristics if embeddings are disabled at runtime.

Further transport and schema details live in `docs/mcp_protocol.md`.
