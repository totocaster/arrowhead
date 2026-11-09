# Incremental indexing contract

The daemon keeps the index in step with the vault between full passes. It
collects watcher paths into a batch, plans the batch with
`Indexer::plan_paths`, and applies the plan with `Indexer::apply_plan`.
`Indexer::reindex_paths` runs both steps in one call. After a batch, the notes,
metrics files, and links inside the batch's paths match what a full pass
(`Indexer::index_all`) would store for them.

## Every watcher path is a subtree

The planner ignores event kinds, because the platform watcher does not report
them reliably. FSEvents reports a folder rename as the old and the new folder
path, with no paths for the notes inside. It reports a folder moved out of the
vault as the old path only, and a folder moved in as the new path only. It
reports an edit to an existing note as `Create(File)`.

`Vault::inventory_within` turns each path into a vault-relative scope:

- A path that names a folder covers everything below that folder.
- A path that no longer exists still counts. The indexed notes and metrics
  files below it are removed, whether or not its name has an extension.
- A path inside another path in the same batch adds nothing.
- Paths outside the vault and paths inside `.arrowhead` are dropped.

When a batch plans nothing, the daemon writes no status and no log line. The
daemon's own status and log writes raise watcher events, so this rule keeps
those events from starting more batches.

When notify reports that the platform lost events, the batch reconciles the
lost paths. inotify reports a queue overflow with no paths, so the watcher sends
the vault root and the batch reconciles the whole vault. FSEvents names the
folder to rescan. Each rescan writes a warning to the daemon log.

The daemon still merges every batch that is waiting in its queue before it
plans.

## Scopes use the same rules as a full pass

For notes, the full inventory's folder rules apply to every folder above and
below each scope:

- Folders whose names start with `.`, such as `.trash`, `.obsidian`, and
  `.git`, are skipped.
- Folders named `attachments` in any case, the configured attachments folder,
  `.arrowhead`, and ignored folders are skipped.
- Symbolic links are never followed, for folders or for notes.
- Note files whose names start with `.`, such as `.dotfile.md`, are included,
  because the full inventory includes them.

When several files in one folder claim the same note id, such as `Plan.md` and
`Plan.MD`, the file that sorts first keeps the id, as in a full pass. Scopes are
compared with the names that the folder listing returns. On a case-insensitive
volume, a path that matches a note only by case finds no note.

For metrics, `Vault::metrics_files` decides which files exist, in targeted and
full passes alike. It walks the metrics root without the note folder rules.
An event inside a hidden or ignored folder under the metrics root therefore
updates metrics. Symlinked metrics files and folders are skipped in both
passes.

## A plan reindexes, skips, or removes

| Found inside a scope | Action |
| --- | --- |
| A note that the inventory has | Reindex it when it is new or stale. Otherwise skip it. |
| An indexed note that the inventory does not have | Remove it. |
| A metrics file that `Vault::metrics_files` has | Reindex it when it is new or stale. Otherwise skip it. |
| An indexed metrics file that `Vault::metrics_files` does not have | Remove it. |

Metrics removals run before metrics writes, in targeted passes and in full
passes. A moved metrics file keeps its record ids, and `metric_records.id` is
unique. The stale path must give up those ids before the new path can store
them.

## Staleness compares content

A note is stale when its modification time is newer than the indexed time. It
is also stale when `NoteRecord::content_fingerprint` differs from the stored
`notes.content_fingerprint`. The fingerprint is a SHA-256 of the note's
vault-relative path, frontmatter, and body.

A metrics file follows the same rule with `metrics::metrics_rows_fingerprint`.
That fingerprint is a SHA-256 of the file's non-blank lines and their line
numbers. `IndexDatabase::upsert_metrics_file` computes the stored value from
the rows it writes. A metrics mutation and the indexer therefore store the same
fingerprint for the same file.

An edit that keeps the old modification time is reindexed by its watcher event
and by the next full pass. Full passes read every note and every metrics file
to compare fingerprints. Before schema version 7, full passes read every note
but only the metrics files with a newer modification time.

## Stored links follow the notes they can reach

`note_links.raw_text` keeps the text between `[[` and `]]`. `WikiLink::parse`
turns that text back into a target, a heading, and display text. Stored links
can therefore resolve again without reading their notes.

`graph::LinkTargets` resolves a target in this order:

1. The exact note id.
2. A note id that matches without regard to case.
3. A note title.
4. An alias that exactly one note claims.

