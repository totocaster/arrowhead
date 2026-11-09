use std::{
    path::{Path, PathBuf},
    time::Duration,
};

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
            let root = root_dir.clone();
            let mut watcher = RecommendedWatcher::new(
                move |res: notify::Result<Event>| handle_event(res, &root, &sender),
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
            let root = root_dir.clone();
            let mut watcher = PollWatcher::new(
                move |res: notify::Result<Event>| handle_event(res, &root, &sender),
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

fn handle_event(result: notify::Result<Event>, root: &Path, sender: &Sender<Vec<PathBuf>>) {
    match result {
        Ok(event) => {
            let rescan = event.need_rescan();
            let mut paths = event.paths;
            if rescan {
                // The platform lost events under these paths. inotify names none
                // when its queue overflows, which leaves the whole vault in doubt.
                if paths.is_empty() {
                    paths.push(root.to_path_buf());
                }
                warn!(paths = ?paths, "filesystem watcher lost events; reconciling these paths");
            }
            if paths.is_empty() {
                return;
            }

            if let Err(err) = sender.blocking_send(paths) {
                warn!(error = %err, "dropped filesystem event because queue is closed");
            }
        }
        Err(err) => {
            error!(error = %err, "filesystem watcher reported an error");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{EventKind, Flag};
    use tokio::sync::mpsc;

    #[test]
    fn rescan_without_paths_reconciles_the_whole_vault() {
        let (sender, mut receiver) = mpsc::channel(4);
        let root = PathBuf::from("/vault");

        handle_event(
            Ok(Event::new(EventKind::Other).set_flag(Flag::Rescan)),
            &root,
            &sender,
        );

        assert_eq!(receiver.try_recv().expect("rescan forwarded"), vec![root]);
    }

    #[test]
    fn other_events_without_paths_are_dropped() {
        let (sender, mut receiver) = mpsc::channel(4);

        handle_event(Ok(Event::new(EventKind::Any)), Path::new("/vault"), &sender);

        assert!(receiver.try_recv().is_err());
    }
}
