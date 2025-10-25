#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use arrowhead_core::{
    ActivityState, DeamonStatus,
    sqlite::{IndexDatabase, NoteIndexState},
};
use arrowhead_deamon::{DeamonRuntimeBuilder, WatcherStrategy};
use chrono::{DateTime, Utc};
use tempfile::TempDir;
use tokio::time::{Instant, sleep};

#[tokio::test(flavor = "multi_thread")]
async fn reindex_updates_status_with_poll_watcher() -> Result<()> {
    let (temp_dir, vault_root) = prepare_vault()?;

    let handle = DeamonRuntimeBuilder::new(&vault_root)
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

    handle.shutdown().await?;
    drop(temp_dir);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn control_socket_status_and_shutdown() -> Result<()> {
    let (temp_dir, vault_root) = prepare_vault()?;

    let handle = DeamonRuntimeBuilder::new(&vault_root)
        .disable_embeddings()
        .watcher_strategy(WatcherStrategy::Poll {
            interval: Duration::from_millis(50),
        })
        .spawn()
        .await?;

    wait_for_socket(handle.socket_path()).await?;
    let status = handle.request_status().await?;
    assert_eq!(status.activity.state, ActivityState::Idle);

    handle.shutdown().await?;
    drop(temp_dir);

    Ok(())
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

fn load_status(path: &Path) -> Result<Option<DeamonStatus>> {
    DeamonStatus::load_from_path(path)
}

async fn wait_for_status_update(path: &Path, previous: DateTime<Utc>) -> Result<DeamonStatus> {
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