When several notes match a case-insensitive id or a title, the smallest note id
wins. An alias that two or more notes claim resolves to no note, with reason
`unresolved`.

`IndexDatabase::refresh_link_targets` resolves every stored link again. It
rewrites `target_id` and `reason` only. `raw_text`, `heading`, and
`display_text` keep their extracted values, and no note file is read or
written. It reserves the SQLite writer before reading the targets, so a
concurrent writer cannot invalidate the snapshot between reading and updating
the links.

Every full pass ends with a refresh. A targeted pass refreshes when it changed
the notes that links can reach: a note was created, deleted, or renamed, or a
title or an alias changed. A backlink therefore appears when its target note is
created after the source, and it disappears when the target is deleted. A link
to an alias moves when the alias gains or loses a second claimant.

`IndexDatabase::remove_note` sets `reason` to `unresolved` on the links that
pointed at the removed note, along with clearing `target_id`.

## Cost of a batch

Planning lists each folder that contains a scope once per batch. It walks the
whole subtree of each folder scope. It finds the indexed notes inside the scopes
through range lookups on the `idx_notes_relative_path` index.

A batch with notes loads every note state and loads the link targets before
and after the changes. Its database work therefore grows with the number of
indexed notes even for a single-file event. A batch that changes the notes
that links can reach also scans every stored link.

A path that names the vault root reconciles every note. That costs about as
much as a full pass. The poll watcher reports a folder's own path when the
folder's modification time changes. Under that watcher, a note created at the
vault root costs a whole-vault reconcile. The daemon uses the platform watcher
unless it is built with `WatcherStrategy::Poll`.

## Schema version 7

`INDEX_SCHEMA_VERSION` is 7. Version 7 adds `notes.content_fingerprint`,
`metric_files.content_fingerprint`, and the `idx_notes_relative_path` index.

`IndexDatabase::open` never deletes or replaces `index.db`. The daemon, the
CLI, and the MCP server share that file, and a process with a connection to a
replaced file keeps writing to a database that no other process reads. An
index from an older version is rebuilt in place instead. One `BEGIN IMMEDIATE`
transaction drops every table, creates the version 7 schema, and stores the
new version. A process that opens the index during the rebuild waits up to
five seconds for SQLite's write lock, reads the version again, and finds the
work done. If the rebuild fails or its process dies, SQLite rolls the
transaction back, and the next open rebuilds from the old index. Opening an
index that already has version 7 takes no write lock.

`open` refuses an index from a newer version and leaves it untouched. The
error names both versions. Upgrade Arrowhead to open this index.

The rebuild empties the index. The first daemon pass after an upgrade
therefore reindexes the whole vault and generates every embedding again. The
file does not shrink, because new rows reuse the pages that the dropped tables
freed.

### Upgrade every Arrowhead process together

Install the new `arrowhead` and `arrowheadd` at the same time. Then run
`arrowhead index restart`, and restart every MCP server that uses the vault.

A daemon from 0.11.6 or earlier that keeps running after the rebuild still uses
the same file, but it cannot index anything. Each note fails with
`NOT NULL constraint failed: notes.content_fingerprint` until the daemon
restarts on the new release.

Do not run 0.11.6 or earlier against a vault after 0.11.7 has opened its
index. Those releases delete and recreate `index.db` whenever its version
differs from theirs. A running 0.11.7 daemon then keeps writing to the deleted
file, logs no error, and the CLI stops seeing its updates. Restart the daemon
on 0.11.7 to recover.

## Known limits

- SQLite writes wait up to five seconds for a competing writer. Sustained
  contention can still fail a pass; reserving the writer for link refresh
  prevents snapshot-upgrade failures, not write-lock timeouts.
- `derive_note_id` trims whitespace from the whole relative path. ` Plan/Note.md`
  and `Plan/Note.md` claim the same id from different folders. A targeted pass
  compares files in one folder only, so it can keep a different file for that
  id than a full pass keeps. The next full pass stores the full pass's choice.
- A metrics record that moves from one live file to another in a single batch
  fails with a unique-id error when the gaining file is written first. The next
  pass that reindexes the gaining file stores the record.
- Paths inside `.arrowhead` never reach the planner. A metrics root inside
  `.arrowhead` updates on full passes only.
- A watcher that drops events without a rescan notice leaves those paths stale
  until a later event covers them or the next full pass runs.
