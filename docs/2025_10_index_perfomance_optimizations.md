# Arrowhead Indexing Performance Optimisation Log — 2025-10

## Summary

- Introduced a reusable vault inventory that captures note IDs, paths, and
  filesystem timestamps in a single pass. Indexing, single-note refreshes, and
  wikilink resolution now operate on this cached snapshot instead of
  re-enumerating the vault for every note.
- Batched database state reads by loading the entire `notes` table state into
  memory before indexing and by reusing a per-thread SQLite connection; the
  indexer no longer performs one query per note to obtain staleness data.
- Replaced the single global fastembed mutex with a bounded pool of model
  instances so parallel workers can generate embeddings concurrently without
  blocking each other. Semantic writes are buffered and flushed in batches to
  reduce sqlite-vec delete/add churn.
- Added thread-scoped SQLite connection caching to keep prepared statements and
  WAL buffers warm during heavy ingestion.
- Normalised minor search test formatting that surfaced during `cargo fmt`.

## Observed Impact

- Pre-optimisation baseline: 1 528 notes indexed in 7 min 35 s (≈455 s), or ~3.36 notes/s.
- Post-optimisation run: 1 528 notes indexed in 4 min 58 s (≈298 s), or ~5.13 notes/s.
- Net improvement: ~34.5 % reduction in wall-clock time and ~52 % increase in throughput.
- Profiling logs still show embedding generation as the dominant slice of the remaining runtime; further gains will depend on hardware-accelerated embedding backends (e.g. Core ML on Apple Silicon).

## Tests

- `cargo fmt`
- `cargo check`
- `cargo test`

## Follow-up / Next Steps

1. **Benchmark Harness:** Add criterion-based benchmarks that cover inventory
   building, indexing with and without embeddings, and large-vault scenarios to
   quantify the impact of these changes and catch regressions.
2. **Inventory Persistence:** Explore persisting the inventory snapshot (e.g.
   to `.arrowhead/index/inventory.json`) so subsequent runs can skip the initial
   filesystem walk when directories are unchanged.
3. **Prepared Statement Pooling:** Revisit SQLite `upsert_note` to reuse prepared
   statements within the thread connection for further micro-optimisation.
4. **Embedding Pipeline Metrics:** Emit structured timing metrics around embed
   generation and sqlite-vec flushes to validate pooling effectiveness across
   diverse CPU configurations.
5. **Parallel File I/O:** Investigate switching the inventory walker to the
   `ignore` crate’s parallel traversal for even faster directory scanning on
   multi-core systems.
