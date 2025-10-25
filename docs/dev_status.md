# Arrowhead Development Status — 2025-10-21 (updated)

## Snapshot

- **Toolchain:** Rust 1.86 (2024 edition) pinned via `rust-toolchain.toml`.
- **Workspace health:** `cargo fmt`, `cargo check`, and `cargo test` all pass.
- **Crates:** Core/CLI/MCP crates ship concrete implementations for Phase 1 (vault, metadata, SQLite, indexer) with tests.
- **Indexer:** Staleness detection compares filesystem mtimes with stored `indexed_at`, skipping unchanged notes automatically and respecting Obsidian ignore filters.
- **Vault settings:** `.obsidian/app.json` is parsed for attachments and user ignore filters so templates stay out of the index.
- **CLI:** `init` now performs the full interactive vault setup (prompting for auto-start, launching the deamon unless `--no-start` is passed) and returns immediately once `arrowheadd` is alive—the initial crawl continues in the background and progress is surfaced via `arrowhead vault status`. A new `--fts-only` switch lets users opt out of semantic indexing up front. `index` remains informational, and full `notes` CRUD (read/list/create/update/delete) execute end-to-end; logging writes to `.arrowhead/logs/cli.log` with multi-day retention. Vault subcommands (`vault init/start/status/stop/cleanup`) manage the background deamon, cache socket/status metadata in config, render runtime health (JSON or human-readable), and provide teardown. Search commands rely on the deamon status instead of running local indexing passes.
- **Auto-start:** `vault init` now offers to register per-user auto-start units (launchd on macOS, `systemd --user` on Linux), persists manifest metadata under `.arrowhead/deamon/autostart/`, surfaces enablement in `vault status`, and ensures `vault cleanup` removes the units.
- **Search:** `arrowhead search fts` executes against SQLite FTS5 with `field:value` and boolean syntax, stemming (`porter`) enabled, richer relevance scores, and cleaner snippets while relying on the deamon-maintained index (no inline refresh).
- **Deamon runtime:** New `arrowhead-deamon` crate (Tokio binary `arrowheadd` + library) exposes `status`/`shutdown` JSON socket commands, persists PID/status/log files under `.arrowhead/deamon`, and streams filesystem events via `notify` to `Indexer::reindex_paths`. Poll-based watcher integration tests verify path reindex + status updates. When semantic indexing is enabled the runtime now initialises the fastembed + LanceDB pipeline itself, streaming Hugging Face download progress into `DeamonStatus.downloads`, raising issues on failure, and falling back to FTS-only indexing if the model cannot be prepared. Logging now captures lifecycle, watcher batch, and per-note outcomes in `.arrowhead/logs/daemon.log` so debugging no longer depends on the CLI process.
- **CI:** GitHub Actions workflow (`CI`) runs on push/PR/workflow_dispatch, enforcing fmt, clippy, check, and test across the workspace.
- **Documentation:** Specification aligned (`docs/predev_synapse_rust_rewrite.md`) and updated to include the deamon crate/runtime responsibilities; feature development guide established; integration fixtures ready.
- **Vectors:** Semantic pipeline (fastembed + LanceDB) integrates with indexing, the deamon runtime, and CLI search when the optional `vector-lancedb` feature is enabled; defaults to FTS-only when the feature is off or `--fts-only` is chosen. Vector builds skip per-command file logging by default (set `ARROWHEAD_ENABLE_FILE_LOGS=1` to opt in) while we stabilise LanceDB tracing behaviour.
- **Graph:** WikiLink extraction now persists raw/display/heading metadata plus a resolution reason; `arrowhead graph` surfaces backlinks, forward links, orphans, unresolved edges, and combined context views with human-readable explanations.

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
- Semantic + hybrid search: embedding presets (`fast`/`good`/`better`), automatic Hugging Face downloads scoped to the vault, LanceDB persistence/refresh from the indexer, and CLI entry points for `search semantic` / `search hybrid` with cosine + weighted scoring.
- Graph foundation: link extraction now records target/display/heading data with resolution reasons; indexer updates `note_links` during daemon runs and the CLI `graph` subcommands render backlinks, forward links, context, orphans, and unresolved edges.

## Next Focus Areas

1. **Search Hardening (Phase 2 follow-up)**
   - Tune hybrid weighting/thresholds against real vault corpora and add regression fixtures covering semantic-only and mixed queries.
   - Improve semantic previews (smarter snippet generation) and document evaluation tooling.
   - Extend integration tests to exercise LanceDB-backed searches (requires `protoc` in CI agents).

2. **Model Management & UX**
   - Finalise licensing guidance for the shipped presets and surface model selection in docs/CLI help.
   - Allow opt-in cache directory overrides and consider richer CLI presentation for deamon-reported download progress.

3. **Graph Enhancements**
   - Layer graph metrics (degree counts, orphan summaries) into CLI/MCP responses.
   - Document link reason taxonomy for MCP consumers and explore caching strategies for large vaults.

4. **MCP Surface (Phase 4-5)**
   - Finalise JSON-RPC types and tool schemas.
   - Implement stdio transport, then HTTP transport with bearer auth.

## Open Decisions / Risks

- **Vector MSRV:** LanceDB currently requires Rust ≥1.86; workspace bumped accordingly.
- **Model distribution:** Implement Hugging Face-backed downloads with clear licensing documentation and opt-in presets.
- **Schema migrations:** Continue relying on drop-and-reindex for incompatible schemas; document any future scenarios that require persistent migrations.
- **Concurrent access:** Clarify whether multi-process vault access needs to be supported in v1.
- **Protobuf tooling:** `vector-lancedb` builds need `protoc` on developer and CI machines; decide whether to vendor binaries or document the prerequisite.
- **Indexing diagnostics:** Some vaults still report failed notes without actionable errors—improve logging around extraction failures and include note identifiers in the status output.

## Tracking

- Specification: `docs/predev_synapse_rust_rewrite.md`
- API notes: `docs/api.md`
- MCP notes: `docs/mcp_protocol.md`
- Integration test harness: `tests/integration/`
