# Arrowhead Metrics And Context Design

This document proposes how Arrowhead should expand to support file-first
metrics data and richer context retrieval across notes, days, and metrics.

## Status

> **Status:** In progress.
>
> **Last updated:** 2026-04-15

| Area | Status | Notes |
| --- | --- | --- |
| Product direction | Accepted | Arrowhead stays a complementary assistant tool for agents, automation, CLI, and MCP usage. |
| Metrics indexing model | In progress | Metrics conventions resolution, parser/validator coverage, SQLite persistence, core indexing refresh, and read/search surfaces are now wired; mutation indexing is now refreshed directly after record writes. |
| Metrics CLI CRUD | In progress | Record-level `metrics create`, `metrics update`, and `metrics delete` are now wired alongside `metrics files`, `metrics read`, and `metrics search`; file subcommands and `assign-missing-ids` are still pending. |
| Metrics MCP CRUD | In progress | Read-only `mcp.metrics.list_files`, `mcp.metrics.read`, and `mcp.metrics.search` are now wired; mutation tools are still pending. |
| Context command family | Proposed | New context surfaces should work across days, notes, and metrics. |
| Proactive linking | Proposed | Explicit and inferred links should be surfaced with reasons. |
| Implementation | In progress | Round 1 through record-level CLI metrics CRUD are now landing; MCP mutations, file subcommands, and context retrieval are still pending. |

## Accepted Decisions

These decisions are accepted for this feature direction unless a later design
update explicitly revises them.

1. Arrowhead-owned metrics settings should live in `.arrowhead/workspace.toml`
   under a `[metrics]` section, while Arrowhead continues to prefer
   `.obsidian/plugins/metrics-lens/data.json` when present.
2. `context` is the primary user-facing linking and retrieval surface.
3. `graph` remains the lower-level structural primitive layer.
4. Pre-1.0 breaking CLI changes are acceptable if they produce a cleaner,
   more CLIG-compliant command model.
