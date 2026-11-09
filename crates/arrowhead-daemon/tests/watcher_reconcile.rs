#![cfg(unix)]
//! The platform watcher reports directory moves and edits without per-note events;
//! the running daemon must still converge on what a full pass would index.

use std::{
    fs,
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use arrowhead_core::{GraphService, sqlite::IndexDatabase};
use arrowhead_daemon::{DaemonHandle, DaemonRuntimeBuilder, WatcherStrategy};
use once_cell::sync::Lazy;
use tempfile::TempDir;
use tokio::{
    sync::Mutex as AsyncMutex,
    time::{Instant, sleep},
};

/// Daemon logging is process-global, so runtimes in this binary take turns.
static DAEMON_MUTEX: Lazy<AsyncMutex<()>> = Lazy::new(|| AsyncMutex::new(()));

const WAIT: Duration = Duration::from_secs(20);

struct Running {
    vault: TempDir,
    outside: TempDir,
    handle: DaemonHandle,
    database: Arc<IndexDatabase>,
}

impl Running {
    async fn start(files: &[(&str, &str)], ready_note: &str) -> Result<Self> {
        let vault = tempfile::tempdir().context("temp vault")?;
        for (relative, content) in files {
            write(&vault.path().join(relative), content)?;
        }
        let handle = DaemonRuntimeBuilder::new(vault.path())
            .disable_embeddings()
            .watcher_strategy(WatcherStrategy::Recommended)
            .spawn()
            .await?;
        let database = handle.database();
        let running = Self {
            vault,
            outside: tempfile::tempdir().context("temp outside dir")?,
            handle,
            database,
        };
        running
            .wait_until("initial full pass", |ids| {
                ids.iter().any(|id| id == ready_note)
            })
            .await?;
        Ok(running)
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.vault.path().join(relative)
    }

    async fn wait_until(&self, what: &str, done: impl Fn(&[String]) -> bool) -> Result<()> {
        wait_for(what, done, || {
            let database = Arc::clone(&self.database);
            async move { tokio::task::spawn_blocking(move || database.list_note_ids()).await? }
        })
        .await
    }

    async fn stop(self) -> Result<()> {
        self.handle.shutdown().await
    }
}

async fn wait_for<F, Fut>(
    what: &str,
    done: impl Fn(&[String]) -> bool,
    mut observe: F,
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Vec<String>>>,
{
    let deadline = Instant::now() + WAIT;
    loop {
        let observed = observe().await?;
        if done(&observed) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for {what}; last observed {observed:?}");
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn write(path: &Path, content: &str) -> Result<()> {
    fs::create_dir_all(path.parent().context("parent dir")?)?;
    fs::write(path, content)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn directory_rename_and_move_out_reach_the_index() -> Result<()> {
    let _turn = DAEMON_MUTEX.lock().await;
    let daemon = Running::start(
        &[
            ("Projects/Alpha.md", "# Alpha\n"),
            ("Projects/Deep/Bravo.md", "# Bravo\n"),
            ("Keep.md", "# Keep\n"),
        ],
        "Projects/Deep/Bravo",
    )
    .await?;

    fs::rename(daemon.path("Projects"), daemon.path("Archive"))?;
    daemon
        .wait_until("renamed subtree", |ids| {
            ids == ["Archive/Alpha", "Archive/Deep/Bravo", "Keep"]
        })
        .await?;

    fs::rename(
        daemon.path("Archive"),
        daemon.outside.path().join("Archive"),
    )?;
    daemon
        .wait_until("subtree moved out of the vault", |ids| ids == ["Keep"])
        .await?;

    daemon.stop().await
}

#[tokio::test(flavor = "multi_thread")]
async fn nested_directory_rename_and_delete_reach_the_index() -> Result<()> {
    let _turn = DAEMON_MUTEX.lock().await;
    let daemon = Running::start(
        &[
            ("Projects/Alpha.md", "# Alpha\n"),
            ("Projects/Deep/Bravo.md", "# Bravo\n"),
            ("Projects/Deep/Deeper/Charlie.md", "# Charlie\n"),
            ("Keep.md", "# Keep\n"),
        ],
        "Projects/Deep/Deeper/Charlie",
    )
    .await?;

    fs::rename(
        daemon.path("Projects/Deep"),
        daemon.path("Projects/Shallow"),
    )?;
    daemon
        .wait_until("renamed nested folder", |ids| {
            ids == [
                "Keep",
                "Projects/Alpha",
                "Projects/Shallow/Bravo",
                "Projects/Shallow/Deeper/Charlie",
            ]
        })
        .await?;

    fs::remove_dir_all(daemon.path("Projects/Shallow"))?;
    daemon
        .wait_until("deleted nested folder", |ids| {
            ids == ["Keep", "Projects/Alpha"]
        })
        .await?;
    daemon.stop().await
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_directory_rename_reaches_the_index() -> Result<()> {
    const HEALTH: &str = r#"{"id":"01AAA","ts":"2026-04-14T08:30:00+00:00","key":"body.weight","value":105.6,"unit":"kg","source":"withings"}"#;
    const LEGACY: &str = r#"{"id":"01OLD","ts":"2025-04-14T08:30:00+00:00","key":"body.weight","value":99.0,"unit":"kg","source":"withings"}"#;
    let _turn = DAEMON_MUTEX.lock().await;
    let daemon = Running::start(
        &[
            ("Metrics/health.metrics.ndjson", HEALTH),
            ("Metrics/Old/legacy.metrics.ndjson", LEGACY),
            ("Keep.md", "# Keep\n"),
        ],
        "Keep",
    )
    .await?;
    let metric_files = |database: Arc<IndexDatabase>| async move {
        Ok(
            tokio::task::spawn_blocking(move || database.list_metric_files())
                .await??
                .into_iter()
                .map(|file| file.relative_path.to_string_lossy().into_owned())
                .collect(),
        )
    };
    wait_for(
        "initial metrics pass",
        |files| {
            files
                == [
                    "Metrics/Old/legacy.metrics.ndjson",
                    "Metrics/health.metrics.ndjson",
                ]
        },
        || metric_files(Arc::clone(&daemon.database)),
    )
    .await?;

    fs::rename(daemon.path("Metrics/Old"), daemon.path("Metrics/Archive"))?;
    wait_for(
        "renamed metrics folder",
        |files| {
            files
                == [
                    "Metrics/Archive/legacy.metrics.ndjson",
                    "Metrics/health.metrics.ndjson",
                ]
        },
        || metric_files(Arc::clone(&daemon.database)),
    )
    .await?;
    daemon.stop().await
}

#[tokio::test(flavor = "multi_thread")]
async fn directory_moved_in_is_indexed() -> Result<()> {
    let _turn = DAEMON_MUTEX.lock().await;
    let daemon = Running::start(&[("Keep.md", "# Keep\n")], "Keep").await?;

    let staged = daemon.outside.path().join("Inbox");
    write(&staged.join("New.md"), "# New\n")?;
    write(&staged.join("Sub/Deeper.md"), "# Deeper\n")?;
    fs::rename(&staged, daemon.path("Inbox"))?;

    daemon
        .wait_until("moved-in subtree", |ids| {
            ids == ["Inbox/New", "Inbox/Sub/Deeper", "Keep"]
        })
        .await?;
    daemon.stop().await
}

#[tokio::test(flavor = "multi_thread")]
async fn timestamp_preserving_edit_becomes_searchable() -> Result<()> {
    let _turn = DAEMON_MUTEX.lock().await;
    let daemon = Running::start(&[("Old.md", "oldword\n")], "Old").await?;

    let path = daemon.path("Old.md");
    let modified = fs::metadata(&path)?.modified()?;
    fs::write(&path, "newword\n")?;
    fs::File::options()
        .write(true)
        .open(&path)?
        .set_times(fs::FileTimes::new().set_modified(modified))?;

    let database = Arc::clone(&daemon.database);
    wait_for(
        "timestamp-preserving edit in search",
        |hits| hits == ["Old"],
        || {
            let database = Arc::clone(&database);
            async move {
                Ok(
                    tokio::task::spawn_blocking(move || database.search_fts("newword", 10))
                        .await??
                        .into_iter()
                        .map(|hit| hit.note_id)
                        .collect(),
                )
            }
        },
    )
    .await?;
    daemon.stop().await
}

#[tokio::test(flavor = "multi_thread")]
async fn late_target_gains_backlinks() -> Result<()> {
    let _turn = DAEMON_MUTEX.lock().await;
    let daemon = Running::start(&[("Source.md", "See [[Target]].\n")], "Source").await?;

    write(&daemon.path("Target.md"), "# Target\n")?;

    let graph = GraphService::new(Arc::clone(&daemon.database));
    wait_for(
        "backlink from Source",
        |sources| sources == ["Source"],
        || {
            let graph = graph.clone();
            async move {
                Ok(graph
                    .backlinks("Target")
                    .await?
                    .into_iter()
                    .map(|edge| edge.source)
                    .collect())
            }
        },
    )
    .await?;
    daemon.stop().await
}

#[tokio::test(flavor = "multi_thread")]
async fn notes_moved_to_trash_leave_the_index() -> Result<()> {
    let _turn = DAEMON_MUTEX.lock().await;
    let daemon = Running::start(
        &[("Doomed.md", "# Doomed\n"), ("Keep.md", "# Keep\n")],
        "Doomed",
    )
    .await?;

    fs::create_dir(daemon.path(".trash"))?;
    fs::rename(daemon.path("Doomed.md"), daemon.path(".trash/Doomed.md"))?;
    write(&daemon.path("Barrier.md"), "# Barrier\n")?;

    daemon
        .wait_until("trash move and barrier note", |ids| {
            ids == ["Barrier", "Keep"]
        })
        .await?;
    daemon.stop().await
}
