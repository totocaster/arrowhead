# Workflow Templates

## Standard Vault Edit Loop
1. **Intake** – Restate the request; clarify scope + constraints.
2. **Search-first** – `arrowhead search hybrid "<topic>" --limit 20` to confirm existing coverage.
3. **Read** – `arrowhead notes read "Note ID"` for the selected targets.
4. **Plan** – Outline edits + tools (mention files and commands explicitly to the user).
5. **Edit** – Apply minimal diffs via editor/CLI.
6. **Verify** – Re-run the initial search/graph command to ensure fresh index hits; confirm daemon still healthy.
7. **Report** – Summaries must mention behavioural change, checks performed, next steps/gaps.

## Intake to MCP Flow
1. Launch Arrowhead MCP transport (`arrowhead --mcp` or HTTP variant).
2. Immediately call `mcp.discovery.get_vault_conventions` to ingest vault rules.
3. Use `mcp.search.*` or CLI equivalents depending on latency needs.
4. Record MCP warnings/errors; map them to actionable CLI steps (restart daemon, regenerate token, etc.).

## Search Mode Selection Checklist
- **FTS** when: note IDs, metadata filters, precise tokens.
- **Semantic** when: conceptual ideas, adjacent topics, you expect fuzzy matches.
- **Hybrid** when: uncertain or summarizing broad work.
- Switch modes explicitly and explain why to the user.

## Daemon Recovery Drill
```
arrowhead index status        # confirm failure
arrowhead index stop          # ensure clean shutdown
arrowhead index start         # relaunch runtime
sleep 2                       # give watcher time to attach
arrowhead index status        # verify embeddings + queue health
```
If issues persist, inspect `.arrowhead/logs/daemon.log` and report before proceeding.

## Token & Auth Rotation
1. `arrowhead --mcp-server --generate-token` → capture secret (only printed once).
2. Update MCP client configs (Claude, Codex) with new token/URL.
3. Remove stale tokens from configs/secrets stores.
4. Run `/health` (HTTP) or a lightweight `mcp.search.fts` call (stdio) to confirm connectivity.
5. Document the rotation in the user summary when relevant.

## Reporting Template
- What changed (files/commands).
- Validations (search reruns, daemon status, tests).
- Next steps or open questions.
- Any semantic/embedding degradation warnings observed.
