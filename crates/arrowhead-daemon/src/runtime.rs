use std::sync::Mutex as StdMutex;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use crate::logging::LoggingGuard;
use anyhow::{Context, Result, anyhow};
use arrowhead_core::embeddings::EmbeddingDescriptor;
use arrowhead_core::embeddings::EmbeddingPipeline;
use arrowhead_core::indexer::{Indexer, IndexerConfig};
use arrowhead_core::sqlite::IndexDatabase;
use arrowhead_core::{
    ActivityState, ActivityStatus, DaemonStatus, DownloadState, DownloadStatus, IssueSeverity,
    StatusFrame, StatusIssue, Vault, VaultConfig,
};
use clap::{Parser, error::ErrorKind};
use fastembed::{EmbeddingModel, ModelTrait};
use hf_hub::api::{Progress, sync::ApiBuilder};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::{
    sync::{Mutex, broadcast, mpsc},
    task::JoinHandle,
};
use tracing::{debug, info, warn};

use crate::control::{ControlRequest, ControlResponse, run_control_server, send_control_request};
use crate::watcher::{WatcherHandle, WatcherStrategy, start_watcher};

/// Runtime configuration produced by the builder.
pub struct DaemonConfig {
    pub(crate) vault: Arc<Vault>,
    pub(crate) database: Arc<IndexDatabase>,
    pub(crate) indexer_config: IndexerConfig,
    pub(crate) embedding_model: Option<String>,
    pub(crate) status_path: PathBuf,
    pub(crate) socket_path: PathBuf,
    pub(crate) log_path: PathBuf,
    pub(crate) event_buffer: usize,
    pub(crate) watcher_strategy: WatcherStrategy,
}

/// Builder used to construct and launch the daemon runtime.
pub struct DaemonRuntimeBuilder {
    vault_root: PathBuf,
    watcher_strategy: WatcherStrategy,
    event_buffer: usize,
    indexer_config: IndexerConfig,
    embedding_model: Option<String>,
}

