# Vault write containment

This page describes the rules that every note and metrics file change in a
vault follows. `arrowhead-core` enforces them in
`crates/arrowhead-core/src/vault/contained_fs.rs`, so the CLI and the MCP server
get the same behavior. The [note mutation contract](note_mutations.md) covers
the note methods themselves.

## Changes that follow these rules

| Change | Core entry point | CLI | MCP tool |
| --- | --- | --- | --- |
| Create a note | `Vault::create_note` | `notes create` | `notes_create` |
| Update a note | `Vault::write_note` | `notes update` | `notes_update` |
| Delete a note | `Vault::delete_note` | `notes delete` | `notes_delete` |
| Add a metric row | `MetricsMutationService::create` | `metrics create` | `metrics_create` |
| Change a metric row | `MetricsMutationService::update` | `metrics update` | `metrics_update` |
| Delete a metric row | `MetricsMutationService::delete` | `metrics delete` | `metrics_delete` |
| Create a metrics file | `MetricsMutationService::create_file` | `metrics files create` | `metrics_create_file` |
| Rename a metrics file | `MetricsMutationService::rename_file` | `metrics files rename` | `metrics_rename_file` |
| Delete a metrics file | `MetricsMutationService::delete_file` | `metrics files delete` | `metrics_delete_file` |
| Assign missing metric ids | `MetricsMutationService::assign_missing_ids` | `metrics assign-missing-ids` | none |

For metrics, the rules also cover the reads that feed a change: the row lookup
for `update` and `delete`, the existing rows that `create` appends to, the id
scan, the rows that `assign-missing-ids` rewrites, the row count that
`files delete` reports, and the index refresh after each change.

## Symbolic links are never followed

Arrowhead opens the vault root, then opens each folder on the path relative to
its parent folder's handle with `O_NOFOLLOW`. The final step runs relative to
the innermost folder handle. A symlink anywhere on the path stops the change,
whether it points outside the vault, inside it, or nowhere. The kernel refuses
the symlink when Arrowhead uses the name, so a folder swapped for a symlink
between two steps cannot redirect the change.

Three paths are exceptions:

- The vault root may be a symlink or sit below one. `Vault::new` resolves it to
  a canonical path.
- The Arrowhead folder, `.arrowhead`, may be a symlink, as it may for the index
  database.
- The metrics write lock file, `.arrowhead/metrics.lock`, may not be a symlink.
  Arrowhead opens it with `O_NOFOLLOW`, so a planted link cannot create a file
  outside the Arrowhead folder.

A metrics root that is a symlink, such as `Metrics -> ~/elsewhere`, makes every
metrics change fail. Before this rule, `metrics create` wrote a new file outside
the vault through such a link, `metrics update` rewrote an outside file, and
`metrics delete` removed one. A linked file such as
`Metrics/Alias.metrics.ndjson` made `metrics create` copy the outside file's
rows into a regular vault file. `metrics files delete` through a linked folder
removed the outside file and the outside folder it left empty.

## Changes need the exact spelling

Every existing folder and file on a change's path must be spelled exactly as
its parent folder lists it. Arrowhead compares the requested name with the
stored names in the folder listing, byte for byte. Missing folders are created
with the spelling the caller gave.

The rule exists because a case-insensitive or normalization-insensitive
filesystem, such as APFS on macOS, opens an entry by another spelling of its
name. `.Obsidian` reaches `.obsidian`, `PRIVATE` reaches an ignored `Private`
folder, `attachmentſ` (with a long s) reaches `attachments`, and a decomposed
`è` reaches a precomposed one. The path rules compare names exactly, so these
spellings bypassed them. With the rule, the path that changes is the path that
the rules checked and that inventory lists.

Reads skip the spelling check, including metrics reads that feed a change.
The operation that writes, renames, or deletes the file checks its spelling
before changing it. All of these reads still refuse symbolic links.

A change refused for spelling fails before Arrowhead creates any folder or
file. Notes fail with `NoteMutationError::Misspelled`, which names the
misspelled path and, when the listing shows it, the stored spelling:

```text
refusing to change note PRIVATE/Leak: the vault spells `PRIVATE` as `Private`, and note changes need the exact spelling
```

Metrics changes fail with the same explanation:

```text
refusing to write metrics file `Metrics/SUB/Data.metrics.ndjson`: the vault spells `Metrics/SUB` as `Metrics/sub`, and metrics changes need the exact spelling
```

When either spelling contains non-ASCII characters, the message also prints
both spellings as code points, because spellings that differ only in Unicode
composition look identical.

Exclusive creates fail with `AlreadyExists` when the final filename already
exists, including when a case variant reaches that file. A differently spelled
parent folder still fails with `Misspelled`. Neither refusal changes the file.

### What the spelling rule changes for callers

- On macOS, a note id that differs from the stored name only in case, such as
  `projects/Plan` for `Projects/Plan.md`, used to change the existing note.
  Arrowhead now refuses it. Use the id that inventory lists, for example from
  `arrowhead notes list` or the MCP `notes_list` tool.
