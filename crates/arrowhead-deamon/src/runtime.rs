use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow};
use arrowhead_core::indexer::{Indexer, IndexerConfig};
use arrowhead_core::sqlite::IndexDatabase;
use arrowhead_core::{
    ActivityState, ActivityStatus, DeamonStatus, InventorySnapshot, IssueSeverity, StatusIssue,
    Vault, VaultConfig,
};
use tokio::{
    sync::{Mutex, broadcast, mpsc},
    task::JoinHandle,
};
use tracing::{info, warn};

use crate::control::{ControlRequest, ControlResponse, run_control_server, send_control_request};
use crate::watcher::{WatcherHandle, WatcherStrategy, start_watcher};

/// Runtime configuration produced by the builder.
pub struct DeamonConfig {
    pub(crate) vault: Arc<Vault>,
    pub(crate) database: Arc<IndexDatabase>,
    pub(crate) indexer: Arc<Indexer>,
    pub(crate) status_path: PathBuf,
    pub(crate) socket_path: PathBuf,
    pub(crate) log_path: PathBuf,
    pub(crate) event_buffer: usize,
    pub(crate) watcher_strategy: WatcherStrategy,
}

/// Builder used to construct and launch the deamon runtime.
pub struct DeamonRuntimeBuilder {
    vault_root: PathBuf,
    watcher_strategy: WatcherStrategy,
    event_buffer: usize,
    indexer_config: IndexerConfig,
}

impl DeamonRuntimeBuilder {
    /// Construct a builder targeting the supplied vault root.
    pub fn new<P: Into<PathBuf>>(vault_root: P) -> Self {
        Self {
            vault_root: vault_root.into(),
            watcher_strategy: WatcherStrategy::Recommended,
            event_buffer: 128,
            indexer_config: IndexerConfig::default(),
        }
    }

    /// Override the watcher strategy (useful for tests).
    pub fn watcher_strategy(mut self, strategy: WatcherStrategy) -> Self {
        self.watcher_strategy = strategy;
        self
    }

    /// Override the event queue capacity.
    pub fn event_buffer(mut self, capacity: usize) -> Self {
        self.event_buffer = capacity.max(16);
        self
    }

    /// Override the indexer configuration.
    pub fn indexer_config(mut self, config: IndexerConfig) -> Self {
        self.indexer_config = config;
        self
    }

    fn prepare(self) -> Result<DeamonConfig> {
        let vault_config = VaultConfig::new(self.vault_root.clone());
        let vault = Arc::new(Vault::new(vault_config)?);
        vault.ensure_arrowhead_dirs()?;

        let arrowhead_dir = vault.paths().arrowhead_dir.clone();
        std::fs::create_dir_all(&arrowhead_dir).with_context(|| {
            format!("failed to ensure arrowhead dir {}", arrowhead_dir.display())
        })?;

        let deamon_dir = arrowhead_dir.join("deamon");
        std::fs::create_dir_all(&deamon_dir)
            .with_context(|| format!("failed to ensure deamon dir {}", deamon_dir.display()))?;

        let logs_dir = vault.paths().logs_dir();
        std::fs::create_dir_all(&logs_dir)
            .with_context(|| format!("failed to ensure logs dir {}", logs_dir.display()))?;

        let status_path = deamon_dir.join("status.json");
        let socket_path = deamon_dir.join("control.sock");
        let log_path = logs_dir.join("arrowheadd.log");

        let db_path = arrowhead_dir.join("index.db");
        let database = Arc::new(IndexDatabase::open(db_path)?);
        let indexer = Arc::new(Indexer::new(
            Arc::clone(&vault),
            Arc::clone(&database),
            self.indexer_config,
            None,
        ));

        Ok(DeamonConfig {
            vault,
            database,
            indexer,
            status_path,
            socket_path,
            log_path,
            event_buffer: self.event_buffer,
            watcher_strategy: self.watcher_strategy,
        })
    }

    /// Build the runtime configuration without launching it.
    pub fn build(self) -> Result<DeamonConfig> {
        self.prepare()
    }

    /// Spawn the deamon runtime using this builder.
    pub async fn spawn(self) -> Result<DeamonHandle> {
        let config = self.prepare()?;
        DeamonRuntime::spawn(config).await
    }
}

/// Handle returned from a spawned runtime.
pub struct DeamonHandle {
    shutdown_tx: broadcast::Sender<()>,
    task: JoinHandle<Result<()>>,
    status_path: PathBuf,
    socket_path: PathBuf,
    database: Arc<IndexDatabase>,
}