impl DaemonRuntimeBuilder {
    /// Construct a builder targeting the supplied vault root.
    pub fn new<P: Into<PathBuf>>(vault_root: P) -> Self {
        Self {
            vault_root: vault_root.into(),
            watcher_strategy: WatcherStrategy::Recommended,
            event_buffer: 128,
            indexer_config: IndexerConfig::default(),
            embedding_model: None,
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

    /// Configure the embedding model identifier used by the runtime.
    ///
    /// Passing `None` disables semantic indexing.
    pub fn embedding_model<S: Into<String>>(mut self, model: Option<S>) -> Self {
        self.embedding_model = model.map(|value| value.into());
        self
    }

    /// Disable semantic indexing, restricting the runtime to FTS updates.
    pub fn disable_embeddings(mut self) -> Self {
        self.embedding_model = None;
        self
    }

    fn prepare(self) -> Result<DaemonConfig> {
        let vault_config = VaultConfig::new(self.vault_root.clone());
        let vault = Arc::new(Vault::new(vault_config)?);
        vault.ensure_arrowhead_dirs()?;

        let arrowhead_dir = vault.paths().arrowhead_dir.clone();
        std::fs::create_dir_all(&arrowhead_dir).with_context(|| {
            format!("failed to ensure arrowhead dir {}", arrowhead_dir.display())
        })?;

        let daemon_dir = arrowhead_dir.join("daemon");
        std::fs::create_dir_all(&daemon_dir)
            .with_context(|| format!("failed to ensure daemon dir {}", daemon_dir.display()))?;

        let logs_dir = vault.paths().logs_dir();
        std::fs::create_dir_all(&logs_dir)
            .with_context(|| format!("failed to ensure logs dir {}", logs_dir.display()))?;

        let status_path = daemon_dir.join("status.json");
        let socket_path = daemon_dir.join("control.sock");
        let log_path = logs_dir.join("daemon.log");

        let db_path = arrowhead_dir.join("index.db");
        let database = Arc::new(IndexDatabase::open(db_path)?);

        Ok(DaemonConfig {
            vault,
            database,
            indexer_config: self.indexer_config,
            embedding_model: self.embedding_model,
            status_path,
            socket_path,
            log_path,
            event_buffer: self.event_buffer,
            watcher_strategy: self.watcher_strategy,
        })
    }

    /// Build the runtime configuration without launching it.
    pub fn build(self) -> Result<DaemonConfig> {
        self.prepare()
    }

    /// Spawn the daemon runtime using this builder.
    pub async fn spawn(self) -> Result<DaemonHandle> {
        let config = self.prepare()?;
        DaemonRuntime::spawn(config).await
    }
}

/// Handle returned from a spawned runtime.
pub struct DaemonHandle {
    shutdown_tx: broadcast::Sender<()>,
    task: JoinHandle<Result<()>>,
    status_path: PathBuf,
    socket_path: PathBuf,
    database: Arc<IndexDatabase>,
}

impl DaemonHandle {
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
    pub async fn request_status(&self) -> Result<DaemonStatus> {
        match send_control_request(&self.socket_path, ControlRequest::StatusSnapshot).await? {
            ControlResponse::Status { status } => Ok(status),
            ControlResponse::Error { message } => Err(anyhow!(message)),
            ControlResponse::ShutdownAck => Err(anyhow!("unexpected shutdown acknowledgement")),
        }
    }

    /// Wait for the runtime task to finish.
    pub async fn join(self) -> Result<()> {
        match self.task.await {
            Ok(result) => result,
            Err(err) => Err(anyhow!("daemon task aborted: {err}")),
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
struct DaemonRuntime {
    config: DaemonConfig,
    indexer: Option<Arc<Indexer>>,
    status: Arc<Mutex<DaemonStatus>>,
    frame_tx: broadcast::Sender<StatusFrame>,
    _watcher: WatcherHandle,
    event_rx: mpsc::Receiver<Vec<PathBuf>>,
    shutdown_tx: broadcast::Sender<()>,
    _logging_guard: LoggingGuard,
}

impl DaemonRuntime {
    async fn new(config: DaemonConfig) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel(config.event_buffer);
        let watcher_root = config.vault.paths().root.clone();
        let watcher = start_watcher(
            config.watcher_strategy.clone(),
            watcher_root,
            event_tx.clone(),
        )?;

        let logging_guard = crate::logging::init_logging(&config.log_path)?;

        let status_snapshot = DaemonStatus::new(config.log_path.clone());
        status_snapshot.save_to_path(&config.status_path)?;
        let status = Arc::new(Mutex::new(status_snapshot.clone()));
        let (frame_tx, _) = broadcast::channel(256);
        let _ = frame_tx.send(StatusFrame::new(status_snapshot));
        let (shutdown_tx, _) = broadcast::channel(8);

        Ok(Self {
            config,
            indexer: None,
            status,
            frame_tx,
            _watcher: watcher,
            event_rx,
            shutdown_tx,
            _logging_guard: logging_guard,
        })
    }

    async fn spawn(config: DaemonConfig) -> Result<DaemonHandle> {
        let runtime = DaemonRuntime::new(config).await?;
        let shutdown_tx = runtime.shutdown_tx.clone();
        let status_path = runtime.config.status_path.clone();
        let socket_path = runtime.config.socket_path.clone();
        let database = Arc::clone(&runtime.config.database);
        let task = tokio::spawn(async move { runtime.run().await });

        Ok(DaemonHandle {
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
            let frames = self.frame_tx.clone();
            tokio::spawn(async move {
                run_control_server(
                    socket_path,
                    status,
                    frames,
                    shutdown_tx.clone(),
                    shutdown_tx.subscribe(),
                )
                .await
            })
        };

        if let Err(err) = self.initialise_indexer().await {
            let _ = self.shutdown_tx.send(());
            if let Err(join_err) = control_task.await {
                warn!(error = ?join_err, "control server task aborted during startup");
            }
            return Err(err);
        }

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

        let indexer = self
            .indexer
            .as_ref()
            .expect("indexer should be initialised before indexing")
            .clone();
        info!("starting initial indexing pass");

        let status = Arc::clone(&self.status);
        let status_path = self.config.status_path.clone();
        let frame_tx = self.frame_tx.clone();
        let handle = tokio::runtime::Handle::current();
        let throttle = Arc::new(StdMutex::new(Instant::now()));

        let stats = indexer
            .index_all_with_observer(|event| {
                let should_emit = {
                    if let Ok(mut guard) = throttle.lock() {
                        if guard.elapsed() >= Duration::from_millis(500)
                            || event.processed == event.total
                        {
                            *guard = Instant::now();
                            true
                        } else {
                            false
                        }
                    } else {
                        true
                    }
                };

                if !should_emit {
                    return;
                }

                let status = Arc::clone(&status);
                let status_path = status_path.clone();
                let frame_tx = frame_tx.clone();
                let event = event.clone();
                handle.spawn(async move {
                    let remaining = event.total.saturating_sub(event.processed);
                    let note_id = event.note_id.clone();
                    let description =
                        format!("indexing note {} of {}", event.processed, event.total);
                    if let Err(err) =
                        persist_status_to_path(&status, &status_path, Some(&frame_tx), |snapshot| {
                            snapshot.activity.note_id = Some(note_id.clone());
                            snapshot.activity.queued_jobs = remaining as usize;
                            snapshot.activity.description = Some(description.clone());
                            snapshot.indexed_notes = event.processed;
                        })
                        .await
                    {
                        debug!(error = ?err, "failed to persist indexing progress status");
                    }
                });
            })
            .await?;
        self.persist_status(|status| {
            status.indexed_notes = stats.total_notes;
            update_index_error_issue(status, stats.errors);
            status.activity = ActivityStatus::idle();
        })
        .await?;
        info!(
            indexed = stats.indexed,
            skipped = stats.skipped,
            removed = stats.removed,
            errors = stats.errors,
            total = stats.total_notes,
            "initial indexing pass completed"
        );

        Ok(())
    }

    async fn initialise_indexer(&mut self) -> Result<()> {
        if self.indexer.is_some() {
            return Ok(());
        }

        let embeddings = prepare_embeddings(
            &self.config,
            Arc::clone(&self.status),
            self.frame_tx.clone(),
        )
        .await?;
        self.indexer = Some(Arc::new(Indexer::new(
            Arc::clone(&self.config.vault),
            Arc::clone(&self.config.database),
            self.config.indexer_config.clone(),
            embeddings,
        )));
        Ok(())
    }

    async fn process_paths(&self, paths: Vec<PathBuf>, queued_jobs: usize) -> Result<()> {
        let mut targets = HashSet::new();

        for path in paths {
            if let Some(entry) = self.config.vault.inventory_entry_for_path(&path)? {
                targets.insert(entry.absolute_path.clone());
                continue;
            }

            if let Some((_, relative)) = self.config.vault.normalise_note_path(&path) {
                let absolute = self.config.vault.note_path(&relative);
                targets.insert(absolute);
                continue;
            }

            if let Some(relative) = self.config.vault.resolve_relative_metrics_path(&path) {
                let absolute = self.config.vault.note_path(&relative);
                targets.insert(absolute);
            }
        }

        if targets.is_empty() {
            return Ok(());
        }

        let target_list: Vec<PathBuf> = targets.into_iter().collect();
        let mut target_ids: Vec<String> = target_list
            .iter()
            .filter_map(|path| self.config.vault.note_id_from_path(path))
            .collect();
        target_ids.sort();
        let active_note = target_ids.first().cloned();

        if !target_ids.is_empty() {
            let sample = if target_ids.len() <= 5 {
                target_ids.join(", ")
            } else {
                let head = target_ids[..5].join(", ");
                format!("{head}… (+{} more)", target_ids.len() - 5)
            };
            info!(
                resolved = target_ids.len(),
                sample = %sample,
                "watcher resolved note ids for reindex"
            );
            debug!(targets = ?target_ids, "watcher resolved note ids");
        }

        self.persist_status(|status| {
            status.activity =
                ActivityStatus::running(ActivityState::Indexing, active_note.clone(), queued_jobs);
            status.activity.description = Some(format!("processing {} file(s)", target_list.len()));
        })
        .await?;
        info!(
            queued = queued_jobs,
            targets = target_list.len(),
            "processing watcher event batch"
        );

        let indexer = self
            .indexer
            .as_ref()
            .expect("indexer should be initialised before reindexing");
        let stats = indexer.reindex_paths(&target_list).await?;
        let indexed_notes = self.config.database.note_count()?;

        self.persist_status(|status| {
            status.indexed_notes = indexed_notes;
            update_index_error_issue(status, stats.errors);
            status.activity = ActivityStatus::idle();
        })
        .await?;
        info!(
            indexed = stats.indexed,
            skipped = stats.skipped,
            removed = stats.removed,
            errors = stats.errors,
            "watcher batch completed"
        );

        Ok(())
    }

    async fn persist_status<F>(&self, update: F) -> Result<()>
    where
        F: FnMut(&mut DaemonStatus),
    {
        persist_status_to_path(
            &self.status,
            &self.config.status_path,
            Some(&self.frame_tx),
            update,
        )
        .await
    }
}

async fn prepare_embeddings(
    config: &DaemonConfig,
    status: Arc<Mutex<DaemonStatus>>,
    frame_tx: broadcast::Sender<StatusFrame>,
) -> Result<Option<Arc<EmbeddingPipeline>>> {
    use tokio::sync::mpsc::unbounded_channel;

    let Some(model_id) = config.embedding_model.as_ref().and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }) else {
        return Ok(None);
    };

    let descriptor = EmbeddingDescriptor::resolve(&model_id)
        .with_context(|| format!("invalid embedding preset `{model_id}`"))?;
    info!(
        model = descriptor.identifier(),
        "preparing embedding pipeline for daemon"
    );
    let paths = config.vault.paths();
    let models_dir = paths
        .arrowhead_dir
        .join("models")
        .join(descriptor.identifier());

    std::fs::create_dir_all(&models_dir).with_context(|| {
        format!(
            "failed to create embedding cache directory {}",
            models_dir.display()
        )
    })?;

    persist_status_to_path(&status, &config.status_path, Some(&frame_tx), |snapshot| {
        snapshot
            .downloads
            .retain(|entry| !entry.item.starts_with(descriptor.identifier()));
    })
    .await?;

    let (tx, rx) = unbounded_channel();
    let descriptor_for_task = descriptor.clone();
    let models_dir_for_task = models_dir.clone();
    let download_handle = tokio::task::spawn_blocking(move || {
        download_embedding_assets(&descriptor_for_task, &models_dir_for_task, tx)
    });

    let downloads_failed = consume_download_events(
        &descriptor,
        Arc::clone(&status),
        config.status_path.clone(),
        frame_tx.clone(),
        rx,
    )
    .await?;

    let join_result = download_handle.await;

    if downloads_failed {
        if let Ok(Err(err)) = &join_result {
            warn!(
                error = ?err,
                model = descriptor.identifier(),
                "embedding download reported failure"
            );
        } else if let Err(err) = &join_result {
            warn!(
                error = ?err,
                model = descriptor.identifier(),
                "embedding download task panicked"
            );
        }
        persist_status_to_path(&status, &config.status_path, Some(&frame_tx), |snapshot| {
            if snapshot.activity.state == ActivityState::Downloading
                || snapshot.activity.state == ActivityState::Faulted
            {
                snapshot.activity = ActivityStatus::idle();
            }
        })
        .await?;
        return Ok(None);
    }

    match join_result {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            warn!(
                error = ?err,
                model = descriptor.identifier(),
                "embedding download failed"
            );
            record_embedding_failure(
                &status,
                &config.status_path,
                &descriptor,
                &frame_tx,
                err.to_string(),
            )
            .await?;
            persist_status_to_path(&status, &config.status_path, Some(&frame_tx), |snapshot| {
                snapshot.activity = ActivityStatus::idle();
            })
            .await?;
            return Ok(None);
        }
        Err(err) => {
            warn!(
                error = ?err,
                model = descriptor.identifier(),
                "embedding download task panicked"
            );
            record_embedding_failure(
                &status,
                &config.status_path,
                &descriptor,
                &frame_tx,
                format!("download task panicked: {err}"),
            )
            .await?;
            persist_status_to_path(&status, &config.status_path, Some(&frame_tx), |snapshot| {
                snapshot.activity = ActivityStatus::idle();
            })
            .await?;
            return Ok(None);
        }
    }

    match EmbeddingPipeline::initialise(&config.vault, Arc::clone(&config.database), &model_id)
        .await
    {
        Ok(pipeline) => {
            if pipeline.model_changed() {
                persist_status_to_path(&status, &config.status_path, Some(&frame_tx), |snapshot| {
                    if let Some(entry) = snapshot
                        .downloads
                        .iter_mut()
                        .find(|entry| entry.item.starts_with(descriptor.identifier()))
                    {
                        entry.message = Some("model updated; vectors rebuilt".to_string());
                    }
                })
                .await?;
            }
            persist_status_to_path(&status, &config.status_path, Some(&frame_tx), |snapshot| {
                if snapshot.activity.state == ActivityState::Downloading
                    || snapshot.activity.state == ActivityState::Faulted
                {
                    snapshot.activity = ActivityStatus::idle();
                }
            })
            .await?;
            info!(
                model = pipeline.descriptor().identifier(),
                "embedding pipeline initialised"
            );
            Ok(Some(Arc::new(pipeline)))
        }
        Err(err) => {
            record_embedding_failure(
                &status,
                &config.status_path,
                &descriptor,
                &frame_tx,
                err.to_string(),
            )
            .await?;
            persist_status_to_path(&status, &config.status_path, Some(&frame_tx), |snapshot| {
                snapshot.activity = ActivityStatus::idle();
            })
            .await?;
            Ok(None)
        }
    }
}

#[derive(Debug, Clone)]
enum DownloadEvent {
    Started {
        item: String,
        total: Option<u64>,
    },
    Progress {
        item: String,
        downloaded: u64,
        total: Option<u64>,
    },
    Completed {
        item: String,
        downloaded: u64,
        total: Option<u64>,
        cached: bool,
    },
    Failed {
        item: String,
        message: String,
    },
}

#[derive(Default, Clone)]
struct ProgressState {
    downloaded: u64,
    total: Option<u64>,
    last_reported: u64,
}

struct ObserverProgress {
    item: String,
    sender: UnboundedSender<DownloadEvent>,
    state: Arc<StdMutex<ProgressState>>,
}

impl ObserverProgress {
    fn with_state(
        item: String,
        sender: UnboundedSender<DownloadEvent>,
        state: Arc<StdMutex<ProgressState>>,
    ) -> Self {
        Self {
            item,
            sender,
            state,
        }
    }
}

impl Progress for ObserverProgress {
    fn init(&mut self, size: usize, _filename: &str) {
        if let Ok(mut guard) = self.state.lock() {
            guard.total = Some(size as u64);
            guard.downloaded = 0;
            guard.last_reported = 0;
        }
        let _ = self.sender.send(DownloadEvent::Started {
            item: self.item.clone(),
            total: Some(size as u64),
        });
    }

    fn update(&mut self, size: usize) {
        let mut should_emit = false;
        let mut downloaded = 0;
        let mut total = None;
        if let Ok(mut guard) = self.state.lock() {
            guard.downloaded = guard.downloaded.saturating_add(size as u64);
            downloaded = guard.downloaded;
            total = guard.total;
            if guard.downloaded.saturating_sub(guard.last_reported) >= 512 * 1024 {
                guard.last_reported = guard.downloaded;
                should_emit = true;
            }
        }
        if should_emit {
            let _ = self.sender.send(DownloadEvent::Progress {
                item: self.item.clone(),
                downloaded,
                total,
            });
        }
    }

    fn finish(&mut self) {
        if let Ok(mut guard) = self.state.lock() {
            guard.last_reported = guard.downloaded;
        }
    }
}

fn download_embedding_assets(
    descriptor: &arrowhead_core::embeddings::EmbeddingDescriptor,
    cache_dir: &Path,
    sender: UnboundedSender<DownloadEvent>,
) -> Result<()> {
    let model_info = EmbeddingModel::get_model_info(descriptor.model())
        .ok_or_else(|| anyhow!("missing model metadata for {}", descriptor.identifier()))?;

    let api = ApiBuilder::new()
        .with_cache_dir(cache_dir.to_path_buf())
        .with_progress(false)
        .build()
        .context("failed to initialise hf-hub client")?;
    let repo = api.model(model_info.model_code.clone());

    let mut assets = Vec::new();
    assets.push(model_info.model_file.clone());
    assets.extend(model_info.additional_files.clone());

    for asset in assets {
        let item = format!("{}:{}", descriptor.identifier(), asset);
        let progress_state = Arc::new(StdMutex::new(ProgressState::default()));
        let observer =
            ObserverProgress::with_state(item.clone(), sender.clone(), Arc::clone(&progress_state));
        match repo.download_with_progress(&asset, observer) {
            Ok(path) => {
                let metadata_size = std::fs::metadata(&*path).ok().map(|meta| meta.len());
                let snapshot = progress_state
                    .lock()
                    .expect("progress state poisoned")
                    .clone();
                let total = snapshot.total.or(metadata_size);
                let downloaded = if snapshot.downloaded > 0 {
                    snapshot.downloaded
                } else {
                    total.unwrap_or(0)
                };
                let cached = snapshot.downloaded == 0 && downloaded > 0;
                let _ = sender.send(DownloadEvent::Completed {
                    item,
                    downloaded,
                    total,
                    cached,
                });
            }
            Err(err) => {
                let _ = sender.send(DownloadEvent::Failed {
                    item,
                    message: err.to_string(),
                });
                return Err(err.into());
            }
        }
    }

    Ok(())
}

async fn consume_download_events(
    descriptor: &arrowhead_core::embeddings::EmbeddingDescriptor,
    status: Arc<Mutex<DaemonStatus>>,
    status_path: PathBuf,
    frame_tx: broadcast::Sender<StatusFrame>,
    mut rx: UnboundedReceiver<DownloadEvent>,
) -> Result<bool> {
    let mut failed = false;

    while let Some(event) = rx.recv().await {
        match event {
            DownloadEvent::Started { item, total } => {
                info!(item = %item, total = total.unwrap_or(0), "embedding download started");
                persist_status_to_path(&status, &status_path, Some(&frame_tx), |snapshot| {
                    if snapshot.activity.state != ActivityState::Downloading {
                        snapshot.activity =
                            ActivityStatus::running(ActivityState::Downloading, None, 0);
                        snapshot.activity.description = Some(format!(
                            "downloading embeddings for {}",
                            descriptor.identifier()
                        ));
                    }
                    let entry = ensure_download_entry(snapshot, &item);
                    entry.state = DownloadState::InProgress;
                    entry.bytes_total = total;
                    entry.bytes_downloaded = 0;
                    entry.message = Some("downloading".to_string());
                })
                .await?;
            }
            DownloadEvent::Progress {
                item,
                downloaded,
                total,
            } => {
                persist_status_to_path(&status, &status_path, Some(&frame_tx), |snapshot| {
                    let entry = ensure_download_entry(snapshot, &item);
                    entry.state = DownloadState::InProgress;
                    entry.bytes_total = entry.bytes_total.or(total);
                    entry.bytes_downloaded = downloaded;
                })
                .await?;
            }
            DownloadEvent::Completed {
                item,
                downloaded,
                total,
                cached,
            } => {
                info!(item = %item, cached, downloaded, "embedding download completed");
                persist_status_to_path(&status, &status_path, Some(&frame_tx), |snapshot| {
                    let entry = ensure_download_entry(snapshot, &item);
                    entry.state = DownloadState::Completed;
                    entry.bytes_total = entry.bytes_total.or(total).or(Some(downloaded));
                    entry.bytes_downloaded = entry.bytes_total.unwrap_or(downloaded);
                    entry.message = if cached {
                        Some("cache hit".to_string())
                    } else {
                        Some("downloaded".to_string())
                    };
                })
                .await?;
            }
            DownloadEvent::Failed { item, message } => {
                failed = true;
                let descriptor_id = descriptor.identifier().to_string();
                warn!(item = %item, error = %message, "embedding download failed");
                persist_status_to_path(&status, &status_path, Some(&frame_tx), move |snapshot| {
                    let entry = ensure_download_entry(snapshot, &item);
                    entry.state = DownloadState::Failed;
                    entry.message = Some(message.clone());
                    snapshot.activity = ActivityStatus::running(ActivityState::Faulted, None, 0);
                    snapshot.activity.description = Some(format!(
                        "failed to download embeddings for {}",
                        descriptor_id
                    ));
                    snapshot
                        .issues
                        .retain(|issue| issue.code != EMBEDDING_DOWNLOAD_ISSUE_CODE);
                    let mut issue = StatusIssue::new(
                        EMBEDDING_DOWNLOAD_ISSUE_CODE,
                        format!("failed to download embedding assets for {}", descriptor_id),
                        IssueSeverity::Error,
                    );
                    issue.detail = Some(message.clone());
                    snapshot.issues.push(issue);
                })
                .await?;
            }
        }
    }

    Ok(failed)
}

fn ensure_download_entry<'a>(status: &'a mut DaemonStatus, item: &str) -> &'a mut DownloadStatus {
    if let Some(index) = status.downloads.iter().position(|entry| entry.item == item) {
        &mut status.downloads[index]
    } else {
        status
            .downloads
            .push(DownloadStatus::pending(item.to_string()));
        status
            .downloads
            .last_mut()
            .expect("download entry should exist")
    }
}

