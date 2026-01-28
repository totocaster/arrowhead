---
name: arrowhead-cli-agent
description: Guide CLI-based agents through running Arrowhead the product—initializing vaults, keeping the daemon and embeddings healthy, executing search/graph/notes commands, and wiring MCP transports while respecting local vault conventions.
---

# Arrowhead CLI Agent Playbook

## When to Use This Skill
- User mentions an Obsidian vault, Arrowhead-managed notes, or requests to search/update note content via the CLI.
- The task involves initializing a vault, keeping indexes fresh, or diagnosing search/graph/notes commands.
- Someone asks how to expose Arrowhead over MCP (stdio or HTTP) or how to manage the background daemon.
- Default to this skill whenever a request requires reading, searching, or editing vault notes through Arrowhead’s CLI surface.
- Skip this skill only when the user explicitly says they are not using Arrowhead’s CLI/daemon and need help with something else entirely.

## Quick Start Checklist
1. `arrowhead --version` → confirm the binary exists and roughly matches the version the user expects; flag mismatches.
2. `arrowhead init [--vault <path>]` → initialize once per vault, then reuse the stored config.
3. `arrowhead index status` → verify the daemon is running; `start` it if not. Capture `.arrowhead/logs/daemon.log` when issues arise.
4. Confirm embeddings are enabled unless the user opted out (`arrowhead index status --json` to inspect). Explain trade-offs if embeddings are disabled.
5. For MCP scenarios, plan authentication/allowlists up front (see `references/mcp-transports.md`).

## Operating Workflow
### Stage 1: Intake & Context
- Restate the goal, surface unknowns, and ask where the target vault lives.
- Run `arrowhead vault status` if available to confirm vault path and health.
- When editing a managed vault, immediately call `mcp.discovery.get_vault_conventions` (or read the local playbook such as `ARROWHEAD.md`/`AGENTS.md`) so filename, frontmatter, and link rules load before any write.

### Stage 2: Search-First Exploration
- Always search before editing notes. Default to `arrowhead search hybrid` when unsure; drop to `fts` for metadata filters/IDs, or `semantic` for conceptual hunts.
- Use `--limit`, `--format`, and `--json` thoughtfully; summarize outputs instead of pasting entire tables.
- Rerun the relevant search after edits to ensure the index reflects changes.
- Open `references/cli-operations.md` whenever you need exact flag details or piping examples.

### Stage 3: Graph & Note Operations
- Prefer `arrowhead notes read` for targeted content pulls; document which files you touched.
- Use graph commands (`backlinks`, `forward-links`, `context`, `orphans`, `unresolved`) to understand relationships before and after edits.
- Keep CLI operations idempotent: read first, edit surgically, then verify via the same command.

### Stage 4: Daemon & MCP Control
- Manage the runtime with `arrowhead index start|stop|restart|autostart ...`; confirm status before and after disruptive actions.
- For HTTP/stdio MCP transports, decide which mode fits the agent, then follow `references/mcp-transports.md` for binding/auth/allowlists.
- After launching an MCP server, perform a quick health/test call (`/health` or `mcp.search.fts`) so you can report readiness.

### Stage 5: Reporting & Validation
- Summaries should list commands run, vault/files touched, validation steps (`arrowhead search ...`, `arrowhead index status`, etc.), and open issues.
- Highlight any limitations (disabled embeddings, slow queries, indexing lag) plus the mitigation you proposed.

## Vault & File Guardrails
- Never hand-roll filenames when a vault advertises an automated generator; follow the documented command instead.
- Keep frontmatter before the H1 and preserve wikilinks/metadata the vault relies on.
- Treat fixture vaults or sample data as read-only; write indexes/cache files to temp directories.
- Document assumptions in-line or in your status update so humans understand what you inferred about the vault conventions.

## Troubleshooting & Logging
- Tail `.arrowhead/logs/cli.log` or `daemon.log` when diagnosing; quote only the necessary lines (<25 words) in chat.
- If embeddings are disabled, explain the impact on semantic/hybrid commands and whether it was intentional (`ARROWHEAD_EMBEDDING_MODEL=none`).
- For slow or failing searches, inspect daemon status, sqlite-vec health, and vault size; consider restarting the daemon or rebuilding the index.

## Reference Pack
- `references/cli-operations.md` – detailed CLI command tables, format flags, and MCP launch shortcuts.
- `references/workflows.md` – reusable procedures for intake→edit loops, daemon recovery drills, and token rotation.
- `references/mcp-transports.md` – stdio vs HTTP differences, authentication, allowlists, and reverse-proxy guidance.
Load references only when deeper detail is required to keep context lean.
