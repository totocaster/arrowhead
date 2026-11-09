# Embedding freshness contract

`arrowhead-core` stores one vector per note in the sqlite-vec table
`note_embeddings`, next to the text index. This document states when a stored
vector counts as current, what an indexing acknowledgement promises about it,
and how the indexer repairs a vector that fell behind its note.

## A vector is current only for its note's index generation

Every text reindex stamps the note row with a new `notes.indexed_at` value.
The indexer computes the note's vector in the same step and stores it with
the same value in `EmbeddingRecord::indexed_at`. That shared timestamp is the
note's index generation.

A vector is current when its `indexed_at` equals the note row's `indexed_at`
and its `model` matches the active preset. Every other state is stale:

| Stored state | Cause |
| --- | --- |
| No vector for the note | A new note, an index built with embeddings disabled, or a rebuilt vector table. |
| A vector with a different `indexed_at` | A text reindex that committed no new vector: an edit indexed with embeddings disabled, a failed vector flush, or a crash between the text commit and the vector flush. |

`EmbeddingStore::stale_note_ids` returns the stale notes among a set of note
rows. It compares for equality, not for "newer than", so a wall clock that
steps backwards cannot make an old vector look current.

## What an indexing acknowledgement promises

The index writer commits each note's text row in its own transaction. It then
buffers the note's vector and commits vectors in batches of
`EMBEDDING_FLUSH_BATCH` (64). The writer acknowledges a note to its indexing
task after the text commit, before the note's vector batch commits.

When a note is acknowledged as indexed, one of these states is on disk:

- The note's vector is committed with the note's generation.
- The note's vector is missing or carries an older generation. This mismatch
  is the durable repair marker. It survives a process crash and a reopen,
  because both timestamps live in the same SQLite file.

The index runs in WAL mode with `synchronous=NORMAL`, so a power loss can roll
back the most recent commits. Recovery keeps the commits in order, and the
text commit always precedes its vector commit. A rolled-back vector commit
leaves the marker in place. A rolled-back text commit restores the note's
previous row. That row is either older than the file, so the next pass
reindexes the note, or it already met this contract.

A pass (`Indexer::index_all`, `Indexer::reindex_paths`, or
`Indexer::index_note`) returns `Ok` only after its final vector batch commits.
A failed batch in the middle of a pass is reported as an error for the note
whose write triggered the flush, and the writer retries the batch at the next
flush. A failed final batch fails the pass. In both cases the committed text
rows keep their notes marked stale until a vector commits.

## When the indexer repairs a stale vector

At the start of each pass with embeddings enabled, the indexer reads every
vector generation in one scan. Each stale note in the pass is reindexed even
when its file is unchanged, so its text row and its vector get a new shared
generation.

- `Indexer::index_all`, which the daemon runs at startup, repairs every stale
  note in the vault.
- `Indexer::reindex_paths`, which handles watcher batches, repairs stale notes
  among the paths in the batch only. A note left stale by a failed watcher
  batch is repaired by its next edit or by the next full pass.
- A pass with embeddings disabled writes text rows only and leaves the old
  vectors stale. The first pass after embeddings are enabled again re-embeds
  those notes.

Until the repair runs, semantic search can still rank a stale note by its old
vector. The note id is live, and the result's title, path, and preview come
from the current text row.

## A model change rebuilds the vector table

The `embedding_metadata` row stores the preset identifier, the model
repository, and the vector dimension. `EmbeddingStore::bootstrap` drops and
recreates `note_embeddings` when any of the three differs from the active
`EmbeddingDescriptor`, and it reports `model_changed`. Every note is then
stale, and the next full pass re-embeds the vault.

To change the model behind a preset, also change the preset's string in
`EmbeddingPreset::repository`. Otherwise existing indexes keep vectors from
the old model.

## Search never returns a vector without a note row

Releases before v0.11.6 could remove a note and keep its vector.
`EmbeddingStore::bootstrap` deletes every vector whose `note_id` has no row in
`notes`. `EmbeddingStore::search` also skips such vectors, so semantic search,
hybrid search, and related-note lookups return only note ids that have a text
index row.

The skip happens after the nearest-neighbour lookup. A vector without a note
row that appears after bootstrap, for example from an older daemon that still
runs against the same index, can take a result slot until the next bootstrap.

## Compatibility

- The freshness rule uses timestamps already stored since v0.3.0, so the rule
  itself does not invalidate current vectors. Version 0.11.7 also introduces
  [schema version 7](incremental_indexing.md#schema-version-7) for content
  fingerprints. That schema upgrade rebuilds the index and regenerates all
  embeddings on the first full pass.
- Subsequent full passes re-embed only new or changed notes and notes whose
  stored vectors are stale.
- The first bootstrap after an upgrade deletes vectors without a note row.
- Any code that rewrites `notes.indexed_at` must also store a vector with the
  same value, or the note is reported stale and re-embedded on the next pass.

## Cost of the stale check

sqlite-vec has no index on `note_id`, so any lookup by note scans the whole
vector table. The previous presence check ran one such scan for every note
in a pass. The stale check reads all generations in one scan per pass, so a
watcher batch with embeddings enabled pays one scan. Its cost grows with the
number of stored vectors.