impl DeamonHandle {
    /// Path to the persisted status snapshot.
    pub fn status_path(&self) -> &Path {
        &self.status_path
    }

    /// Path to the control socket used for JSON commands.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Clone the index database handle for inspection.
    pub fn database(&self) -> Arc<IndexDatabase> {
        Arc::clone(&self.database)
    }

    /// Query the runtime over the control socket.
    pub async fn request_status(&self) -> Result<DeamonStatus> {
        match send_control_request(&self.socket_path, ControlRequest::Status).await? {
            ControlResponse::Status { status } => Ok(status),
            ControlResponse::Error { message } => Err(anyhow!(message)),
            ControlResponse::ShutdownAck => Err(anyhow!("unexpected shutdown acknowledgement")),
        }
    }

    /// Wait for the runtime task to finish.
    pub async fn join(self) -> Result<()> {
        match self.task.await {
            Ok(result) => result,
            Err(err) => Err(anyhow!("deamon task aborted: {err}")),
        }
    }

    /// Request a graceful shutdown and wait for completion.
    pub async fn shutdown(self) -> Result<()> {
        #[cfg(unix)]
        {
            match send_control_request(&self.socket_path, ControlRequest::Shutdown).await {
                Ok(ControlResponse::ShutdownAck) => {}
                Ok(ControlResponse::Error { message }) => return Err(anyhow!(message)),
                Ok(other) => {
                    warn!(?other, "unexpected control response during shutdown");
                    let _ = self.shutdown_tx.send(());
                }
                Err(err) => {
                    warn!(error = ?err, "control socket shutdown failed; forcing stop");
                    let _ = self.shutdown_tx.send(());
                }
            }
        }

        #[cfg(not(unix))]
        {
            let _ = self.shutdown_tx.send(());
        }

        self.join().await
    }
}

/// Main runtime structure wiring watchers, event queue, and control server.
struct DeamonRuntime {
    config: DeamonConfig,
    status: Arc<Mutex<DeamonStatus>>,
    _watcher: WatcherHandle,
    event_rx: mpsc::Receiver<Vec<PathBuf>>,
    shutdown_tx: broadcast::Sender<()>,
}

impl DeamonRuntime {
    fn new(config: DeamonConfig) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel(config.event_buffer);
        let watcher_root = config.vault.paths().root.clone();
        let watcher = start_watcher(
            config.watcher_strategy.clone(),
            watcher_root,
            event_tx.clone(),
        )?;

        let status_snapshot = DeamonStatus::new(config.log_path.clone());
        status_snapshot.save_to_path(&config.status_path)?;
        let status = Arc::new(Mutex::new(status_snapshot));
        let (shutdown_tx, _) = broadcast::channel(8);

