# Arrowhead Development Status — 2025-10-21 (updated)

## Snapshot

- **Toolchain:** Rust 1.86 (2024 edition) pinned via `rust-toolchain.toml`.
- **Workspace health:** `cargo fmt`, `cargo check`, and `cargo test` all pass.
- **Crates:** Core/CLI/MCP crates ship concrete implementations for Phase 1 (vault, metadata, SQLite, indexer) with tests.
- **Indexer:** Staleness detection compares filesystem mtimes with stored `indexed_at`, and a new bounded write queue funnels all SQLite (including sqlite-vec) persistence through a single writer so unchanged notes skip cheaply while updated notes commit without exhausting the connection pool.
- **Vault settings:** `.obsidian/app.json` is parsed for attachments and user ignore filters so templates stay out of the index.
- **CLI:** `init` now performs the full interactive vault setup (prompting for auto-start, launching the deamon unless `--no-start` is passed) and returns immediately once `arrowheadd` is alive—the initial crawl continues in the background and live progress streams through `arrowhead status` (TTY view or NDJSON frames). A new `--fts-only` switch lets users opt out of semantic indexing up front. `index` remains informational, and full `notes` CRUD (read/list/create/update/delete) execute end-to-end; logging writes to `.arrowhead/logs/cli.log` with multi-day retention. Vault subcommands (`vault init/start/stop/cleanup`) manage the background deamon, cache socket/status metadata in config, render runtime health, and provide teardown. Search commands rely on the deamon-maintained index instead of running local refresh passes. `arrowhead --mcp` now bootstraps the MCP stdio server, exposing the full Phase‑4 tool surface (graph analytics, search, notes CRUD, discovery helpers, protocol initialise/tools) with bounded backpressure, structured tracing, and shared runtime wiring. `arrowhead --mcp-server` serves the same surface over HTTP with bearer/link-token authentication, CIDR allowlists, `/health` readiness probes, concurrency limits, and one-shot token generation.
- **Auto-start:** `vault init` now offers to register per-user auto-start units (launchd on macOS, `systemd --user` on Linux), persists manifest metadata under `.arrowhead/deamon/autostart/`, surfaces enablement in the status stream, and ensures `vault cleanup` removes the units.
- **Search:** `arrowhead search fts` executes against SQLite FTS5 with `field:value` and boolean syntax, stemming (`porter`) enabled, richer relevance scores, and cleaner snippets while relying on the deamon-maintained index (no inline refresh).
- **Deamon runtime:** New `arrowhead-deamon` crate (Tokio binary `arrowheadd` + library) exposes `status`/`shutdown` JSON socket commands, persists PID/status/log files under `.arrowhead/deamon`, and streams filesystem events via `notify` to `Indexer::reindex_paths`. Poll-based watcher integration tests verify path reindex + status updates. When semantic indexing is enabled the runtime now initialises the fastembed + sqlite-vec pipeline itself, streaming Hugging Face download progress into `DeamonStatus.downloads`, raising issues on failure, and falling back to FTS-only indexing if the model cannot be prepared. Logging now captures lifecycle, watcher batch, and per-note outcomes in `.arrowhead/logs/daemon.log` so debugging no longer depends on the CLI process.
- **CI:** GitHub Actions workflow (`CI`) runs on push/PR/workflow_dispatch, enforcing fmt, clippy, check, and test across the workspace.
- **Documentation:** Specification aligned (`docs/predev_synapse_rust_rewrite.md`) and updated to include the deamon crate/runtime responsibilities; feature development guide established; integration fixtures ready.
- **Vectors:** Semantic pipeline (fastembed + sqlite-vec) integrates with indexing, the daemon runtime, and CLI search by default; use `--fts-only` to disable embeddings when required. File logging stays opt-in (`ARROWHEAD_ENABLE_FILE_LOGS=1`) while we monitor sqlite-vec logging overhead.
- **Graph:** WikiLink extraction now persists raw/display/heading metadata plus a resolution reason; `arrowhead graph` defaults to a combined context view showing inbound/outbound edges and supports `--json` for structured responses alongside backlinks, forward links, orphan, and unresolved reports.

