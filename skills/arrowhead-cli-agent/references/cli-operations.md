# CLI Operations Reference

## Search Commands
| Command | Purpose | Typical Flags | Notes |
|---------|---------|---------------|-------|
| `arrowhead search fts "query"` | Fast lexical/BM25 search for known terms, metadata filters, IDs | `--limit N`, `--format human|ids|paths`, `--json` | Use when you know the exact token (`P0461`, `category:project`).|
| `arrowhead search semantic "concept"` | Embedding search for conceptual similarity | Same as above | Requires embeddings; expect ~400 ms latency; scores show similarity only (BM25 column is `N/A`). |
| `arrowhead search hybrid "topic"` | Combined FTS + semantic ranking | Same as above | Default when unsure; explains matched mode per row. |

### Format Flags
- `--format human` (default): table with BM25/semantic scores and match reasons.
- `--format ids`: note IDs only (pipe to `xargs`, `wc -l`, etc.).
- `--format paths`: absolute file paths.
- `--json`: machine-readable payload with metadata, ready for `jq`.

### Metadata Filters & Tips
- Combine filters inline: `arrowhead search fts "category:project status:active"`.
- Boolean FTS behaviour is AND by default; separate unrelated terms if necessary.
- For exploratory work, start hybrid → narrow with FTS filters once you spot candidate notes.

## Graph Commands
| Command | Description | Output Tricks |
|---------|-------------|---------------|
| `arrowhead graph backlinks "Note"` | Incoming links | Supports `--format ids|paths|human|--json`; default human matrix shows counts + previews. |
| `arrowhead graph forward-links "Note"` | Outgoing links | Same flags; use to audit link hygiene. |
| `arrowhead graph context "Note"` | Combined in/out context | Great for project hub reviews. |
| `arrowhead graph orphans` | Notes without links | Pair with `--format ids` to feed clean-up scripts. |
| `arrowhead graph unresolved` | Broken wikilinks | Audit before shipping. |

## Notes Commands
| Command | Usage | Notes |
|---------|-------|-------|
| `arrowhead notes list` | List all notes; add `--json` for full metadata | Pipe-friendly with IDs/paths formats. |
| `arrowhead notes read "Note ID"` | Print content with metadata header | Prefer this over `cat` for consistent encoding + metadata. |
| `arrowhead notes similar "Note ID"` | Semantic neighbors of an existing note | Alias: `notes surprise`; limit default 10. |

## Indexer & Vault Management
| Command | Purpose |
|---------|---------|
| `arrowhead index status` | Show daemon status, embed runtime health. |
| `arrowhead index start|stop|restart` | Control runtime; `start` auto-launches watcher and embedding pool. |
| `arrowhead index autostart enable|disable|status` | Manage launchd/systemd registration. |
| `arrowhead vault status` | Inspect stored vault metadata + health summary. |
| `arrowhead vault reset` | Delete Arrowhead caches/index; reindex from scratch (warn user first). |

## Tail Logs & Debugging
```bash
tail -f /path/to/vault/.arrowhead/logs/cli.log
tail -f /path/to/vault/.arrowhead/logs/daemon.log
```
Capture only the lines needed (<25 words) when quoting in chat.

## MCP Launch Commands
| Command | Situation |
|---------|-----------|
| `arrowhead --mcp` | stdio MCP server (Claude Code, Codex CLI). |
| `arrowhead --mcp-server --bind 127.0.0.1:3911 --token $TOKEN` | HTTP MCP service for remote agents. |
| `arrowhead --mcp-server --generate-token` | Mint bearer token (digest stored, raw printed once). |

See `references/mcp-transports.md` for binding/auth guidance.
