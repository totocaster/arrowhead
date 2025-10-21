# Arrowhead Development Status — 2025-10-21

## Snapshot

- **Toolchain:** Rust 1.85 (2024 edition) pinned via `rust-toolchain.toml`.
- **Workspace health:** `cargo check` / `cargo test` pass with the new scaffolding.
- **Crates:** `arrowhead-core`, `arrowhead-cli`, and `arrowhead-mcp` compile with fully typed modules returning informative `todo` errors.
- **CLI:** Command surface (`init`, `index`, `search`, `notes`, `graph`, `vault`) parses successfully and persists configuration via `AppConfig`.
- **Documentation:** Specification aligned (`docs/predev_synapse_rust_rewrite.md`), API/MCP reference stubs added, integration test harness directories created.
- **Vectors:** LanceDB wiring lives behind the optional `vector-lancedb` feature; disabled until semantic search sprint begins.

## Completed Work (Phase 0)

- Workspace upgrade to modern Rust/edition with reproducible toolchain.
- Dependency audit and alignment to current stable releases.
- Replacement of `todo!()` placeholders with structured scaffolding across core modules.
- CLI architecture (commands module tree, config loader, tracing bootstrap).
- Documentation refresh and repository structure parity with the specification.

## Next Focus Areas

1. **Vault & Metadata Implementation (Phase 1)**
   - Flesh out `Vault`, `MetadataExtractor`, and associated types.
   - Start wiring SQLite schemas (notes, metadata, FTS tables).
   - Add unit tests using `tests/fixtures/test-vault`.

2. **Indexer Foundations (Phase 1 → Phase 2)**
   - Implement staleness checks, note traversal, and progress reporting.
   - Define schema migrations/versioning approach.

3. **Search & Embeddings (Phase 2)**
   - Enable `vector-lancedb` feature when ready, confirm MSRV remains acceptable.
   - Implement embedding generation via `fastembed`, persistence via LanceDB, and hybrid search strategy.

4. **MCP Surface (Phase 4-5)**
   - Finalise JSON-RPC types and tool schemas.
   - Implement stdio transport, then HTTP transport with bearer auth.

## Open Decisions / Risks

- **Vector MSRV:** Monitor LanceDB releases; enabling additional features may require Rust ≥1.86.
- **Model distribution:** Need decision on embedding model delivery (bundle vs. download vs. local path).
- **Schema migrations:** Determine approach for evolving SQLite schema once Phase 1 data structures land.
- **Concurrent access:** Clarify whether multi-process vault access needs to be supported in v1.

## Tracking

- Specification: `docs/predev_synapse_rust_rewrite.md`
- API notes: `docs/api.md`
- MCP notes: `docs/mcp_protocol.md`
- Integration test harness: `tests/integration/`