## Completed Work (Phase 0-1)

- Workspace upgrade to modern Rust/edition with reproducible toolchain.
- Dependency audit and alignment to current stable releases.
- Replacement of `todo!()` placeholders with structured scaffolding across core modules.
- CLI architecture (commands module tree, config loader, tracing bootstrap).
- Documentation refresh and repository structure parity with the specification.
- Phase 1 foundations: vault metadata extraction, SQLite schema + migrations, indexing orchestration with mtime skips, CLI command wiring, logging, and regression tests.
- Automatic schema-version detection for the SQLite index: incompatible databases are discarded and rebuilt to keep migrations unnecessary.
- Indexer progress instrumentation landed (batching hooks, observer events, CLI progress bar) so long-running reindexes surface user feedback out of the box.
- FTS search pipeline (Synapse-style query rewriting, porter tokenization, metadata/value dual-token indexing, revamped ranking/snippets) with comprehensive unit and integration coverage, including automatic index refresh when running searches.
- Real-time status streaming: the deamon emits `StatusFrame` updates over the control socket, `arrowhead status` tails frames (TTY or NDJSON), and the CLI falls back to cached snapshots when the runtime is offline.
- Semantic + hybrid search: embedding presets (`fast`/`good`/`better`), automatic Hugging Face downloads scoped to the vault, sqlite-vec persistence/refresh from the indexer, and CLI entry points for `search semantic` / `search hybrid` with cosine + weighted scoring.
- Graph foundation: link extraction now records target/display/heading data with resolution reasons; indexer updates `note_links` during daemon runs and the CLI `graph` command now defaults to the full context view (with optional `--json` output that summarises forward/backlink counts) while still exposing dedicated backlinks, forward links, orphan, and unresolved subcommands.

## Next Focus Areas

1. **Graph Enhancements (Phase 3 focus)**
   - Add sync guarantees between note edits and graph edges, including queue depth/back-pressure metrics for troubleshooting.
   - Profile large vaults to tune channel sizing and surface alerts when the writer falls behind.

2. **Search Hardening (Phase 2 follow-up)**
   - Tune hybrid weighting/thresholds against real vault corpora and add regression fixtures covering semantic-only and mixed queries.
   - Improve semantic previews (smarter snippet generation) and document evaluation tooling.
   - Extend integration tests to exercise sqlite-vec-backed searches.

3. **Model Management & UX**
   - Finalise licensing guidance for the shipped presets and surface model selection in docs/CLI help.
   - Allow opt-in cache directory overrides and consider richer CLI presentation for deamon-reported download progress.

4. **MCP Observability & Hardening**
   - Expose metrics/structured traces for the HTTP transport, add regression coverage for bearer/link-token flows (including negative cases), and explore optional TLS proxy integration guidance.

## Open Decisions / Risks

- **Vector MSRV:** sqlite-vec currently aligns with the workspace toolchain; continue monitoring upstream requirements.
- **Toolchain alignment:** `rust-toolchain.toml` pins 1.86.0 but `rust-version` metadata and CI still target 1.85; update the remaining references to avoid drift.
- **Model distribution:** Implement Hugging Face-backed downloads with clear licensing documentation and opt-in presets.
- **Schema migrations:** Continue relying on drop-and-reindex for incompatible schemas; document any future scenarios that require persistent migrations.
- **Concurrent access:** Clarify whether multi-process vault access needs to be supported in v1.
- **Vector tooling:** sqlite-vec auto-extends SQLite at runtime; ensure CI agents ship with compatible libc/SQLite builds.
- **Indexing diagnostics:** Instrument writer queue depth / latency so operators can spot back-pressure and correlate with per-note failures.

## Tracking

- Specification: `docs/predev_synapse_rust_rewrite.md`
- API notes: `docs/api.md`
- MCP notes: `docs/mcp_protocol.md`
- Integration test harness: `tests/integration/`
