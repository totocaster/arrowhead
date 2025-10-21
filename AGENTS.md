# Arrowhead Coding Agent Playbook

## Mission
- Deliver the Arrowhead Rust rewrite according to `docs/predev_synapse_rust_rewrite.md`.
- Preserve repository cleanliness and avoid ambiguous states; never leave `todo!()` panics in committed code.
- Prioritise maintainability, clear error handling, and strong test coverage using the provided fixture vault.

## Ground Rules
1. **Toolchain:** Build against Rust 1.85 (2024 edition). Update `rust-toolchain.toml` if a later MSRV is required; call it out in reviews.
2. **Features:** The `vector-lancedb` cargo feature is off by default. Enable it only when implementing semantic search/persistence and confirm LanceDB’s MSRV requirements.
3. **Style:** Follow idiomatic Rust (clippy clean, `cargo fmt`). Prefer explicit structs/enums over loose maps; document non-obvious flows with concise comments.
4. **Error Handling:** Use `anyhow`/`thiserror` as scoped in the spec. Return actionable errors instead of panicking.
5. **Testing:** Add unit tests alongside code (`#[cfg(test)]`) and integration tests under `tests/integration/`. Use `tests/fixtures/test-vault` as read-only input; write indexes to temp dirs.
6. **Docs:** Update specs/design docs when behaviour or architecture shifts. `docs/dev_status.md` tracks high-level progress—keep it fresh after major milestones.

## Workflow Expectations
- Start complex tasks with a brief plan (2–5 steps max) and keep it up to date.
- Stage work incrementally; validate with `cargo fmt`, `cargo check`, and relevant tests before surfacing results.
- Summaries must highlight behavioural changes, tests executed, and next steps. Flag known gaps or follow-up items.
- If external dependencies require network or MSRV bumps, pause and confirm with the designer before proceeding.

### Commit Style
- Use single-line messages in the form `type: imperative summary` (e.g., `docs: update spec status`).
- Keep the summary ≤72 characters and pick the closest conventional type (`docs`, `feat`, `fix`, `refactor`, etc.).
- Group related changes into one commit; avoid mixing unrelated work.

## Code Priorities
- **Phase 1:** Implement vault I/O, metadata extraction, and SQLite schema.
- **Phase 2:** Build indexer, search pipeline, and embed LanceDB (behind feature) when ready.
- **Phase 3+:** Complete graph services, MCP transports, and polish.

## Communication
- Ask for clarification when specs conflict or edge cases appear (e.g., concurrent vault access expectations).
- Document assumptions inside PR descriptions or follow-up issues so the designer can confirm.
- Avoid silent scope creep—highlight optional improvements separately.

## Quick Reference
- Spec: `docs/predev_synapse_rust_rewrite.md`
- Status: `docs/dev_status.md`
- API overview: `docs/api.md`
- MCP notes: `docs/mcp_protocol.md`
- CLI commands: `crates/arrowhead-cli/src/commands/`
- Core modules: `crates/arrowhead-core/src/`

Keep the repo ready-to-build at all times. When in doubt, optimise for clarity and testability.
