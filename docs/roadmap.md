# Arrowhead Roadmap

This roadmap rolls up the outstanding work from the retired planning/status docs. It focuses on the remaining tasks needed to make Arrowhead comfortable for open-source release and on the near-term backlog we want to track.

## Near-Term (Pre-release) Tasks

- **Toolchain alignment**: bump the workspace `rust-version` metadata to 1.86 so it matches `rust-toolchain.toml`, then update CI to the same baseline.
- **Model licensing & distribution**: document licensing for the bundled fastembed presets, describe cache override options, and ensure the CLI/docs call out the defaults.
- **MCP HTTP observability**: add metrics/tracing hooks for the HTTP transport (e.g. `/metrics`, structured spans) plus regression tests for bearer/link-token auth failures.
- **Graph runtime insight**: surface queue depth/back-pressure metrics from the daemon writer pipeline and ensure they appear in status frames/logs.
- **Hybrid search tuning**: benchmark hybrid weights against fixture vaults, capture semantic snippet improvements, and add regression fixtures for sqlite-vec-backed runs.
- **Indexing diagnostics**: capture quantitative status (event queue depth, write latency) so operators can spot when the daemon is falling behind.

## Performance & Benchmarking

- Build a Criterion-based harness that covers inventory building, indexing (with/without embeddings), and large-vault scenarios.
- Persist or reuse the inventory snapshot so subsequent runs can skip redundant filesystem scans.
- Revisit SQLite prepared-statement pooling inside the indexer writer.
- Emit timing metrics around embedding generation and sqlite-vec batch flushes to validate pooling effectiveness.
- Explore parallel filesystem walking (e.g. via `ignore` crate parallel traversal) for large vaults.

## Operational Hardening

- Document a clear schema migration policy (drop-and-reindex vs. future migrations) and note how to handle incompatible index versions.
- Clarify expectations for concurrent vault access or explicitly state single-writer support only.
- Track sqlite-vec MSRV and libc compatibility so CI agents stay in lock-step.

## Optional / Nice-to-have Backlog

- Metrics endpoint or richer telemetry for the daemon (beyond basic status snapshots).
- Additional search features (date range filters, regex/proximity/fuzzy modes) once the core pipeline is solid.
- Auxiliary tooling ideas from the rewrite spec—exports, backup/restore, dashboard UI, multi-vault management, third-party integrations—should land only after the core stability items above.

## References

- Primary usage and API docs now live in `README.md`.
- Developer workflow guidelines remain in `docs/feature_development_guide.md`.
