use std::{path::PathBuf, time::Duration};

use anyhow::Result;
use notify::{Config, Event, PollWatcher, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, warn};

/// Strategy used to construct a filesystem watcher.
#[derive(Debug, Clone)]
pub enum WatcherStrategy {
    /// Use the platform recommended watcher (kqueue/inotify/fsevents).
    Recommended,
    /// Use the poll watcher with a specific interval.
    Poll {
        /// How frequently to poll for filesystem changes.
        interval: Duration,
    },
}

impl Default for WatcherStrategy {
    fn default() -> Self {
        Self::Recommended
    }
}

/// Handle that keeps the underlying watcher alive for the runtime lifetime.
#[allow(dead_code)]
pub enum WatcherHandle {
    Recommended(RecommendedWatcher),
    Poll(PollWatcher),
}

/// Start a watcher rooted at `root_dir`, delivering note candidate paths to the
/// supplied sender. Paths are batched per event to preserve grouping.
pub fn start_watcher(
    strategy: WatcherStrategy,
    root_dir: PathBuf,
    sender: Sender<Vec<PathBuf>>,
) -> Result<WatcherHandle> {
    let config = Config::default()
        .with_compare_contents(false)
        .with_poll_interval(match strategy {
            WatcherStrategy::Poll { interval } => interval,
            WatcherStrategy::Recommended => Duration::from_secs(2),
        });

    match strategy {
        WatcherStrategy::Recommended => {
            debug!(
                root = %root_dir.display(),
                "starting recommended filesystem watcher"
            );
            let mut watcher = RecommendedWatcher::new(
                move |res: notify::Result<Event>| handle_event(res, sender.clone()),
                config,
            )
            .map_err(|err| anyhow::anyhow!("failed to build filesystem watcher: {err}"))?;
            watcher
                .watch(&root_dir, RecursiveMode::Recursive)
                .map_err(|err| anyhow::anyhow!("failed to watch vault: {err}"))?;
            Ok(WatcherHandle::Recommended(watcher))
        }
        WatcherStrategy::Poll { interval: _ } => {
            debug!(
                root = %root_dir.display(),
                "starting poll-based filesystem watcher"
            );
            let mut watcher = PollWatcher::new(
                move |res: notify::Result<Event>| handle_event(res, sender.clone()),
                config,
            )
            .map_err(|err| anyhow::anyhow!("failed to build poll watcher: {err}"))?;
            watcher
                .watch(&root_dir, RecursiveMode::Recursive)
                .map_err(|err| anyhow::anyhow!("failed to watch vault: {err}"))?;
            Ok(WatcherHandle::Poll(watcher))
        }
    }
}

fn handle_event(result: notify::Result<Event>, sender: Sender<Vec<PathBuf>>) {
    match result {
        Ok(event) => {
            if event.paths.is_empty() {
                return;
            }

            let paths = event.paths;
            if let Err(err) = sender.blocking_send(paths) {
                warn!(error = %err, "dropped filesystem event because queue is closed");
            }
        }
        Err(err) => {
            error!(error = %err, "filesystem watcher reported an error");
        }
    }
}