async fn record_embedding_failure(
    status: &Arc<Mutex<DaemonStatus>>,
    status_path: &Path,
    descriptor: &arrowhead_core::embeddings::EmbeddingDescriptor,
    frame_tx: &broadcast::Sender<StatusFrame>,
    detail: String,
) -> Result<()> {
    let descriptor_id = descriptor.identifier().to_string();
    persist_status_to_path(status, status_path, Some(frame_tx), move |snapshot| {
        snapshot
            .issues
            .retain(|issue| issue.code != EMBEDDING_INIT_ISSUE_CODE);
        if let Some(entry) = snapshot
            .downloads
            .iter_mut()
            .find(|entry| entry.item.starts_with(&descriptor_id))
        {
            entry.state = DownloadState::Failed;
            entry.message = Some(detail.clone());
        }
        let mut issue = StatusIssue::new(
            EMBEDDING_INIT_ISSUE_CODE,
            format!(
                "semantic embeddings unavailable for model {}",
                descriptor_id
            ),
            IssueSeverity::Error,
        );
        issue.detail = Some(detail.clone());
        snapshot.issues.push(issue);
    })
    .await
}

async fn persist_status_to_path<F>(
    status: &Arc<Mutex<DaemonStatus>>,
    path: &Path,
    frame_tx: Option<&broadcast::Sender<StatusFrame>>,
    mut update: F,
) -> Result<()>
where
    F: FnMut(&mut DaemonStatus),
{
    let mut guard = status.lock().await;
    update(&mut guard);
    guard.touch();
    guard
        .save_to_path(path)
        .context("failed to persist daemon status")?;
    let snapshot = guard.clone();
    drop(guard);
    if let Some(tx) = frame_tx {
        let _ = tx.send(StatusFrame::new(snapshot));
    }
    Ok(())
}

