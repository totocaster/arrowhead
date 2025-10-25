# Arrowhead Deamon Planning — 2025-10

*(“Deamon” naming follows product direction.)*

## Context

- Goal: evolve Arrowhead into a deamon-backed experience where vault
  initialisation provisions a background service that keeps the index hot
  without per-command reindexing.
- Current state: CLI relies on on-demand indexing (kicking off full passes from
  `search`/`index`). No background watcher exists; incremental deletion support
  is missing in the SQLite layer and CLI always reindexes before searches.
- Constraints: Rust 1.86 toolchain, optional LanceDB feature, cross-platform
  support (macOS + Linux), maintainability & testability per rewrite spec.

## Feasibility Summary

- ✅ File watching: `notify` crate (recommended watcher on macOS/Linux) works on
  Rust 1.86. Poll-based watcher available for integration tests.
- ✅ Incremental indexing: `arrowhead-core::Indexer` already supports single-note
  reindex (`index_note`), but it rebuilds the full inventory each time. Adding a
  path-based fast path plus delete handling is tractable.
- ✅ Deamon process: CLI already uses Tokio; reusing it for a long-lived service
  fits the stack. Detaching via `std::process::Command::spawn` plus PID files is
  portable enough for v1, with follow-on work to register auto-start agents on
  macOS (`launchd`) and Linux (`systemd --user`).
- ⚠️ IPC/control plane: no existing mechanism for CLI↔deamon coordination.
  Needs a lightweight channel (Unix domain socket + JSON command envelope) or
  status files. Chosen approach must survive restarts and support status checks.
- ⚠️ CLI behaviour: commands currently self-refresh the index. We must refactor
  them to skip indexing, trust the deamon’s hot index, and fall back gracefully
  if the deamon is unhealthy.

## Implementation Plan

1. **Core Enhancements**
   - Introduce `IndexDatabase::remove_note(&str)` and `IndexDatabase::list_note_ids()`.
   - Extend `Indexer` with `reindex_paths(&[PathBuf])` and `remove_note(&str)` APIs
     that bypass full inventory rebuilds by resolving note IDs from relative paths.
   - Add a reusable `InventorySnapshot` helper (path → metadata/id map) so the
     deamon can refresh specific files cheaply, detect renames, and reconcile deletes.
   - Harden vault helpers with conversion utilities (`path_to_note_id`, `is_note_path`)
     and ensure `.arrowhead` and ignore directories are filtered consistently.
   - Ensure indexing APIs return structured outcomes (indexed/skipped/removed) so
     the deamon can emit precise status updates and errors.

2. **Arrowhead Deamon Crate**
  - New crate `arrowhead-deamon` (library + Tokio binary `arrowheadd`) that depends on
     `arrowhead-core`.
   - Responsibilities:
     - Bootstraps vault context, runs an initial full index, writes status file,
       and records operational metadata under `.arrowhead/deamon/`.
     - Starts a `notify` watcher (recursive, debounced) ignoring `.arrowhead`.
     - Queues events into a bounded channel, coalesces duplicate work, and executes
       sequential indexing/removal tasks via the new incremental APIs.
     - Persists deamon state under `.arrowhead/deamon/` (`status.json`,
       `deamon.pid`, `control.sock`, `autostart/` metadata); every artefact lives inside
       the vault for portability and backups. `status.json` reports total indexed notes,
       current error count, detailed activity status (including `idle`), download
       progress for embeddings/models, outstanding issues, and the
       `arrowheadd.log` path for diagnostics.
     - Exposes a JSON command interface over `.arrowhead/deamon/control.sock`
       (Unix domain socket, owner-only permissions) supporting `status`, `shutdown`,
       and health pings. The PID file enforces a single active deamon instance.
   - Observability:
     - Structured `tracing` routed to `.arrowhead/logs/arrowheadd.log`, using the
       same retention and rotation policy as `arrowhead.log`.
     - Rotate logs similarly to CLI, keeping separation between CLI
       (`arrowhead.log`) and background service logging.
     - Emit counters for processed events, queued work, failures, time of last
       successful index, and index staleness.
   - Auto-start:
     - During `vault init`, prompt the user before installing a per-user `launchd`
       plist (macOS) or `systemd --user` unit (Linux). Follow-up commands (`start`,
       `stop`, `cleanup`) remain non-interactive.
     - Maintain install status in `.arrowhead/deamon/autostart/` for troubleshooting,
       and ensure `vault cleanup` removes these units and marks them disabled.
     - After launching, inform the user that the first indexing pass may take time and
       direct them to `arrowhead vault status` for live progress.

