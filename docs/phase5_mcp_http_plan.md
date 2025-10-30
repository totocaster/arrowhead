# Arrowhead MCP Phase 5 Plan — HTTP Transport

## Context & Objectives

Phase 5 delivers the remote Model Context Protocol transport so Arrowhead can
serve agents over HTTP in addition to the existing stdio path. The goal is a
production-ready `arrowhead --mcp-server` mode that exposes the same tool
surface, enforces bearer authentication, and slots cleanly into the existing
runtime/shared handler architecture without regressing daemon-backed behaviour.
Tokens are displayed only once; we persist hashed digests so operators must
record the raw value when it is generated.

## Resources
- Specification: `docs/predev_synapse_rust_rewrite.md` (Phase 5 scope, security)
- MCP protocol reference: `README.md#mcp-protocol-details`
- Phase 4 groundwork: `docs/phase4_mcp_stdio_plan.md`
- HTTP stack: `axum`/`tower`/`tower-http` crates already listed in workspace
- Security notes: `docs/dev_status.md` and spec security section

## Success Criteria
- `arrowhead --mcp-server` boots an Axum server that accepts JSON-RPC 2.0 POSTs
  at `/rpc`, reuses `HandlerRegistry` for request execution, and honours the
  daemon/embedding availability semantics already implemented.
- Bearer-token mode (default) and link-token mode both authenticate requests; the
  latter allows MCP clients configured as “No Auth” to target path-scoped URLs
  such as `/rpc/<token>` and still receive protected access, while failures
  return `401`/`403`.
- Health probe (`GET /health`) reports runtime readiness; metrics endpoints are
  deferred to a later phase.
- Server shuts down gracefully on SIGINT/SIGTERM, drains in-flight work, and
  persists structured logs alongside the CLI/stdio modes.
- CLI help + docs describe configuration, token management, and recommended
  deployment posture; tests cover happy/error paths for transport, auth, and
  handler wiring.

## Functional Scope

**Transport**
- Build HTTP server in `crates/arrowhead-mcp/src/http.rs` using Axum routers and
  Tower layers for tracing, backpressure, and graceful shutdown.
- Support JSON-RPC single objects and batch arrays in POST `/rpc`, mapping to
  `Incoming`/`Message` types reused from Phase 4.
- Enforce bounded concurrency (Semaphore or Tower limit layer) mirroring stdio
  defaults with configurable overrides.

**Request Lifecycle**
- Parse request body into `Incoming`, fan out to shared handler instances,
  serialize responses, and surface JSON-RPC errors when handlers fail.
- Return HTTP status codes aligned with JSON-RPC semantics (`200` for protocol
  responses, `400` for malformed JSON, `429` when queue is saturated, etc.).

**Authentication & Network Policy**
- Implement token validation in `crates/arrowhead-mcp/src/auth.rs`, supporting
  two modes:
  - **Bearer mode (default):** clients send `Authorization: Bearer <token>`.
  - **Link-token mode:** clients that cannot set headers (e.g., ChatGPT “No
    Auth”) connect to a tokenised path (e.g., `/rpc/<token>`); the server matches
    the segment and responds with 401 when absent/invalid.
- Persist only hashed token digests, compare in constant time, and allow
  multiple active tokens for rotation.
- Provide `arrowhead --mcp-server --generate-token` to emit new tokens and
  print usable URLs for link-token mode.
- Enforce IP allowlists via CIDR matching (introduce `ipnet` dependency if
  needed) with sensible defaults (localhost only unless overridden).

**CLI & Configuration**
- Extend `crates/arrowhead-cli/src/main.rs` to add `--mcp-server` (mutually
  exclusive with `--mcp`) plus supporting flags:
  - `--bind`, `--allow`, `--allow-file` for network policy.
  - `--auth-mode <bearer|link-token>`, defaulting to `bearer`.
  - `--token`, `--token-file`, `--token-hash` to seed bearer/link tokens.
  - `--generate-token` utility that prints a fresh token + example URLs then
    exits without starting the server.
- Persist HTTP server settings under a new `mcp` section in
  `crates/arrowhead-cli/src/config.rs`; store hashed tokens (e.g., SHA-256)
  and remember the selected auth mode, while allowing temporary override via
  CLI flags/env vars.
- `commands/mcp.rs` grows a `run_server` entry that initialises runtime,
  renders the correct client URL for link-token mode, installs scoped logging
  (dedicated `mcp-http.log`), and spawns the Axum server until shut down.

**Observability & Ops**
- Expose `/health` with daemon + embedding status snapshot.
- Emit structured tracing (request ID, method, duration) to log files while
  keeping HTTP responses protocol-clean.
- Document that metrics endpoints are deferred; capture follow-up in the Phase 6
  backlog.

## Non-Goals
- Native TLS/HTTPS termination (document reverse proxy expectation).
- WebSocket or SSE transports.
- Multi-tenant vault multiplexing or cross-vault routing.
- Deployment tooling (systemd units) beyond documentation pointers.
- OAuth 2.0 / OpenID Connect flows (document future exploration if needed).

## Architecture Notes
- Factor the `MessageHandler` trait out of `stdio.rs` into a shared module so
  both transports depend on the same abstraction without duplication.
- Construct the Axum router with layered middleware: IP filter (custom
  extractor), authentication, request body limiter, concurrency guard, tracing.
- Implement auth middleware that first enforces IP policy, then checks for a
  bearer header, falling back to link-token query/path extraction when in that
  mode; reuse the same verifier to avoid drift between transports.
