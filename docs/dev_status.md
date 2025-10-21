# Arrowhead Development Status — 2025-10-21

## Snapshot

- **Toolchain:** Rust 1.85 (2024 edition) pinned via `rust-toolchain.toml`.
- **Workspace health:** `cargo fmt`, `cargo check`, and `cargo test` all pass.
- **Crates:** Core/CLI/MCP crates ship concrete implementations for Phase 1 (vault, metadata, SQLite, indexer) with tests.
- **Indexer:** Staleness detection compares filesystem mtimes with stored `indexed_at`, skipping unchanged notes automatically and respecting Obsidian ignore filters.
- **Vault settings:** `.obsidian/app.json` is parsed for attachments and user ignore filters so templates stay out of the index.
- **CLI:** `init`, `index`, and full `notes` CRUD (read/list/create/update/delete) execute end-to-end; logging writes to `.arrowhead/logs/arrowhead.log` with multi-day retention.
- **Search:** `arrowhead search fts` executes against SQLite FTS5 with `field:value` and boolean syntax, stemming (`porter`) enabled, richer relevance scores, cleaner snippets, and self-refreshing the index beforehand.
- **CI:** GitHub Actions workflow (`CI`) runs on push/PR/workflow_dispatch, enforcing fmt, clippy, check, and test across the workspace.
- **Documentation:** Specification aligned (`docs/predev_synapse_rust_rewrite.md`), feature development guide established, integration fixtures ready.
- **Documentation:** Specification aligned (`docs/predev_synapse_rust_rewrite.md`), API/MCP reference stubs added, integration test harness directories created.
- **Vectors:** LanceDB wiring lives behind the optional `vector-lancedb` feature; disabled until semantic search sprint begins.

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

## Next Focus Areas

1. **Phase 2 Prep — Embedding Infrastructure**
   - Finalise model tier presets (`fast`/`good`/`better`), source ONNX assets from Hugging Face with licensing checks, and persist selection under `.arrowhead/config`.
   - Implement authenticated/unauthenticated download + caching flow for embedding models, including progress reporting and checksum validation.
   - Confirm LanceDB's current MSRV and bump the workspace toolchain as needed before enabling the `vector-lancedb` feature.

2. **Search & Embeddings (Phase 2 Execution)**
   - Layer semantic search: embedding generation via `fastembed`, LanceDB persistence, hybrid scoring.
   - Expand CLI with `semantic`/`hybrid` modes once vector pipeline lands.

3. **Graph Pipeline (Phase 3)**
   - Defer WikiLink resolution persistence to the upcoming graph implementation; schedule planning session ahead of Phase 3 kickoff.

4. **MCP Surface (Phase 4-5)**
   - Finalise JSON-RPC types and tool schemas.
   - Implement stdio transport, then HTTP transport with bearer auth.

## Open Decisions / Risks

- **Vector MSRV:** Track LanceDB's requirements; we are comfortable bumping to the latest stable Rust once we confirm the need.
- **Model distribution:** Implement Hugging Face-backed downloads with clear licensing documentation and opt-in presets.
- **Schema migrations:** Continue relying on drop-and-reindex for incompatible schemas; document any future scenarios that require persistent migrations.
- **Concurrent access:** Clarify whether multi-process vault access needs to be supported in v1.

## Tracking

- Specification: `docs/predev_synapse_rust_rewrite.md`
- API notes: `docs/api.md`
- MCP notes: `docs/mcp_protocol.md`
- Integration test harness: `tests/integration/`
