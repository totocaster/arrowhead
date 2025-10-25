# Arrowhead Feature Development Guide

This guide defines the baseline expectations for implementing new features in
Arrowhead. Treat it as a companion to `AGENTS.md` and the rewrite
specification.

## 1. Planning & Scope
- Confirm the target phase and specification pages (`docs/predev_synapse_rust_rewrite.md`).
- Identify affected crates and command surfaces before coding.
- Capture open questions early; never assume behaviour that contradicts the
  spec or existing UX.

## 2. Logging & Observability
- Use `tracing` for diagnostic output. During CLI execution, logs normally flow
  to `.arrowhead/logs/cli.log` via `logging::scoped_file_logging`. When
  building with the `vector-lancedb` feature we keep file logging opt-in (set
  `ARROWHEAD_ENABLE_FILE_LOGS=1`) to avoid known issues in LanceDB's tracing
  subscriber. Keep stdout/stderr for intentional user-facing output only.
- Emit at least `info!` on command start/finish and for notable decisions (e.g.
  skipping stale work, writing migrations).
- When adding async tasks, ensure the logging guard spans their lifetime so the
  non-blocking writer can flush.

## 3. Testing Expectations
- Unit tests co-located with the code (`#[cfg(test)]`). Use the fixture vault in
  `tests/fixtures/test-vault` for vault/indexer scenarios; never mutate it.
- Integration tests belong under `tests/integration/` and should use ephemeral
  temp directories for side effects.
- Cover behavioural regressions before introducing new logic; add regression
  tests when fixing bugs.
- Always run `cargo fmt`, `cargo check`, and the relevant `cargo test` suite
  prior to handing work back.

## 4. Data & Persistence
- SQLite changes require migrations in `sqlite.rs` plus tests proving schema
  upgrades succeed. Document versioning decisions in `docs/dev_status.md` when
  they influence roll-outs.
- Vault-aware features must respect detected Obsidian settings (e.g.
  `.obsidian/app.json` ignore filters, attachment directories) and surface them
  through the `VaultSettings` APIs.
- Feature flags (e.g. `vector-lancedb`) stay off by default until the phase
  specifies activation.

## 5. Documentation & Communication
- Update `docs/dev_status.md` after significant milestones or architecture
  shifts.
- When a new workflow or guideline emerges, add it here and cross-reference
  from `AGENTS.md`.
- Summaries in delivery comments must describe behaviour, tests executed, and
  immediate next steps or risks.

## 6. Review Checklist
1. Logging routed to file, stdout kept clean.
2. Tests covering success, edge cases, and error paths.
3. Docs + status files updated.
4. `cargo fmt`, `cargo check`, `cargo test` executed.
5. No stray `todo!()`/`dbg!()`/dead debug prints.

Following this checklist keeps Arrowhead reliable, observable, and easy to
extend.