        Ok(Self {
            config,
            status,
            _watcher: watcher,
            event_rx,
            shutdown_tx,
        })
    }

    async fn spawn(config: DeamonConfig) -> Result<DeamonHandle> {
        let runtime = DeamonRuntime::new(config)?;
        let shutdown_tx = runtime.shutdown_tx.clone();
        let status_path = runtime.config.status_path.clone();
        let socket_path = runtime.config.socket_path.clone();
        let database = Arc::clone(&runtime.config.database);

        let task = tokio::spawn(async move { runtime.run().await });

        Ok(DeamonHandle {
            shutdown_tx,
            task,
            status_path,
            socket_path,
            database,
        })
    }

    async fn run(mut self) -> Result<()> {
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let control_task = {
            let status = Arc::clone(&self.status);
            let shutdown_tx = self.shutdown_tx.clone();
            let socket_path = self.config.socket_path.clone();
            tokio::spawn(async move {
                run_control_server(
                    socket_path,
                    status,
                    shutdown_tx.clone(),
                    shutdown_tx.subscribe(),
                )
                .await
            })
        };

        if let Err(err) = self.ensure_initial_index().await {
            let _ = self.shutdown_tx.send(());
            if let Err(join_err) = control_task.await {
                warn!(error = ?join_err, "control server task aborted during startup");
            }
            return Err(err);
        }

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    break;
                }
                Some(batch) = self.event_rx.recv() => {
                    let mut paths: HashSet<PathBuf> =
                        batch.into_iter().filter(|path| path.exists() || path.extension().is_some()).collect();
                    while let Ok(next) = self.event_rx.try_recv() {
                        paths.extend(next);
                    }

                    if paths.is_empty() {
                        continue;
                    }

                    let queue_len = self.event_rx.len();
                    self.process_paths(paths.into_iter().collect(), queue_len).await?;
                }
                else => break,
            }
        }

        // Ensure control server exits.
        let _ = self.shutdown_tx.send(());
        if let Err(err) = control_task.await {
            warn!(error = ?err, "control server task terminated unexpectedly");
        }

        self.persist_status(|status| {
            status.activity = ActivityStatus::idle();
        })
        .await?;

        Ok(())
    }

    async fn ensure_initial_index(&self) -> Result<()> {
        self.persist_status(|status| {
            status.activity = ActivityStatus::running(ActivityState::Indexing, None, 0);
            status.activity.description = Some("performing initial full index".to_string());
            status.issues.retain(|issue| issue.code != INDEX_ERROR_CODE);
        })
        .await?;

        let stats = self.config.indexer.index_all().await?;
        let snapshot: InventorySnapshot = self.config.vault.inventory_snapshot()?;
        let indexed_notes = snapshot.entries().len() as u64;

        self.persist_status(|status| {
            status.indexed_notes = indexed_notes;
            update_index_error_issue(status, stats.errors);
            status.activity = ActivityStatus::idle();
        })
        .await?;

        Ok(())
    }

    async fn process_paths(&self, paths: Vec<PathBuf>, queued_jobs: usize) -> Result<()> {
        let snapshot: InventorySnapshot = self.config.vault.inventory_snapshot()?;
        let mut targets = HashSet::new();

        for path in paths {
            if let Some(entry) = snapshot.get_by_path(&path) {
                targets.insert(entry.absolute_path.clone());
                continue;
            }

            if let Some((_, relative)) = self.config.vault.normalise_note_path(&path) {
                let absolute = self.config.vault.note_path(relative);
                targets.insert(absolute);
                continue;
            }

            if let Some(note_id) = snapshot.note_id_for_path(&path) {
                let mut relative = PathBuf::from(&note_id);
                relative.set_extension("md");
                let absolute = snapshot.paths().root.join(relative);
                targets.insert(absolute);
            }
        }

        if targets.is_empty() {
            return Ok(());
        }

        let target_list: Vec<PathBuf> = targets.into_iter().collect();
        let active_note = target_list
            .iter()
            .filter_map(|path| self.config.vault.note_id_from_path(path))
            .next();

        self.persist_status(|status| {
            status.activity =
                ActivityStatus::running(ActivityState::Indexing, active_note.clone(), queued_jobs);
            status.activity.description = Some(format!("processing {} file(s)", target_list.len()));
        })
        .await?;

        let stats = self.config.indexer.reindex_paths(&target_list).await?;
        let indexed_notes = self.config.database.list_note_ids()?.len() as u64;

        self.persist_status(|status| {
            status.indexed_notes = indexed_notes;
            update_index_error_issue(status, stats.errors);
            status.activity = ActivityStatus::idle();
        })
        .await?;

        Ok(())
    }

    async fn persist_status<F>(&self, mut update: F) -> Result<()>
    where
        F: FnMut(&mut DeamonStatus),
    {
        let mut status = self.status.lock().await;
        update(&mut status);
        status.touch();
        status
            .save_to_path(&self.config.status_path)
            .context("failed to persist deamon status")
    }
}

const INDEX_ERROR_CODE: &str = "index_errors";

fn update_index_error_issue(status: &mut DeamonStatus, errors: u64) {
    status.error_notes = errors;
    status.issues.retain(|issue| issue.code != INDEX_ERROR_CODE);

    if errors == 0 {
        return;
    }

    let message = if errors == 1 {
        "1 note failed during the last run".to_string()
    } else {
        format!("{errors} notes failed during the last run")
    };

    let mut issue = StatusIssue::new(INDEX_ERROR_CODE, &message, IssueSeverity::Warning);
    issue.detail = Some(format!(
        "See {} for detailed log output",
        status.log_path.display()
    ));
    status.issues.push(issue);
}

/// Default CLI entrypoint used by the binary.
pub async fn cli_main() -> Result<()> {
    let root = std::env::var_os("ARROWHEAD_VAULT")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("ARROWHEAD_VAULT environment variable must be set"))?;

    let handle = DeamonRuntimeBuilder::new(root).spawn().await?;
    info!("arrowhead deamon started; waiting for shutdown signal");

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for ctrl-c")?;

    info!("shutdown signal received; stopping deamon");
    handle.shutdown().await
}
