# Arrowhead Alfred Workflow

This integration lets Alfred trigger Arrowhead CLI searches and open matching notes directly in Obsidian or the system default editor.

## Layout

- `src/search.py` &mdash; Script Filter entrypoint. Wraps `arrowhead search <mode> --json` and emits Alfred Script Filter JSON.
- `src/open_note.py` &mdash; Run Script handler for opening the selected note in the configured editor.
- `workflow/` &mdash; Holds `info.plist` plus the generated `.alfredworkflow` archive.
- `assets/` &mdash; Placeholder for icons exported with the workflow bundle.

## Workflow Variables

Expose these via Alfred 5's Workflow Configuration panel:

| Variable | Default | Description |
| --- | --- | --- |
| `SEARCH_MODE` | `hybrid` | Search strategy (`fts`, `semantic`, or `hybrid`). |
| `RESULT_LIMIT` | `15` | Maximum results request per query. |
| `PRIMARY_EDITOR` | `obsidian` | Editor used on Return key (`obsidian` or `default`). |
| `VAULT_PATH` | _(empty)_ | Optional vault override. Leave blank to read Arrowhead's config. |
| `ARROWHEAD_CLI_PATH` | `arrowhead` | Path to the CLI binary when it is not on `PATH`. |

The Script Filter will populate Alfred item variables (`open_editor`, `primary_editor`, `secondary_editor`) so the downstream Run Script knows which editor to invoke.

## Wiring the Workflow

1. Add a **Keyword** input (e.g., `n`) with *Argument: Required*.
2. Connect to a **Script Filter** running `/usr/bin/python3` and call `src/search.py`:

   ```bash
   /usr/bin/python3 "{query}" <<'PY'
   import runpy, sys, pathlib
   runpy.run_path(str(pathlib.Path("$WORKFLOW_DIR") / "src" / "search.py"), run_name="__main__")
   PY
   ```

   (Alfred expands `$WORKFLOW_DIR` automatically when running scripts.)

3. Connect the Script Filter to a **Run Script** action executing `/usr/bin/python3 src/open_note.py "{query}"`.
4. Optionally attach an **Open File** action for Quick Look preview or additional modifiers.

Ready-to-install build: `workflow/arrowhead-search.alfredworkflow`. Double-click to import, or regenerate with `make alfred-workflow` before distribution (the script injects the current Arrowhead workspace version into the bundle).

## CLI Contract & JSON Payload

- Invokes `arrowhead search <mode> "<query>" --json --limit <n>` (default limit 15, modes: `fts`, `semantic`, `hybrid`). The workflow assumes the vault has been initialised and the daemon is running.
- JSON output mirrors the CLI `render_results` structure:

  ```json
  [
    {
      "note_id": "20230815T103210Z",
      "title": "Project Roadmap",
      "score": 0.812345,
      "bm25": 2.0,
      "relative_path": "Projects/Roadmap.md",
      "preview": "…important milestones captured here…",
      "reason": "Hybrid blend: FTS rank 3, semantic boost 0.18",
      "metadata": {
        "tags": ["planning", "q3"],
        "category": "project"
      }
    }
  ]
  ```

- Field mapping:
  - `title` (fallback to `metadata.title` or `note_id`) → Alfred item title.
  - `preview` snippet → Alfred subtitle.
  - `relative_path` combined with the vault root → Alfred argument/copy text.
  - `reason` → ⌘ modifier subtitle explaining the ranking.
  - `note_id` → Alfred UID for knowledge-based ordering.

## Packaging

- `make alfred-workflow` &mdash; runs `scripts/package-alfred-workflow.sh` to rebuild `workflow/arrowhead-search.alfredworkflow`.
- Alfred UI &mdash; alternatively, open the workflow and choose **Export...** to create a bundle manually.
- Any files placed in `workflow/` (e.g., `icon.png`) are packaged automatically; Alfred falls back to a generic icon if none is supplied.

## Testing Checklist

- Daemon offline: Script Filter returns a friendly error message.
- Empty query: script shows placeholder instead of shelling out.
- Search runs for `fts`, `semantic`, and `hybrid` with the configured result limit.
- Result items open in Obsidian by default and switch to the system editor via ⌘ modifier.
- Vault path auto-detected from Arrowhead config unless overridden.

## Implementation Notes

- `src/search.py` shells out to `arrowhead search --json --include-paths`, enriches PATH with common install locations (`~/.local/bin`, Homebrew, etc.), and auto-detects the CLI path before surfacing errors.
- `src/open_note.py` resolves absolute paths and opens notes in Obsidian by default (⌘ routes to the macOS default editor or custom command).
- Workflow variables expose search mode, result limit, editor choice, vault override, and CLI override.
- `make alfred-workflow` (via `scripts/package-alfred-workflow.sh`) regenerates `workflow/arrowhead-search.alfredworkflow` and syncs its version with the workspace package. The GitHub release workflow ships the bundle alongside macOS binaries.
