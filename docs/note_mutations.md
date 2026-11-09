# Note mutation contract

`arrowhead-core` owns every change to a note file. The CLI commands
`arrowhead notes create`, `update`, and `delete`, and the MCP tools
`notes_create`, `notes_update`, and `notes_delete`, call these `Vault` methods
and never write or delete note files themselves:

| Method | Effect |
| --- | --- |
| `Vault::create_note` | Creates the note file and any missing parent folders. Fails if any entry already exists at the note path. |
| `Vault::write_note` | Creates or truncates the note file and writes the new content. Creates missing parent folders. |
| `Vault::delete_note` | Unlinks the note file, then removes the parent folders that the delete left empty. Returns those folders relative to the vault root, deepest first. |

All three methods return `vault::NoteMutationError`.

## Accepted note ids

A note id maps to `<id>.md` below the vault root. `Vault::relative_path_from_id`
rejects an id that is absolute or contains `..`. It also rejects an id inside
`.arrowhead`, `.obsidian`, the attachments folder, or an ignored folder. The
error is `NoteMutationError::InvalidId`.

## Symbolic links are never followed

A note change fails with `NoteMutationError::Symlink` when any folder on the
note path is a symbolic link. It also fails when the note file itself is a
symbolic link, including a broken one. The error names the symlink relative to
the vault root. The rule covers every target: a symlink that points outside the
vault, inside the vault, or nowhere.

Full vault inventory also skips symlinks. The mutation rule matches that
inventory policy and stops an alias such as `Notes/cfg -> ../.obsidian` from
reaching folders that the note id rules reject. This change does not harden
note reads or the incremental indexer's path handling.

The vault root is the exception. The root may be a symlink or sit below one,
because `Vault::new` resolves it to a canonical path when the vault opens.

## How the check holds up against concurrent changes

The rule is enforced by the system calls that change the files, not by a
separate check that runs first. On Unix, each method opens the vault root and
then opens each folder on the note path relative to its parent folder's handle,
with `O_NOFOLLOW`. The final step runs relative to the innermost folder handle:

- `create_note` opens the file with `O_CREAT | O_EXCL | O_NOFOLLOW`.
- `write_note` opens the file with `O_CREAT | O_TRUNC | O_NOFOLLOW`.
- `delete_note` calls `unlinkat`, and removes empty folders with
  `unlinkat(AT_REMOVEDIR)` against the folder handles it already holds.

If another process replaces a folder with a symlink between two of these calls,
the next call refuses the symlink or keeps working on the folder it already
opened. The replacement symlink cannot redirect the operation to its target.

## Delete and empty folders

`delete_note` removes a parent folder only when the kernel reports it empty. It
stops at the first folder that is not empty or cannot be removed. The note is
already gone at that point, so a folder that stays in place does not fail the
delete. The vault root is never removed.

## Errors

| Variant | Cause | MCP error |
| --- | --- | --- |
| `InvalidId` | The note id is rejected, as described in "Accepted note ids". | invalid params |
| `InvalidMetadata` | The metadata cannot be written as YAML frontmatter. | invalid params |
| `AlreadyExists` | `create_note` found a non-symlink entry at the note path. | invalid params |
| `NotFound` | `delete_note` found no note file. | invalid params |
| `Symlink` | A symlink is on the note path. | invalid params |
| `Io` | The filesystem rejected the change. | internal |

## Platforms

Unix platforms, including macOS and Linux, use the descriptor-relative calls
above through the `rustix` crate. On platforms other than Unix, every note
change fails with an `Io` error of kind `Unsupported`. Arrowhead does not fall
back to path-based calls that follow symlinks.

## Out of scope

Metrics file operations are separate from this contract and still follow
symlinks. The metrics write lock coordinates concurrent writers but does not
provide path containment.

These risks are tracked in
[issue #24](https://github.com/totocaster/arrowhead/issues/24) and are not
covered by this contract:

- `write_note` truncates the file in place. A note that is a hard link to a
  file outside the vault changes that file too.
- Note id rules compare paths by exact case. On a case-insensitive filesystem,
  `.Obsidian/app` reaches `.obsidian`.
- A process with write access outside the vault can move a folder out of the
  vault while a change is in progress. The change then lands in the moved
  folder.
