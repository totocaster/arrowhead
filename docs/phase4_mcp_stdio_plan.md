# Arrowhead MCP Phase 4 Plan — Stdio Transport

## Context & Objectives

Phase 4 of the Synapse → Arrowhead rewrite introduces a first-class Model
Context Protocol (MCP) server that runs inside the Arrowhead toolchain. The
deliverable is a production-ready stdio transport (`arrowhead --mcp`) that
accepts JSON-RPC 2.0 requests over stdin, emits responses/events on stdout, and
invokes the same core services that back the CLI (vault, search, graph, notes).

The plan below is the brief for implementation agents. Follow it sequentially,
checking in artefacts at each milestone. All work must stay compatible with the
pinned Rust 1.86 toolchain and respect the repository’s formatting/testing
rules (`cargo fmt`, `cargo check`, `cargo test`).

## Resources
- MCP Protocol: https://modelcontextprotocol.io/docs/getting-started/intro
- Synapse MCP instructions (good starting point especially for MCP usage instruction): /Users/toto/Developer/Synapse-Obsidian

## Success Criteria

- `arrowhead --mcp` starts a long-running process that speaks MCP over stdio.
- The transport validates JSON-RPC envelopes, correlates requests/responses,
  and surfaces well-typed errors.
- Core tools are exposed: live status streaming, note content fetch, FTS search,
  semantic search (when enabled), graph context/backlinks/forward links,
  and metadata lookup.
- The server shares index + graph state with the existing daemon-driven
  pipeline (no secondary index).
- Extensive logging, metrics, and integration tests cover the stdio path.
- Documentation in `docs/` explains usage, configuration, and extension hooks.

## Functional Scope

1. **Transport**
   - Single-threaded stdio reader/writer loop with graceful shutdown.
   - Bounded channel to hand off decoded requests to worker tasks.
   - Structured tracing around request lifecycle (trace IDs).

2. **Protocol Engine**
   - JSON-RPC 2.0 request parsing, batching support, notifications.
   - Strong typing for method names, params, and results.
   - Unified error type mapping into standard JSON-RPC errors plus
     MCP-specific codes (InvalidParams, InternalError, ToolDisabled, etc.).

3. **Tool Surface**
   - `mcp.graph.get_context` (combined forward/back/unresolved).
   - `mcp.graph.get_backlinks` / `mcp.graph.get_forward_links`.
   - `mcp.search.fts`, `mcp.search.semantic`, `mcp.search.hybrid`.
   - `mcp.vault.status` (align with `arrowhead status --json`).
   - `mcp.notes.read`, `mcp.notes.list`, `mcp.notes.metadata`.
   - `mcp.search.semantic` returns `ToolDisabled` only when embeddings failed to initialise.
   - Request validation (note IDs, pagination, vault path) with actionable
     error messages.

4. **Runtime Integration**
   - `arrowhead-cli` gains a `--mcp` mode that bootstraps config, connects to
     the shared SQLite database (with sqlite-vec vectors), and runs the stdio server until EOF.
   - When the daemon is offline, MCP methods must emit `ServiceUnavailable`
     errors consistent with CLI behaviour.
   - Shared status caches & config loading reused from CLI modules.

## Non-Goals (Phase 4)

- HTTP transport (`--mcp-server`) — deferred to Phase 5.
- Tool multiplexing across multiple vaults in a single process.
- Authentication or ACLs beyond implicit local access.
- Windows support (explicit follow-up item).

## Architectural Notes

- **Crate Layout:** Add `crate/arrowhead-mcp` modules for `transport`, `rpc`,
  `methods`, `error`, `serde`. The CLI entry point calls into this crate.
- **State Management:** Reuse `AppContext` style pattern providing handles to
  `Vault`, `IndexDatabase`, `GraphService`, `SearchService`, etc. Inject
  dependencies during server start.
- **Concurrency:** Tokio runtime handles request workers (`spawn` per request or
  worker pool). Avoid blocking operations on async tasks — use
  `spawn_blocking` for SQLite/FS operations.
- **Backpressure:** Cap outstanding requests (e.g., semaphore) to prevent
  runaway work when clients flood the server.
- **Tracing & Logging:** Use structured tracing (`tracing` crate). Include
  request ID, method, duration, error code in logs. Ensure logs respect the
  `ARROWHEAD_ENABLE_FILE_LOGS` toggle (stdout should remain protocol-only).