3. **CLI Integration**
   - Expand `arrowhead vault` subcommands:
     - `vault init` – ensures vault directories, performs initial index, launches the
       deamon detached, registers auto-start, and persists socket/pid metadata. Prompts
       the user for auto-start consent and reminds them to allow the deamon time to
       finish the first indexing pass (monitor via `vault status`).
     - `vault status` – queries the Unix socket; if unavailable, reads `status.json`
       to report vault + deamon health, indexed-vs-error counts, current activity,
       download progress/issues, log file location, indexing lag, and auto-start status.
     - `vault start` – starts (or restarts) the deamon and verifies the socket.
     - `vault stop` – sends shutdown command, removes PID file, leaves auto-start in
       place unless flagged otherwise.
     - `vault cleanup` – stops the deamon, removes `.arrowhead` caches (index.db,
       vectors, logs, status, socket, PID, autostart metadata), and uninstalls launch
       agents while leaving raw vault notes untouched.
  - `arrowhead init` delegates to `vault init` and runs the full interactive setup
    (auto-start prompt + deamon launch). Once the control socket is ready the
    command returns immediately while `arrowheadd` completes the initial crawl in the
    background; users can monitor progress via `arrowhead vault status`. Pass
    `--no-start` to skip launching in bespoke deployments.
   - Update `search`/`notes`/future commands to:
     - Skip `ensure_index_fresh`; instead, query deamon status before operations. If
       the socket is unavailable or returns an error—or `status.json` lists outstanding
       issues—commands fail fast with guidance to run `arrowhead vault status`.
     - Continue handling vault I/O (e.g., note CRUD); rely on the watcher for
       subsequent indexing. No direct reindex calls from CLI in steady state.
   - Extend `AppConfig` with `deamon` preferences (socket path override, auto-start
     consent, last-known status) for multi-vault scenarios.

4. **Testing & Tooling**
   - Unit tests for incremental database/indexer APIs, including delete coverage and
     rename handling.
   - Integration tests using temp vaults + `notify::PollWatcher` to simulate file
     changes and assert deamon responses, status updates, and logging separation.
   - CLI smoke tests for `vault status/start/stop/cleanup`, spawning the deamon as a
     child process under tests and ensuring cleanup on panic.
   - Platform tests (behind feature flags) that validate `launchd` plist generation
     and `systemd` units compile and round-trip.
   - Ensure `cargo fmt`, `cargo clippy --all-targets --all-features`, `cargo test`
     remain green with and without `vector-lancedb`.

5. **Documentation & Rollout**
   - Update `docs/predev_synapse_rust_rewrite.md` with the deamon architecture,
     auto-start strategy, control interface, and CLI behavioural changes.
   - Amend `docs/api.md` and CLI help text to describe the new `vault` subcommands and
     the expectation that the deamon owns indexing duties (FTS + vectors).
   - Refresh `docs/dev_status.md` after implementation to capture the milestone and
     note outstanding follow-ups (e.g., Windows support, additional auto-start targets).
   - Provide migration notes for existing users (how to enable the deamon, how CLI
     falls back if it is offline, cleanup semantics).

## Phased Execution Roadmap

**Phase 1 – Core Foundations**
- Implement inventory snapshot utilities plus `IndexDatabase::remove_note` and
  `Indexer::reindex_paths/remove_note` APIs.
- Define and serialise the enriched `status.json` schema (indexed counts, errors,
  activity, download progress, issues, log pointer).
- Update CLI configuration structures to recognise deamon settings and prepare for
  socket/status consumption.
- Add unit tests covering incremental indexing paths, deletion handling, and status
  file serialisation.

**Status Schema Snapshot**
- Types added under `arrowhead-core::status`:
  - `DeamonStatus` (`version`, `updated_at`, `indexed_notes`, `error_notes`,
    `activity`, `downloads`, `issues`, `log_path`).
  - `ActivityStatus` + `ActivityState` (idle/indexing/removing/downloading/faulted).
  - `DownloadStatus` + `DownloadState` for embedding/model downloads.
  - `StatusIssue` + `IssueSeverity` for surfaced problems.
- Helpers exposed for saving/loading JSON snapshots with automatic directory
  creation and version normalisation (`save_to_path`, `load_from_path`).

**Phase 2 – Deamon Runtime**
- Create the `arrowhead-deamon` crate/binary (`arrowheadd`), build the watcher pipeline, and wire up
  status/log emissions.
- Implement the Unix socket server at `.arrowhead/deamon/control.sock` with `status`
  and `shutdown` commands, ensuring single-instance enforcement.
- Integrate logging to `.arrowhead/logs/arrowheadd.log` and ensure download
  progress + errors feed into `status.json`.

**Phase 3 – CLI Integration & Auto-start**
- Expand `arrowhead vault` subcommands (`init/status/start/stop/cleanup`) with the
  interactive auto-start prompt and fail-fast behaviour when issues are reported.
- Teach other commands (`search`, `notes`, etc.) to trust the deamon, surfacing status
  problems immediately.
- Install/remove `launchd`/`systemd --user` units, keeping records under
  `.arrowhead/deamon/autostart/` and ensuring cleanup removes them.

**Phase 4 – Verification & Documentation**
- Execute integration tests (including watcher simulations) and regression suites across
  feature combinations.
- Finalise documentation updates (spec, API, dev status, migration guidance) and ensure
  logging, status messaging, and CLI UX match the agreed behaviour.

## Open Questions / Risks

- Error reporting UX: ensure repeated watcher/indexing failures are surfaced clearly
  in `status.json` and CLI output without ever auto-pausing the deamon. The service
  must remain running until the user intervenes.

## Next Steps

1. Kick off Phase 1 by designing `InventorySnapshot` and incremental database/indexer
   APIs, plus the enriched `status.json` schema.
2. Implement deletion and path-based reindex support with accompanying unit tests.
3. Extend CLI configuration/types to capture deamon settings and consume the new status
   structure.
4. Document the status schema and Phase 1 deliverables, then proceed to Phase 2.
