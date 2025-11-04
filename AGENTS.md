# Arrowhead Coding Agent Playbook

## Mission
- Deliver the Arrowhead Rust rewrite according to the consolidated spec in `README.md`.
- Preserve repository cleanliness and avoid ambiguous states; never leave `todo!()` panics in committed code.
- Prioritise maintainability, clear error handling, and strong test coverage using the provided fixture vault.

## Ground Rules
1. **Toolchain:** Build against Rust 1.86 (2024 edition). Update `rust-toolchain.toml` if a later MSRV is required; call it out in reviews.
2. **Features:** Semantic search ships by default using sqlite-vec; keep embeddings healthy and avoid regressing the always-on pipeline.
3. **Style:** Follow idiomatic Rust (clippy clean, `cargo fmt`). Prefer explicit structs/enums over loose maps; document non-obvious flows with concise comments.
4. **Error Handling:** Use `anyhow`/`thiserror` as scoped in the spec. Return actionable errors instead of panicking.
5. **Testing:** Add unit tests alongside code (`#[cfg(test)]`). Integration tests under `tests/integration/` will land later; plan your APIs so those paths stay testable. Use `tests/fixtures/test-vault` as read-only input; write indexes to temp dirs.
6. **Docs:** Update specs/design docs when behaviour or architecture shifts so the expectations stay current.

## Workflow Expectations
- Start complex tasks with a brief plan (2–5 steps max) and keep it up to date.
- For MCP tooling, call `mcp.discovery.get_vault_conventions` right after initialize and before any note creation, update, or deletion so agents respect vault naming rules.
- Stage work incrementally; validate with `cargo fmt`, `cargo check`, `cargo clippy --all-targets -- -D warnings`, and relevant tests before surfacing results or pushing.
- Summaries must highlight behavioural changes, tests executed, and next steps. Flag known gaps or follow-up items.
- If external dependencies require network or MSRV bumps, pause and confirm with the designer before proceeding.

### Commit Style
- Use single-line messages in the form `type: imperative summary` (e.g., `docs: update spec status`).
- Keep the summary ≤72 characters and pick the closest conventional type (`docs`, `feat`, `fix`, `refactor`, etc.).
- Group related changes into one commit; avoid mixing unrelated work.

## Project Priorities
- Track current roadmap work in the README’s Roadmap section.

## Communication
- Ask for clarification when specs conflict or edge cases appear (e.g., concurrent vault access expectations).
- Document assumptions inside PR descriptions or follow-up issues so the designer can confirm.
- Avoid silent scope creep—highlight optional improvements separately.

## Quick Reference
- Spec: `README.md`
- Feature guide: `docs/feature_development_guide.md`
- API overview: `README.md#cli-reference`
- MCP notes: `README.md#mcp-protocol-details`
- CLI commands: `crates/arrowhead-cli/src/commands/`
- Core modules: `crates/arrowhead-core/src/`

Keep the repo ready-to-build at all times. When in doubt, optimise for clarity and testability.