5. Arrowhead CLI design should follow [clig.dev](https://clig.dev/) as the
   default standard, not just a loose suggestion.
6. Metrics CRUD should be added alongside the existing note CRUD model so both
   domains support assistant-grade read and write workflows.
7. Metrics MCP should stay lean and usage-oriented; maintenance utilities such
   as `assign_missing_ids` should stay out of MCP for now.
8. Unknown metric keys should be warnings, not blocking errors.
9. Context payloads should keep stable top-level sections even when some
   sections are empty.
10. Metrics search should reuse Arrowhead's existing query-parser model and add
    metrics-appropriate fields on top.
11. Inferred links belong in the `context` experience because the goal of
    context is to cast a wider light over the vault; users and agents can then
    decide what is truly relevant.
12. Metrics file create, rename, and delete are part of the shipped spec for
    both CLI and MCP; implementation may land in steps, but the target surface
    includes them.
13. Once `context` lands, `graph context`, `notes similar`, and
    `notes surprise` should remain as compatibility aliases for one pre-1.0
    cycle while docs and examples move to `context`.
14. `context metric` should accept both metric keys and metric record ids under
    one command.
15. Metrics search v1 should prioritize `key:`, `source:`, `file:`, `date:`,
    and `note:` fields, while default free-text search should cover note text
    and other human-authored row text.

## Product Position

Arrowhead should not try to replace Obsidian or the Metrics plugin.

The intended split is:

- `metrics-obsidian` remains the primary editing and current-file viewing UX for
  `*.metrics.ndjson`.
- Arrowhead becomes the assistant layer for automation, CLI workflows, MCP
  access, discovery, cross-file retrieval, and cross-domain context.
- Metrics files remain the source of truth. Arrowhead may index them, cache
  derived data, and expose tools on top, but it must not become the canonical
  store.

## Goals

- Support file-first metrics data without compromising the existing note index.
- Expose metrics through both CLI and MCP, including CRUD operations.
- Add context-oriented commands that answer questions like:
  - what happened on a day
  - what changed recently
  - what is connected to this note, metric, or day
- Surface explicit and inferred relationships between notes, days, metrics, and
  sources.
- Keep outputs useful for both humans and agents.

## Context As The Primary Linking Surface

Arrowhead already has two families of link-oriented commands:

- structural graph commands
- discovery and similarity commands

Those should not remain equal peers in the long-term user-facing product.

The recommended model is:

- `context` becomes the primary orchestration surface for retrieving everything
  that matters around an entity or time window
- `graph` remains the precise structural primitive layer
- semantic and discovery-style relatedness folds into `context` rather than
  standing apart as a separate top-level concept

In practice, this means users and agents should reach for `context` first when
they want understanding, and reach for `graph` when they need exact structural
edges.

### Consolidation recommendation

| Existing surface | Recommendation | Reason |
| --- | --- | --- |
| `graph context <note>` | Consolidate into `context note <note>` | This is already a note-context command in all but name. |
| `notes similar <note>` | Consolidate conceptually into `context note <note>` | Similarity is one source of context, not a separate product concept. |
| `notes surprise <note>` | Consolidate conceptually into `context note <note>` | Same reasoning as `notes similar`. |
| `mcp.discovery.get_related_notes` | Consolidate conceptually into `mcp.context.get_note` and related context tools | Agents usually want one richer answer, not multiple partial calls. |
| `graph backlinks <note>` | Keep as primitive | Exact adjacency remains useful for scripting and diagnostics. |
| `graph forward-links <note>` | Keep as primitive | Exact adjacency remains useful for scripting and diagnostics. |
| `graph orphans` | Keep as primitive/diagnostic | This is graph-health analysis, not entity context. |
| `graph unresolved` | Keep as primitive/diagnostic | This is graph-health analysis, not entity context. |

### Command model after consolidation

- `context`: tell me what matters around this thing
- `search`: help me narrow in on likely candidates
- `read`: show me the exact thing in detail
- `graph`: give me exact structural edges
- `metrics`: give me exact metric files, records, and mutations

That model gets cleaner once metrics are added because it gives Arrowhead one
consistent place to blend:

- graph edges
- semantic relatedness
- temporal proximity
- day-based grouping
- metric links
- recent activity

without forcing the caller to know which retrieval subsystem produced each part
of the answer.

### Retrieval model

The intended product model is:

- `context` shines a wider light over the vault and adjacent information
- `search` is narrower, helping the caller find likely matches or candidates
- `read` is a laser pointer for exact inspection

That distinction should shape both command semantics and output design.

## Non-goals

- Replacing the Metrics plugin timeline, modal flows, or chart-heavy current-file UI.
- Turning Arrowhead into a general-purpose analytics dashboard product.
- Introducing a hidden canonical metrics database.
- Adding a standalone MCP validation-report tool.

Validation should still exist, but it should be embedded into read, search, and
context responses rather than exposed as a separate MCP endpoint.

## Source Of Truth And Conventions

Arrowhead should follow Metrics plugin conventions when they exist, then fall
back to its own workspace-level defaults.

Preferred convention sources, in order:

1. `.obsidian/plugins/metrics-lens/data.json`
2. `.arrowhead/workspace.toml` under a future `[metrics]` section
3. Arrowhead defaults mirroring the plugin defaults

Initial conventions to support:

- metrics root: `Metrics/`
- supported extensions: `.metrics.ndjson`
- default write file: `Metrics/All.metrics.ndjson`
- record reference prefix: `metric:`
- week start day
- day start hour

This keeps Arrowhead aligned with the plugin while still working in automation
contexts where the plugin is absent.

## Core Design

Metrics should be modeled as a separate indexed domain, not as fake notes.

The note model and the metrics model have different needs:

- notes are markdown documents with frontmatter, content, graph links, and
  semantic search
- metrics are append-oriented NDJSON records with stable ids, timestamps,
  numeric values, sources, units, validation state, and file-backed mutation

Trying to coerce metrics into the note model would blur command semantics,
ranking, and storage behavior. Arrowhead should instead add a dedicated metrics
index that sits alongside the existing note index.

### Proposed indexed entities

- metrics file
- metric record
- metric issue
- metric-to-note reference
- metric context edge

### Proposed stored record fields

- `id`
- `ts`
- `key`
- `value`
- `source`
- `date`
- `unit`
- `origin_id`
- `note`
- `context`
- `tags`
- raw line text
- source file path
- source line number
- file modified timestamp
- validation status and issues

### Validation model

Arrowhead should validate the same contract the plugin uses, including:

- missing or invalid `id`
- missing or invalid `ts`
- missing or invalid `key`
- invalid numeric `value`
- missing `source`
- invalid `date`
- invalid `context`
- invalid `tags`
- unknown keys
- unknown units
- unit mismatch for known keys
- duplicate `id`
- duplicate `origin_id`

Validation should be exposed:

- inline in `metrics read`
- inline in `metrics search`
- inline in context payloads
- inline in CRUD error messages when a mutation would be unsafe

Validation should not be exposed as its own MCP tool.

Blocking policy for v1:

- unknown keys warn but do not block
- unknown units warn but do not block unless another validation rule makes the
  row unsafe
- duplicate `id` values block ambiguous update and delete operations
- invalid required fields block create and update operations
- duplicate `origin_id` values should surface clearly but should not block CRUD
  on their own

## CLI Feature Set

### Metrics command family

Proposed top-level surface:

```text
arrowhead metrics files
arrowhead metrics files create
arrowhead metrics files rename
arrowhead metrics files delete
arrowhead metrics search
arrowhead metrics read
arrowhead metrics create
arrowhead metrics update
arrowhead metrics delete
arrowhead metrics assign-missing-ids
```

Representative examples:

```bash
arrowhead metrics files --json
arrowhead metrics files create Metrics/Health.metrics.ndjson
arrowhead metrics files rename Metrics/Health.metrics.ndjson Metrics/Body.metrics.ndjson
arrowhead metrics files delete Metrics/Legacy.metrics.ndjson --yes
arrowhead metrics search "key:body.weight source:withings date:past30d"
arrowhead metrics read metric:01JV7RK8Q4X60M0E2N0A6QK61V
arrowhead metrics create --file Metrics/All.metrics.ndjson --key body.weight --value 105.6 --unit kg --source withings --ts 2026-04-14T08:30:00+04:00
arrowhead metrics update 01JV7RK8Q4X60M0E2N0A6QK61V --value 104.9
arrowhead metrics delete 01JV7RK8Q4X60M0E2N0A6QK61V --yes
arrowhead metrics assign-missing-ids --file Metrics/Legacy.metrics.ndjson
```

### CLI behavior

- `metrics read` should accept both raw ids and `metric:<id>` references.
- `metrics create` should write directly to the target NDJSON file and refresh
  the index.
- `metrics update` should locate the canonical row by stable `id`, rewrite the
  owning file safely, and refresh the index.
- `metrics delete` should remove the matching row by stable `id` and refresh
  the index.
- unsafe writes must fail fast with actionable errors, especially when duplicate
  ids make a mutation ambiguous
- human output should include validation state, file path, line number, and
  linked context when available
- `--json` should always return structured payloads for automation

### Metrics search behavior

Metrics search should reuse Arrowhead's existing parser model and default
behaviour wherever that produces a sane user experience.

For v1, the essential metrics-specific fields are:

- `key:`
- `source:`
- `file:`
- `date:`
- `note:`

Default free-text search should cover at least:

- the metric `note` field
- the raw row text
- source file path text
- tag values when present
- other human-authored textual fields that help answer questions like
  "when did I eat steak"

That makes `context` broad, `search` narrower, and `read` precise, while still
keeping search useful for memory-style lookup over metric records.

### CLI design standard

All new CLI command families introduced here should follow
[clig.dev](https://clig.dev/) as the standing Arrowhead CLI design standard.

That implies, at minimum:

- human-first default output
- `--json` for structured machine output
- concise default help with examples
- full `-h` and `--help` support at every command level
- consistent noun/verb naming across subcommands
- actionable, conversational error messages
- clear post-action state summaries when a command changes data
- guidance toward the next relevant command when it helps discovery

Given Arrowhead is still pre-1.0, command reshaping in pursuit of a cleaner
CLIG-compliant design is acceptable.

## MCP Feature Set

### Metrics MCP tools

Proposed tool surface:

- `mcp.metrics.list_files`
- `mcp.metrics.create_file`
- `mcp.metrics.rename_file`
- `mcp.metrics.delete_file`
- `mcp.metrics.search`
- `mcp.metrics.read`
- `mcp.metrics.create`
- `mcp.metrics.update`
- `mcp.metrics.delete`
- `mcp.metrics.get_context`

The naming should stay close to existing Arrowhead conventions and be finalised
when the rest of the MCP surface is wired.

### MCP design rules

- CRUD support must exist for metrics, not just reads and search.
- no standalone `validation_report` MCP tool
- validation must be embedded in normal responses
- payloads must contain enough structure for agents to reason about links,
  recent activity, source files, timestamps, and mutation safety
- file CRUD belongs in the full shipped MCP surface even if implementation
  arrives incrementally

## Context Command Family

Arrowhead needs a new context-oriented surface that sits above the raw notes and
metrics commands.

This should answer questions like:

- show me everything relevant for a day
- show what happened this week
- show what changed recently
- show the context around a note
- show the context around a metric key or record
- show related notes, metrics, and days proactively

### Proposed CLI surface

```text
arrowhead context day
arrowhead context week
arrowhead context changed
arrowhead context note
arrowhead context metric
arrowhead context source
```

Representative examples:

```bash
arrowhead context day 2026-04-14
arrowhead context week --this
arrowhead context changed --days 3
arrowhead context note "Project Hub"
arrowhead context metric body.weight
arrowhead context metric metric:01JV7RK8Q4X60M0E2N0A6QK61V
arrowhead context source withings --range past-30-days
```

### Proposed MCP surface

- `mcp.context.get_day`
- `mcp.context.get_week`
- `mcp.context.get_changed`
- `mcp.context.get_note`
- `mcp.context.get_metric`
- `mcp.context.get_source`

### What each context response should contain

Every context response should be organized into predictable sections:

- `summary`
  - target entity or time window
  - totals and high-signal counts
- `history`
  - recent notes linked to the target
  - recent metric activity linked to the target
  - prior adjacent days or recurring patterns when relevant
- `activity`
  - notes created or modified
  - metrics recorded
  - metrics files modified
- `links`
  - explicit and inferred relationships
- `attention`
  - validation warnings or errors
  - missing linked records
  - ambiguous references
- `related`
  - adjacent days
  - related notes
  - related metric keys or sources

This structure should stay stable across CLI JSON and MCP responses.

### Context and existing graph/discovery commands

`context` should absorb the user-facing role of:

- `graph context`
- `notes similar`
- `notes surprise`
- related-notes discovery flows

while `graph` stays available for low-level structural access.

For compatibility, Arrowhead can keep the older commands initially, but the
documentation and examples should move users toward `context` as the preferred
entry point.

## Proactive Linking Model

Arrowhead should proactively surface links between notes, days, metrics, and
sources, while making the reason for each link explicit.

### Explicit links

- note text contains `metric:<id>`
- note text contains a metric key or source in a strong exact-match form
- record belongs to a file
- record belongs to a day bucket
- daily note corresponds to the same calendar day as a metric record

### Structural links

- note id or title matches a day bucket
- note metadata date matches a metric date
- record and note are modified in the same time window
- record and note share tags or source terms

### Inferred links

- note content frequently co-occurs with a metric key or source
- multiple notes reference records from the same metric key over time
- a metric key repeatedly clusters around a specific day-note pattern
- a record’s `origin_id` ties it to other records or imported provenance

Every surfaced edge should include:

- `reason`
- `kind`
- `confidence`
- `source evidence`

Arrowhead should never present inferred links as if they were explicit.

### V1 link policy

For the first release, Arrowhead should ship:

- explicit links
- structural links
- inferred links with explicit evidence and confidence

The product rationale is that `context` should cast a wide net. It is the part
of Arrowhead that should surface more possible relationships, not fewer.

That does not mean inferred links should be treated as hard facts. Instead:

- inferred links must be clearly labeled as inferred
- inferred links must include evidence
- inferred links must include confidence
- explicit and structural links should remain distinguishable from inferred ones

This keeps the output broad without pretending probabilistic relationships are
certain.

## Suggested UI

Arrowhead should stay headless-first, but the CLI human output should feel like
compact context cards rather than raw dumps.

### CLI human output

Suggested layout:

1. Header summary
2. History section
3. Activity section
4. Linked items section
5. Attention section
6. Suggested next commands

Example shape:

```text
Context: 2026-04-14

Summary
- 3 notes modified
- 12 metric records logged
- 2 linked daily-note references

Activity
- Note: Project Hub (modified 09:14)
- Metric: body.weight 105.6 kg from withings (08:30)

Links
- 2026-04-14 <-> body.weight (same day)
- Project Hub <-> metric:01JV... (explicit reference)
- Project Hub <-> body.weight (inferred, confidence 0.67, recurring co-occurrence)

Attention
- 1 record has a warning: unknown unit
```

### JSON and MCP output

Machine-facing outputs should preserve the same sections as structured arrays
and objects instead of flattening everything into one list.

### Optional future TUI

If Arrowhead later adds a dedicated context TUI, it should stay lightweight:

- left column: target selector and filters
- center column: activity timeline
- right column: related notes, metrics, and attention items

This should complement the existing Metrics plugin, not compete with it.

## Implementation Plan

### Phase 1: Conventions and discovery

- detect Metrics plugin settings from `.obsidian/plugins/metrics-lens/data.json`
- add Arrowhead-side metrics defaults and workspace overrides
- teach vault discovery to find supported metrics files alongside markdown notes

### Phase 2: Metrics index model

- add dedicated SQLite tables for metrics files, records, issues, and links
- add migrations and schema tests
- add file watching and incremental refresh for metrics files
- keep metrics indexing separate from the note index logic

### Phase 3: Metrics CRUD

- implement CLI CRUD
  - record-level `create`, `update`, and `delete` are now wired
- implement MCP CRUD
- ensure writes go to canonical NDJSON files
- reject unsafe mutations with actionable errors

### Phase 4: Context surfaces

- implement day, week, changed, note, metric, and source context
- define stable JSON payloads
- add human-readable CLI renderers
- fold existing note-context and related-notes behaviors into the new context
  family

### Phase 4a: Compatibility and migration

- keep `graph backlinks`, `graph forward-links`, `graph orphans`, and
  `graph unresolved` as supported primitive commands
- treat `graph context`, `notes similar`, and `notes surprise` as compatibility
  surfaces once `context` lands
- update help text and docs to recommend `context` first
- only consider deprecation after the new context surface has proven stable

### Phase 5: Proactive linking

- detect explicit note-to-metric references
- add structural and inferred link generation
- expose reasons and confidence in CLI and MCP

### Phase 6: Polish and hardening

- regression tests around ambiguous ids, invalid rows, and mixed vault states
- indexing performance checks on larger metrics datasets
- command and tool documentation updates in `README.md`

## Crates And Surfaces Likely Affected

- `crates/arrowhead-core`
  - metrics domain model
  - metrics parser and validator
  - metrics index persistence
  - context aggregation
- `crates/arrowhead-daemon`
  - metrics file watching and refresh
- `crates/arrowhead-cli`
  - `metrics` and `context` command families
- `crates/arrowhead-mcp`
  - metrics CRUD tools
  - context tools
- `docs/`
  - design docs
  - CLI/MCP reference updates

## Spec Status

No blocking product-level decisions remain for this design direction.

The remaining work is implementation sequencing, schema design, command/tool
help text, and test coverage rather than unresolved feature scope.

## Recommendation

The first release should prioritise:

- metrics indexing
- metrics CRUD in CLI and MCP
- day and note/metric context commands
- explicit, structural, and inferred note-to-metric linking

The first release should defer:

- chart-heavy Arrowhead UI
- standalone validation-report MCP tools
- `context upcoming`

That keeps Arrowhead focused on assistant-grade retrieval and automation while
respecting the existing Metrics plugin as the primary human editing surface.