- **External Tool Instructions:** Extracted instructional text for major tool usage into a markdown document somewhere in the repository, which will be consumed at build time for tool descriptions.

## Implementation Roadmap

### Stage 1 — Foundations

1. Define JSON-RPC + MCP data structures:
   - Request, response, notification enums.
   - Error codes mapping to MCP guidance.
   - Serde helpers for parameter decoding.
2. Implement a minimal stdio server harness:
   - Read lines/frames from stdin (newline-delimited JSON).
   - Parse into `IncomingMessage`, forward to handler.
   - Write responses to stdout with `\n` delimiter.
   - Graceful shutdown on EOF or fatal parse error.
3. Add basic integration test (fixture-driven) verifying echo-style method.

### Stage 2 — Method Wiring

1. Create handler traits/structs per tool group (`GraphMethods`,
   `SearchMethods`, `VaultMethods`, `NoteMethods`).
2. Map MCP method names to handler functions.
3. Implement `get_context`, `get_backlinks`, `get_forward_links` using
   `GraphService`. Ensure outputs match CLI JSON schema.
4. Wire `search` methods, returning ToolDisabled when embeddings are unavailable. Respect query
   result limits; include score + snippet when available.
5. Implement `vault.status` by delegating to existing status retrieval helpers.
6. Add note read/list/metadata methods reusing CLI logic.
7. Flesh out validation and service unavailability flows.

### Stage 3 — Robustness & Tooling

1. Add batch request handling (vector of JSON-RPC messages).
2. Support notifications for future events (no response required).
3. Introduce request timeout & cancellation semantics (client-supplied IDs).
4. Implement metrics hooks (counts by method, successes/failures, latency).
5. Harden error mapping, including nested cause logging.
6. Expand integration tests:
   - Happy-path coverage for each method.
   - Error cases (invalid params, note not found, daemon offline).
   - Embedding permutations (semantic enabled vs. `--fts-only`).

### Stage 4 — CLI & Documentation

1. Extend `clap` configuration in `crates/arrowhead-cli` with `--mcp`.
2. Bootstrapping: load config, ensure vault is configured, verify daemon health,
   instantiate MCP server.
3. Update CLI help text, `README.md#cli-reference`, and `docs/mcp_protocol.md` with method
   catalog and example payloads.
4. Provide usage examples (`docs/examples/` if necessary) showing how to launch
   the stdio server and interact via `stdio` pipes.
5. Update `docs/dev_status.md` upon completion.

## Testing Strategy

- **Unit Tests:** JSON encoding/decoding, error mapping, method validation.
- **Integration Tests:** Use fixture vault under `tests/integration/` with
  scripted stdin/stdout interactions. Add tests for daemon-unavailable scenarios
  and for semantic search behind feature flags.
- **Load Testing:** Optional stress harness (Tokio task) sending concurrent
  requests to ensure throughput/backpressure behave.
- **Clippy/Fmt/Test:** Enforce across workspace; add new tests to CI gating.

## Documentation & Developer Experience

- Create `docs/mcp_protocol.md` appendices detailing method schemas, request/
  response examples, and error codes once implementation stabilises.
- Provide README section for `arrowhead-mcp` crate explaining layering.
- Ensure `feature_development_guide.md` references the MCP addition workflow.

## Risk & Mitigation

- **Protocol Drift:** Align with MCP spec revisions; write unit tests mirroring
  canonical examples from spec.
- **Blocking Operations:** Audit all handlers for blocking calls; wrap in
  `spawn_blocking` or redesign to async.
- **Crash Visibility:** Keep stdout strictly protocol-compliant; direct logs to
  `stderr` or file (per existing logging strategy).
- **Daemon Dependency:** Clearly surface when the daemon is required and how the
  agent should retry or fail fast.

## Deliverables Checklist

- [x] `arrowhead-mcp` crate exposing stdio server + handlers.
- [x] `arrowhead-cli --mcp` entry point.
- [x] Method implementations covering graph/search/vault/notes.
- [x] Unit + integration tests for the new surface.
- [x] Documentation updates (`README.md#cli-reference`, `docs/mcp_protocol.md`, usage guide).
- [ ] CI updated if extra tooling/tests introduced.

Follow this plan sequentially, opening follow-up issues for out-of-scope items
such as HTTP transport, multi-vault multiplexing, or Windows compatibility.
