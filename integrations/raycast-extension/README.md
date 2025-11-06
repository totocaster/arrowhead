# Arrowhead Raycast Extension

Search your Arrowhead vault from Raycast, preview results, and open notes in Obsidian or the system default editor.

## Layout

- `src/search.tsx` — Raycast command that shells out to `arrowhead search --json --include-paths` and renders a live-updating list.
- `package.json` — Extension manifest plus build tooling metadata.
- `assets/` — Icons bundled with the extension (`icon.png` mirrors the Alfred workflow icon).
- `build/` — Generated output when running `npm run build` (ignored by git).

## Preferences

Expose the following Raycast preferences (configured via the extensions panel):

| Preference | Default | Description |
| --- | --- | --- |
| `searchMode` | `hybrid` | Search strategy (`fts`, `semantic`, or `hybrid`). |
| `resultLimit` | `15` | Maximum number of results Arrowhead should return. |
| `primaryEditor` | `obsidian` | Editor used for the primary open action (`obsidian` or `default`). |
| `vaultPath` | _(empty)_ | Optional vault override. Leave blank to read Arrowhead’s config. |
| `arrowheadCliPath` | _(empty)_ | Absolute path to the Arrowhead CLI when it is not on `PATH`. |

The command auto-detects the configured vault when no override is supplied and maps the ⌘ modifier to the alternate editor (Obsidian ⇄ default).

## CLI Contract

- Invokes `arrowhead search <mode> "<query>" --json --limit <n> --include-paths`.
- Expects the daemon to be running and the vault indexed.
- Parses the JSON payload produced by `render_results` and mirrors the Alfred integration’s field mapping (title, preview, absolute path, reason, etc.).

## Packaging

- Install dependencies and build: `npm install && npm run build` (runs `ray build --environment dist --output build`).
- Bundle for distribution: `make raycast-extension` (see `scripts/package-raycast-extension.sh`). The script zips `package.json`, `build/`, and `assets/` into `dist/arrowhead-search.raycast`.
- Import into Raycast via **Extensions → Import Extension…** and selecting the generated `.raycast` archive.

## Testing Checklist

- Empty query shows placeholder copy instead of shelling out.
- Search runs for `fts`, `semantic`, and `hybrid` with the configured limit.
- Failure to locate the CLI or vault surfaces descriptive toasts and an empty-state error.
- Notes open in Obsidian by default and fall back to the system editor with ⌘.
- Secondary actions expose copy-to-clipboard and “Reveal in Finder” options when paths are available.

## Implementation Notes

- Ensures PATH includes common install locations (`~/.cargo/bin`, `/opt/homebrew/bin`, etc.) before invoking the CLI.
- Mirrors the vault resolution logic from the Alfred scripts (config parsing fallback, supports `VAULT_PATH` override).
- Uses a debounce when running the CLI to avoid hammering the daemon while the user types (300 ms delay).