- Use `tokio::signal::ctrl_c` plus `cfg(unix)` signal handlers to trigger
  graceful shutdown; propagate shutdown to runtime tasks via cancellation.
- Reuse `McpRuntime` initialisation so HTTP and stdio share caches, status
  refresh logic, and embedding availability checks.
- Ensure error types converge on `ProtocolError`; map to HTTP responses only at
  the transport boundary.

## Implementation Roadmap

### Stage 1 — Transport Foundations
1. Move `MessageHandler` into `transport.rs` (new module) and adjust stdio to
   consume it unchanged.
2. Scaffold `HttpServer` struct with configuration (bind address, limits),
   spawn Axum router, and implement `/rpc` endpoint that accepts POSTed JSON.
3. Add unit coverage for JSON parsing, error mapping, and concurrency limiter
   behaviour using tower test harnesses.

### Stage 2 — Authentication & Policy
1. Implement token store loader in `auth.rs` supporting CLI-provided token,
   token file, environment override (`ARROWHEAD_MCP_TOKEN`), and config
   persistence using hashed digests only (raw tokens never touch disk and are
   discarded after generation).
2. Introduce IP allowlist manager (CIDR parsing, default `127.0.0.0/8` &
   `::1`) with tests for IPv4/IPv6/mixed cases.
3. Wire authentication + IP filters into Axum middleware, returning JSON-RPC
   error payloads with appropriate HTTP status codes.

### Stage 3 — CLI & Runtime Integration
1. Extend Clap definitions for new flags, update `CommandContext` persistence,
   and surface helpful validation (e.g., refuse to start without any token).
2. Implement `run_server` in `commands/mcp.rs` that spins up `McpRuntime`,
   loads auth configuration, surfaces the effective client URL (with a warning
   when link-token mode is active), and serves until shutdown.
3. Ensure logging writes to `.arrowhead/logs/mcp-http.log`, reusing pruning
   helpers and verbosity levels.
4. Update workspace configuration defaults and sample config templates.

### Stage 4 — Validation, Docs, & Tooling
1. Add integration tests under `tests/integration/mcp_http.rs` that start the
   server on an ephemeral port, exercise both bearer and link-token modes
   (success & failure), and confirm `/health` behaviour when the daemon is
   offline.
2. Expand unit tests in `auth.rs` and `http.rs` to cover edge cases (batch
   requests, malformed JSON, token rotation, rate limiting).
3. Refresh documentation: CLI help text, `README.md#cli-reference`, `README.md#mcp-protocol-details`,
   `docs/dev_status.md`, and add examples for reverse proxy deployment.
4. Wire new tests into CI and ensure `cargo fmt`, `cargo clippy`, and
   workspace checks remain clean.

## Configuration & Deployment
- CLI flags control runtime defaults; values persist into `AppConfig` under
  `[[mcp.tokens]]`, `bind_address`, `allowed_ips`, etc.
- Support env overrides (`ARROWHEAD_MCP_BIND`, `ARROWHEAD_MCP_TOKEN`) to ease
  secret injection.
- Provide helper command `arrowhead --mcp-server --generate-token` that prints a
  hex token once (with bearer header and `/rpc/<token>` examples) without
  starting the server. The command reminds operators to store the secret
  immediately because it will not be shown again and only a hash is persisted.
- Document how to rotate tokens: update config/env, reload server (or support
  SIGHUP reload if feasible).
- Ensure IP allowlists are fully configurable (support CIDR lists loaded from
  flags/config files) so deployments exposed via internet or Tailscale can
  widen access deliberately.

## Security Considerations
- Hash tokens with SHA-256 before writing to disk; compare via constant-time
  equality to minimise leak surface. Document the behaviour so operators know
  to store raw tokens elsewhere.
- Never log bearer tokens; scrub headers before tracing.
- Warn that link-token mode exposes the token in URLs; strongly recommend HTTPS
  + reverse proxy and note that browsers/history may retain the token.
- Ensure IP filter executes before handler dispatch to avoid unnecessary load.
- Provide guidance for production hardening (reverse proxy TLS, firewall).

## Testing Strategy
- **Unit:** auth token parsing/rotation, IP filter edge cases, JSON-RPC decoding
  helpers, HTTP error mapping, health handler.
- **Integration:** spin up server against fixture vault, call RPC endpoints with
  valid/invalid tokens, simulate backpressure, verify daemon-offline responses,
  and confirm shutdown handling.
- **Load/Smoke:** optional Tokio test that fires concurrent requests to assess
  latency/backpressure metrics.

## Documentation & Developer Experience
- Update CLI reference (`README.md#cli-reference`) with new flags, environment variables,
  and usage examples (curl + Claude Desktop over HTTP).
- Expand `README.md#mcp-protocol-details` transport section to include HTTP wire format,
  auth headers, error examples, and operational notes.
- Add a deployment checklist (firewall, reverse proxy, logs) either here or in
  `docs/feature_development_guide.md`.
- Note Phase 5 progress in `docs/dev_status.md`.

## Risks & Mitigation
- **Blocking handlers:** HTTP transport must mirror stdio’s use of
  `spawn_blocking` for disk-bound work; audit handlers during integration.
- **Token leakage:** Strictly avoid logging raw headers; add regression tests to
  ensure tokens never appear in error messages.
- **Resource exhaustion:** Tune concurrency + request body limits; expose
  config knobs and document recommended values for larger deployments.
- **Daemon availability:** HTTP callers need actionable errors when the daemon
  is offline; reuse existing messaging and note it in docs.