- A note id maps to `<id>.md`. A note stored with another extension case, such
  as `Plan.MD`, needs the extension in the id: `Plan.MD`.
- A metrics root configured as `Metrics` for a folder stored as `metrics` makes
  every metrics change fail on macOS. Rename the folder, or set the metrics
  root to `metrics`. On Linux, Arrowhead never found such a folder.
- HFS+ volumes store names decomposed. On HFS+, a name typed with precomposed
  accents is refused. Use the id that inventory lists.

## Note ids follow the inventory rules

`Vault::relative_path_from_id` rejects a note id whose file would sit in a
folder that full inventory skips:

- Arrowhead's folder, `.arrowhead`.
- Any hidden folder, which is a folder whose name starts with `.`. This
  includes `.obsidian` and `.trash`.
- The attachments folder that the vault settings name.
- Any folder named `attachments`, in any ASCII case.
- Any ignored path.

The error is `NoteMutationError::InvalidId`, and nothing on disk changes.
Before this rule, Arrowhead created notes such as `.trash/Ghost` and
`Sub/attachments/Ghost`, and then the MCP server reported "not found in vault"
because inventory never lists them.

An id that names a file at the vault root, such as `Private` for `Private.md`,
is accepted, because inventory lists that file.

## Updates replace the file

`Vault::write_note` and every metrics rewrite write the new contents to a
temporary file in the target's folder, named `.arrowhead.<pid>.<random>.tmp`.
Arrowhead then renames the temporary file over the target with `renameat`.

- The old file's inode is never written. A hard link to the old file, inside or
  outside the vault, keeps the old contents. Arrowhead logs a warning when the
  note it replaced had other links.
- If a step fails before the rename, the old file stays whole and Arrowhead
  deletes the temporary file.
- The old file must open for writing, as it had to for the earlier in-place
  write. A read-only note stays read-only and unchanged.
- The folder must be writable, because the temporary file is created there. A
  writable note in a read-only folder can no longer be updated.

The replacement copies this metadata from the old file:

- The permission bits, without the set-user-ID and set-group-ID bits. Writing
  a file clears those bits, so an in-place write would not have kept them
  either.
- The extended attributes. On Linux, POSIX ACLs are extended attributes, so
  they carry over too.
- The owner and the group, where the OS lets the process set them.
- The creation time, on macOS.

If Arrowhead cannot copy an extended attribute, the owner, the group, or the
creation time, it logs a warning and the update still succeeds. This choice
keeps updates working when, for example, a file belongs to another user.

The replacement does not keep these:

- The inode number.
- macOS ACLs and file flags set with `chflags`.
- The creation time on Linux. `created_at` for an updated note changes there.

Arrowhead does not call `fsync`. A power loss shortly after an update can lose
the update. A process killed between creating the temporary file and the rename
leaves that file behind. Inventory and metrics discovery ignore it.

## Metrics files

- The write lock is unchanged. Each metrics change holds the per-vault `flock`
  on `.arrowhead/metrics.lock` from the row lookup through the index refresh.
- `files rename` checks both the source and the destination path. The source
  must be a regular file, and the destination must not exist.
- After a metrics file is deleted or renamed, Arrowhead removes the folders
  left empty, through the folder handles it already holds. It never removes the
  metrics root or the folders above it. A folder that cannot be removed stays,
  and the change still succeeds.
- The index refresh reads the modification time and then the rows from one open
  handle. If another program edits the file during the refresh, the stored time
  is older than the content, and the next index pass reads the file again.

## Errors

| Cause | Note error | Metrics error message starts with | MCP error |
| --- | --- | --- | --- |
| Id or path outside the path rules | `InvalidId` | `metrics file ... must live under` | invalid params |
| Symlink on the path | `Symlink` | `refusing to <action> metrics file` | invalid params |
| Another spelling of an existing entry | `Misspelled` | `refusing to <action> metrics file` | invalid params |
| Entry already exists | `AlreadyExists` | `metrics file ... already exists` | invalid params |
| Entry missing | `NotFound` | `metrics file ... does not exist` | invalid params |
| Filesystem error | `Io` | `failed to <action> metrics file` | internal |

## Limits

- Arrowhead implements these operations only on Unix, including macOS and
  Linux. On other platforms, note and metrics changes fail with an error of
  kind `Unsupported`. Arrowhead does not fall back to path-based calls that
  follow symlinks.
- A process with write access outside the vault can move a folder out of the
  vault while a change is in progress. The change then lands in the moved
  folder.
- `files rename` checks that the destination is missing, then renames. A
  program that is not Arrowhead can create the destination between the two
  steps, and the rename replaces it. Arrowhead writers wait for each other
  through the write lock.
- Note reads, metrics discovery, and the indexer are outside this contract.
  `arrowhead metrics files` and the indexer still list metrics files through a
  symlinked metrics root.
- CI runs on Linux with a case-sensitive filesystem, so the spelling refusals
  run only in local test runs on a case-insensitive filesystem such as APFS.