const INDEX_ERROR_CODE: &str = "index_errors";
const EMBEDDING_DOWNLOAD_ISSUE_CODE: &str = "embedding_download";
const EMBEDDING_INIT_ISSUE_CODE: &str = "embedding_pipeline";

#[derive(Debug, Parser)]
#[command(name = "arrowheadd", version)]
struct DaemonCliArgs {
    /// Vault root to index.
    #[arg(long, value_name = "PATH")]
    vault: Option<PathBuf>,
    /// Embedding model identifier, or `none` to disable semantic indexing.
    #[arg(long = "embedding-model", value_name = "MODEL")]
    embedding_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedDaemonCli {
    vault_root: PathBuf,
    embedding_model: Option<String>,
}

fn update_index_error_issue(status: &mut DaemonStatus, errors: u64) {
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

fn normalize_embedding_model(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    let normalized = trimmed.to_ascii_lowercase();
    if trimmed.is_empty() || matches!(normalized.as_str(), "none" | "off" | "fts-only" | "fts") {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn resolve_daemon_cli_args(
    cli: DaemonCliArgs,
    env_vault: Option<PathBuf>,
    env_embedding_model: Option<&str>,
) -> Result<ResolvedDaemonCli> {
    let vault_root = cli.vault.or(env_vault).ok_or_else(|| {
        anyhow!("vault path must be provided with `--vault <path>` or ARROWHEAD_VAULT")
    })?;
    let embedding_model = if let Some(value) = cli.embedding_model {
        normalize_embedding_model(Some(value))
    } else if let Some(value) = env_embedding_model {
        normalize_embedding_model(Some(value.to_string()))
    } else {
        Some("fast".to_string())
    };

    Ok(ResolvedDaemonCli {
        vault_root,
        embedding_model,
    })
}

#[cfg(test)]
fn resolve_daemon_cli_from<I, T>(
    args: I,
    env_vault: Option<PathBuf>,
    env_embedding_model: Option<&str>,
) -> Result<ResolvedDaemonCli>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let cli = DaemonCliArgs::try_parse_from(args).map_err(|err| anyhow!(err.to_string()))?;
    resolve_daemon_cli_args(cli, env_vault, env_embedding_model)
}

fn resolve_daemon_cli() -> Result<ResolvedDaemonCli> {
    let env_vault = std::env::var_os("ARROWHEAD_VAULT").map(PathBuf::from);
    let env_embedding_model = std::env::var("ARROWHEAD_EMBEDDING_MODEL").ok();
    let cli = match DaemonCliArgs::try_parse() {
        Ok(cli) => cli,
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            err.print().context("failed to write clap output")?;
            std::process::exit(0);
        }
        Err(err) => return Err(anyhow!(err.to_string())),
    };
    resolve_daemon_cli_args(cli, env_vault, env_embedding_model.as_deref())
}

/// Default CLI entrypoint used by the binary.
pub async fn cli_main() -> Result<()> {
    let cli = resolve_daemon_cli()?;

    let handle = DaemonRuntimeBuilder::new(cli.vault_root)
        .embedding_model(cli.embedding_model)
        .spawn()
        .await?;
    info!("arrowhead daemon started; waiting for shutdown signal");

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for ctrl-c")?;

    info!("shutdown signal received; stopping daemon");
    handle.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn resolve_daemon_cli_prefers_flags_over_env() {
        let cli = resolve_daemon_cli_from(
            [
                "arrowheadd",
                "--vault",
                "/tmp/cli-vault",
                "--embedding-model",
                "mini",
            ],
            Some(PathBuf::from("/tmp/env-vault")),
            Some("fast"),
        )
        .expect("cli should parse");

        assert_eq!(cli.vault_root, PathBuf::from("/tmp/cli-vault"));
        assert_eq!(cli.embedding_model, Some("mini".to_string()));
    }

    #[test]
    fn resolve_daemon_cli_uses_env_fallbacks() {
        let cli = resolve_daemon_cli_from(
            ["arrowheadd"],
            Some(PathBuf::from("/tmp/env-vault")),
            Some("fts-only"),
        )
        .expect("env fallback should parse");

        assert_eq!(cli.vault_root, PathBuf::from("/tmp/env-vault"));
        assert_eq!(cli.embedding_model, None);
    }

    #[test]
    fn resolve_daemon_cli_defaults_to_fast_embeddings() {
        let cli = resolve_daemon_cli_from(["arrowheadd", "--vault", "/tmp/vault"], None, None)
            .expect("default config should parse");

        assert_eq!(cli.vault_root, PathBuf::from("/tmp/vault"));
        assert_eq!(cli.embedding_model, Some("fast".to_string()));
    }

    #[test]
    fn resolve_daemon_cli_requires_a_vault() {
        let err = resolve_daemon_cli_from(["arrowheadd"], None, None).expect_err("vault required");
        assert!(
            err.to_string().contains("ARROWHEAD_VAULT"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn daemon_cli_supports_standard_version_flags() {
        for flag in ["--version", "-V"] {
            let err =
                DaemonCliArgs::try_parse_from(["arrowheadd", flag]).expect_err("version exits");
            assert_eq!(err.kind(), ErrorKind::DisplayVersion);
            assert_eq!(
                err.to_string(),
                format!("arrowheadd {}\n", env!("CARGO_PKG_VERSION"))
            );
        }
    }
}
