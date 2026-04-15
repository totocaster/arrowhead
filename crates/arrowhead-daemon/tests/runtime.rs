#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use arrowhead_core::{
    ActivityState, DaemonStatus, StatusFrame,
    sqlite::{IndexDatabase, NoteIndexState},
};
use arrowhead_daemon::{DaemonRuntimeBuilder, WatcherStrategy, status_stream};
use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use tempfile::TempDir;
use tokio::{
    sync::Mutex as AsyncMutex,
    time::{Instant, sleep},
};

static TEST_LOG_MUTEX: Lazy<AsyncMutex<()>> = Lazy::new(|| AsyncMutex::new(()));

#[tokio::test(flavor = "multi_thread")]
async fn reindex_updates_status_with_poll_watcher() -> Result<()> {
    let _log_guard = TEST_LOG_MUTEX.lock().await;
    let (temp_dir, vault_root) = prepare_vault()?;

    let handle = DaemonRuntimeBuilder::new(&vault_root)
        .disable_embeddings()
        .watcher_strategy(WatcherStrategy::Poll {
            interval: Duration::from_millis(50),
        })
        .spawn()
        .await?;

    let db = handle.database();
    let note_id = "Minimal Note";
    let initial_state = wait_for_note_state(db.clone(), note_id).await?;
    let initial_status = load_status(handle.status_path())?.expect("status file missing");

    let note_path = vault_root.join("Minimal Note.md");
    let mut content = fs::read_to_string(&note_path)?;
    sleep(Duration::from_millis(1100)).await;
    content.push_str("\n\nUpdated content from watcher test.\n");
    fs::write(&note_path, content)?;

    let updated_state =
        wait_for_updated_note_state(db.clone(), note_id, initial_state.indexed_at).await?;
    assert!(
        updated_state.indexed_at > initial_state.indexed_at,
        "note should be reindexed after modification"
    );

    let updated_status =
        wait_for_status_update(handle.status_path(), initial_status.updated_at).await?;
    assert_eq!(updated_status.activity.state, ActivityState::Idle);
    assert_eq!(updated_status.error_notes, 0);
    assert!(
        updated_status.indexed_notes >= 1,
        "status should report indexed notes"
    );

    let log_path = handle
        .status_path()
        .parent()
        .and_then(Path::parent)
        .expect("status path should live under .arrowhead/daemon")
        .join("logs")
        .join("daemon.log");

    handle.shutdown().await?;

    if let Ok(log_contents) = fs::read_to_string(&log_path) {
        assert!(
            log_contents.contains("watcher resolved note ids for reindex"),
            "daemon log should record watcher target resolution\n{}",
            log_contents
        );
        assert!(
            log_contents.contains("reindexed note from targeted paths"),
            "daemon log should record note-level reindexing\n{}",
            log_contents
        );
    }

    drop(temp_dir);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn control_socket_status_and_shutdown() -> Result<()> {
    let _log_guard = TEST_LOG_MUTEX.lock().await;
    let (temp_dir, vault_root) = prepare_vault()?;

    let handle = DaemonRuntimeBuilder::new(&vault_root)
        .disable_embeddings()
        .watcher_strategy(WatcherStrategy::Poll {
            interval: Duration::from_millis(50),
        })
        .spawn()
        .await?;

    wait_for_socket(handle.socket_path()).await?;
    let mut attempts = 0;
    loop {
        let status = handle.request_status().await?;
        if status.activity.state == ActivityState::Idle {
            break;
        }
        attempts += 1;
        if attempts > 300 {
            panic!(
                "daemon failed to reach idle state; last observed status {:?}",
                status.activity.state
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    handle.shutdown().await?;
    drop(temp_dir);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn status_stream_emits_frames() -> Result<()> {
    let _log_guard = TEST_LOG_MUTEX.lock().await;
    let (temp_dir, vault_root) = prepare_vault()?;

    let handle = DaemonRuntimeBuilder::new(&vault_root)
        .disable_embeddings()
        .watcher_strategy(WatcherStrategy::Poll {
            interval: Duration::from_millis(50),
        })
        .spawn()
        .await?;

    wait_for_socket(handle.socket_path()).await?;

    let mut stream = status_stream(handle.socket_path()).await?;

    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .context("timed out waiting for first status frame")??
        .context("daemon closed stream before emitting a frame")?;

    assert!(
        matches!(
            first.status.activity.state,
            ActivityState::Idle
                | ActivityState::Indexing
                | ActivityState::Downloading
                | ActivityState::Removing
        ),
        "unexpected activity state {:?}",
        first.status.activity.state
    );

    let note_path = vault_root.join("Minimal Note.md");
    let mut content = fs::read_to_string(&note_path)?;
    sleep(Duration::from_millis(1100)).await;
    content.push_str("\n\nStreaming status test update.\n");
    fs::write(&note_path, content)?;

    let indexing_frame = expect_status_frame(
        &mut stream,
        "follow-up status frame",
        Duration::from_secs(12),
        |state| matches!(state, ActivityState::Indexing),
    )
    .await?;

    assert!(
        indexing_frame.emitted_at >= first.emitted_at,
        "status frames should be monotonic"
    );
    assert!(
        matches!(
            indexing_frame.status.activity.state,
            ActivityState::Indexing
        ),
        "expected indexing state after modifying a note, got {:?}",
        indexing_frame.status.activity.state
    );

    let idle_frame = expect_status_frame(
        &mut stream,
        "idle status frame",
        Duration::from_secs(12),
        |state| matches!(state, ActivityState::Idle),
    )
    .await?;
    assert!(
        matches!(idle_frame.status.activity.state, ActivityState::Idle),
        "expected daemon to return to idle state after processing watcher batch"
    );

    handle.shutdown().await?;
    drop(temp_dir);

    Ok(())
}

async fn expect_status_frame<F>(
    stream: &mut arrowhead_daemon::StatusStream,
    stage: &'static str,
    timeout: Duration,
    mut predicate: F,
) -> Result<StatusFrame>
where
    F: FnMut(&ActivityState) -> bool,
{
    let deadline = Instant::now() + timeout;

    loop {
        let now = Instant::now();
        if now >= deadline {
            bail!("timed out waiting for {stage}");
        }
        let remaining = deadline.duration_since(now);
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Ok(Some(frame))) => {
                if predicate(&frame.status.activity.state) {
                    return Ok(frame);
                }
            }
            Ok(Ok(None)) => bail!("daemon closed stream before emitting {stage}"),
            Ok(Err(err)) => return Err(err),
            Err(_) => bail!("timed out waiting for {stage}"),
        }
    }
}

fn prepare_vault() -> Result<(TempDir, PathBuf)> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/test-vault");
    let temp_dir = tempfile::tempdir().context("failed to create temp vault dir")?;
    let vault_root = temp_dir.path().to_path_buf();
    copy_recursive(fixture.as_path(), vault_root.as_path())?;
    Ok((temp_dir, vault_root))
}

fn copy_recursive(src: &Path, dst: &Path) -> Result<()> {
    for entry in fs::read_dir(src).with_context(|| format!("failed to read {}", src.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            fs::create_dir_all(&target)?;
            copy_recursive(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target)?;
        } else if file_type.is_symlink() {
            let target_path = fs::read_link(entry.path())?;
            std::os::unix::fs::symlink(target_path, &target)?;
        }
    }
    Ok(())
}

async fn wait_for_note_state(
    database: Arc<IndexDatabase>,
    note_id: &str,
) -> Result<NoteIndexState> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = query_note_state(database.clone(), note_id.to_string()).await?;
        if let Some(state) = state {
            return Ok(state);
        }
        if Instant::now() >= deadline {
            bail!("note state for {} not available", note_id);
        }
        sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_updated_note_state(
    database: Arc<IndexDatabase>,
    note_id: &str,
    previous_indexed_at: DateTime<Utc>,
) -> Result<NoteIndexState> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = query_note_state(database.clone(), note_id.to_string()).await?;
        if let Some(state) = state {
            if state.indexed_at > previous_indexed_at {
                return Ok(state);
            }
        }
        if Instant::now() >= deadline {
            bail!("note {} was not reindexed before timeout", note_id);
        }
        sleep(Duration::from_millis(100)).await;
    }
}

async fn query_note_state(
    database: Arc<IndexDatabase>,
    note_id: String,
) -> Result<Option<NoteIndexState>> {
    tokio::task::spawn_blocking(move || database.note_state(&note_id))
        .await
        .context("note_state task panicked")?
        .context("failed to query note state")
}

fn load_status(path: &Path) -> Result<Option<DaemonStatus>> {
    DaemonStatus::load_from_path(path)
}

async fn wait_for_status_update(path: &Path, previous: DateTime<Utc>) -> Result<DaemonStatus> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match load_status(path)? {
            Some(status) if status.updated_at > previous => return Ok(status),
            Some(_) | None => {}
        }

        if Instant::now() >= deadline {
            return Err(anyhow!(
                "status file {} was not updated before timeout",
                path.display()
            ));
        }

        sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_socket(path: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "control socket {} was not created before timeout",
                path.display()
            ));
        }
        sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}
