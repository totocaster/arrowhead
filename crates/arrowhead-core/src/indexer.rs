//! Indexing orchestration.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use futures::{StreamExt, stream::FuturesUnordered};
use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    task,
};
use tracing::{debug, error, info};

use crate::{
    IndexingStats, MetricsFileEntry, NoteRecord, Vault,
    embeddings::{EmbeddingPipeline, EmbeddingRecord, EmbeddingStore},
    graph::{LinkResolutionRecord, LinkTargets},
    metadata::{MetadataExtraction, MetadataExtractor},
    metrics::{metrics_reader_fingerprint, parse_metrics_reader},
    sqlite::{IndexDatabase, MetricFileState, NoteIndexState},
    vault::{NoteInventoryEntry, ScopedInventory},
};

const EMBEDDING_FLUSH_BATCH: usize = 64;

type WriteSender = mpsc::Sender<WriteJob>;

#[derive(Debug)]
struct PreparedNote {
    note: NoteRecord,
    extraction: MetadataExtraction,
    resolved_links: Vec<LinkResolutionRecord>,
    indexed_at: DateTime<Utc>,
    embedding: Option<EmbeddingRecord>,
}

#[derive(Debug)]
enum WriteOperation {
    Upsert(Box<PreparedNote>),
    Remove { note_id: String },
}

#[derive(Debug)]
struct WriteJob {
    op: WriteOperation,
    ack: oneshot::Sender<Result<WriteAck>>,
}

#[derive(Debug)]
enum WriteAck {
    Upsert,
    Remove { existed: bool },
}

#[derive(Debug)]
enum WriterResult {
    Upsert { embedding: Option<EmbeddingRecord> },
    Remove { existed: bool },
}

#[derive(Debug, Default, Clone, Copy)]
struct MetricPassStats {
    indexed: u64,
    skipped: u64,
    removed: u64,
    errors: u64,
}

/// Configuration options shared by the indexing pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexerConfig {
    /// Whether to force reindexing every note regardless of modification time.
    pub force: bool,
    /// Number of worker tasks to spawn when parallelising indexing.
    pub parallelism: usize,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            force: false,
            parallelism: num_cpus::get().clamp(1, 16),
        }
    }
}

/// Coordinates note ingestion, metadata extraction, and persistence.
#[derive(Debug, Clone)]
pub struct Indexer {
    vault: Arc<Vault>,
    database: Arc<IndexDatabase>,
    metadata: MetadataExtractor,
    config: IndexerConfig,
    embeddings: Option<Arc<EmbeddingPipeline>>,
}

impl Indexer {
    /// Create a new indexer over a vault.
    pub fn new(
        vault: Arc<Vault>,
        database: Arc<IndexDatabase>,
        config: IndexerConfig,
        embeddings: Option<Arc<EmbeddingPipeline>>,
    ) -> Self {
        Self {
            vault,
            database,
            metadata: MetadataExtractor::new(),
            config,
            embeddings,
        }
    }

    /// Runs a full indexing pass across the vault.
    pub async fn index_all(&self) -> Result<IndexingStats> {
        self.index_all_with_observer(|_| {}).await
    }

    /// Runs a full indexing pass, invoking `observer` after each processed note.
    pub async fn index_all_with_observer<F>(&self, mut observer: F) -> Result<IndexingStats>
    where
        F: FnMut(IndexProgressEvent),
    {
        let inventory = self.vault.inventory()?;
        let total = inventory.len() as u64;
        info!(
            total_notes = total,
            parallelism = self.config.parallelism,
            force = self.config.force,
            "starting full indexing pass"
        );
        let note_set: HashSet<String> = inventory.iter().map(|entry| entry.id.clone()).collect();
        let resolution = Arc::new(in_flight_link_targets(
            self.database.link_targets()?,
            note_set.iter(),
        ));
        let state_table: Arc<HashMap<String, NoteIndexState>> =
            Arc::new(self.database.note_states()?);
        let stale_embeddings = self.stale_embeddings(&state_table).await?;
        let (write_tx, write_rx) = mpsc::channel(self.config.parallelism.max(1) * 2);
        let writer_handle = tokio::spawn(run_writer(
            Arc::clone(&self.database),
            self.embedding_store(),
            write_rx,
        ));

        let semaphore = Arc::new(Semaphore::new(self.config.parallelism.max(1)));
        let mut tasks = FuturesUnordered::new();

        for entry in inventory {
            let indexer = self.clone();
            let resolution = Arc::clone(&resolution);
            let semaphore = Arc::clone(&semaphore);
            let states = Arc::clone(&state_table);
            let stale_embeddings = Arc::clone(&stale_embeddings);
            let entry_id = entry.id.clone();
            let write_sender = write_tx.clone();

            debug!(note_id = %entry.id, "queueing note for indexing");
            tasks.push(tokio::spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|err| anyhow!("indexer semaphore closed: {err}"))?;
                let result = indexer
                    .run_single(entry, resolution, states, stale_embeddings, write_sender)
                    .await;
                Ok::<_, anyhow::Error>((entry_id, result))
            }));
        }

        let mut stats = IndexingStats {
            total_notes: total,
            ..IndexingStats::default()
        };

        let mut processed = 0u64;
        while let Some(result) = tasks.next().await {
            let (note_id, outcome) = result??;
            match outcome {
                Ok(NoteProcessing::Indexed) => {
                    stats.indexed += 1;
                    processed += 1;
                    observer(IndexProgressEvent {
                        note_id,
                        processed,
                        total,
                        indexed: true,
                    });
                }
                Ok(NoteProcessing::Skipped) => {
                    stats.skipped += 1;
                    processed += 1;
                    debug!(note_id = %note_id, "note is fresh; skipping reindex");
                    observer(IndexProgressEvent {
                        note_id,
                        processed,
                        total,
                        indexed: false,
                    });
                }
                Ok(NoteProcessing::Removed) => {
                    stats.removed += 1;
                    processed += 1;
                    info!(note_id = %note_id, "removed note during full indexing pass");
                    observer(IndexProgressEvent {
                        note_id,
                        processed,
                        total,
                        indexed: true,
                    });
                }
                Err(err) => {
                    stats.errors += 1;
                    processed += 1;
                    error!(note = %note_id, error = ?err, "failed to index note");
                    observer(IndexProgressEvent {
                        note_id,
                        processed,
                        total,
                        indexed: false,
                    });
                }
            }
        }

        drop(write_tx);
        writer_handle
            .await
            .map_err(|err| anyhow!("writer task aborted: {err}"))??;

        let pruned = self.prune_missing_notes(&note_set).await?;
        if !pruned.is_empty() {
            stats.removed += pruned.len() as u64;
            stats.total_notes += pruned.len() as u64;
            info!(removed = pruned.len(), "pruned stale notes from index");
        }

        let metric_inventory = self.vault.metrics_files()?;
        let known_metric_paths: HashSet<String> = metric_inventory
            .iter()
            .map(|entry| entry.relative_path.to_string_lossy().into_owned())
            .collect();
        let pruned_metrics = self.prune_missing_metrics(&known_metric_paths).await?;
        let metric_stats = self.index_metrics_inventory(&metric_inventory).await?;
        if !pruned_metrics.is_empty() {
            info!(
                removed = pruned_metrics.len(),
                "pruned stale metrics files from index"
            );
        }
        if metric_stats.indexed > 0
            || metric_stats.skipped > 0
            || metric_stats.removed > 0
            || !pruned_metrics.is_empty()
            || metric_stats.errors > 0
        {
            info!(
                total = metric_inventory.len(),
                indexed = metric_stats.indexed,
                skipped = metric_stats.skipped,
                removed = metric_stats.removed + pruned_metrics.len() as u64,
                errors = metric_stats.errors,
                "completed metrics indexing pass"
            );
        }
        stats.errors += metric_stats.errors;
        // Unchanged notes were skipped above, so their links only follow notes
        // created, removed, or retitled since the last pass through this refresh.
        self.refresh_links().await?;

        info!(
            total = stats.total_notes,
            indexed = stats.indexed,
            skipped = stats.skipped,
            removed = stats.removed,
            errors = stats.errors,
            "completed indexing pass"
        );
        Ok(stats)
    }

    /// Reindexes a single note identified by the given ID.
    pub async fn index_note(&self, note_id: &str) -> Result<()> {
        info!(note_id = note_id, "starting single-note indexing");
        let entry = self
            .vault
            .inventory()?
            .into_iter()
            .find(|entry| entry.id == note_id)
            .with_context(|| format!("note {note_id} not found in vault"))?;
        let targets_before = self.database.link_targets()?;
        let resolution = Arc::new(in_flight_link_targets(
            targets_before.clone(),
            std::iter::once(&entry.id),
        ));
        let states = Arc::new(self.database.note_states()?);
        let stale_embeddings = self.stale_embeddings(&states).await?;
        let (write_tx, write_rx) = mpsc::channel(2);
        let writer = tokio::spawn(run_writer(
            Arc::clone(&self.database),
            self.embedding_store(),
            write_rx,
        ));
        let outcome = self
            .run_single(entry, resolution, states, stale_embeddings, write_tx)
            .await;
        writer.await.context("writer task aborted")??;
        outcome?;
        if self.database.link_targets()? != targets_before {
            self.refresh_links().await?;
        }
        info!(note_id = note_id, "completed single-note indexing");
        Ok(())
    }

    /// Incrementally reindex the supplied filesystem paths.
    ///
    /// A path may name a note, a metrics file, or a directory, and may no
    /// longer exist. See [`Indexer::plan_paths`].
    pub async fn reindex_paths(&self, paths: &[PathBuf]) -> Result<IndexingStats> {
        let plan = self.plan_paths(paths).await?;
        self.apply_plan(plan).await
    }

    /// Work that brings the index in line with the vault at and below `paths`.
    ///
    /// Every path is treated as a subtree and compared against what a full
    /// pass would index there, whatever kind of event reported it: notes and
    /// metrics files the full inventory has are checked for changes, and
    /// indexed ones it no longer has are removed.
    pub async fn plan_paths(&self, paths: &[PathBuf]) -> Result<ReindexPlan> {
        let indexer = self.clone();
        let paths = paths.to_vec();
        let dispatch = tracing::dispatcher::get_default(|current| current.clone());
        task::spawn_blocking(move || {
            tracing::dispatcher::with_default(&dispatch, || indexer.plan_paths_blocking(&paths))
        })
        .await
        .context("reindex planning task panicked")?
    }

    fn plan_paths_blocking(&self, paths: &[PathBuf]) -> Result<ReindexPlan> {
        let ScopedInventory { scopes, entries } = self.vault.inventory_within(paths)?;
        if scopes.is_empty() {
            return Ok(ReindexPlan::default());
        }
        let notes: BTreeMap<String, NoteInventoryEntry> = entries
            .into_iter()
            .map(|entry| (entry.id.clone(), entry))
            .collect();
        let removed_notes = self
            .database
            .notes_within(&scopes)?
            .into_iter()
            .filter(|(id, _)| !notes.contains_key(id))
            .map(|(id, _)| id)
            .collect();

        // Metrics follow their own inventory, which skips none of the folders
        // notes skip, so they are matched against the scopes directly.
        let metrics_root = &self.vault.metrics_conventions().root;
        let metric_scopes: Vec<&PathBuf> = scopes
            .iter()
            .filter(|scope| scope.starts_with(metrics_root) || metrics_root.starts_with(scope))
            .collect();
        let (metrics, removed_metrics) = if metric_scopes.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            let within = |path: &Path| metric_scopes.iter().any(|scope| path.starts_with(scope));
            let metrics: Vec<MetricsFileEntry> = self
                .vault
                .metrics_files()?
                .into_iter()
                .filter(|entry| within(&entry.relative_path))
                .collect();
            let live: HashSet<&Path> = metrics
                .iter()
                .map(|entry| entry.relative_path.as_path())
                .collect();
            let removed = self
                .database
                .metric_file_states()?
                .into_keys()
                .filter(|path| within(Path::new(path)) && !live.contains(Path::new(path)))
                .collect();
            (metrics, removed)
        };

        Ok(ReindexPlan {
            notes: notes.into_values().collect(),
            removed_notes,
            metrics,
            removed_metrics,
        })
    }

    /// Carry out a plan from [`Indexer::plan_paths`].
    pub async fn apply_plan(&self, plan: ReindexPlan) -> Result<IndexingStats> {
        if plan.is_empty() {
            debug!("reindex_paths called with no resolvable note or metrics paths");
            return Ok(IndexingStats::default());
        }
        let ReindexPlan {
            notes,
            removed_notes,
            metrics,
            removed_metrics,
        } = plan;

        let total = (notes.len() + removed_notes.len()) as u64;
        info!(total_notes = total, "starting targeted reindex");
        let mut stats = IndexingStats {
            total_notes: total,
            ..IndexingStats::default()
        };
        if total > 0 {
            self.apply_note_changes(notes, removed_notes, &mut stats)
                .await?;
        }

        if !metrics.is_empty() || !removed_metrics.is_empty() {
            // A moved file keeps its record ids, which must leave the stale path
            // before the new one can claim them.
            let (mut removed, mut gone) = (0, 0);
            for relative_path in removed_metrics {
                if self
                    .remove_metrics_path(PathBuf::from(relative_path))
                    .await?
                    == MetricFileProcessing::Removed
                {
                    removed += 1;
                } else {
                    gone += 1;
                }
            }
            let mut metric_stats = self.index_metrics_inventory(&metrics).await?;
            metric_stats.removed += removed;
            metric_stats.skipped += gone;
            info!(
                indexed = metric_stats.indexed,
                skipped = metric_stats.skipped,
                removed = metric_stats.removed,
                errors = metric_stats.errors,
                "completed targeted metrics reindex"
            );
            stats.errors += metric_stats.errors;
        }

        Ok(stats)
    }

    async fn apply_note_changes(
        &self,
        notes: Vec<NoteInventoryEntry>,
        removed_notes: Vec<String>,
        stats: &mut IndexingStats,
    ) -> Result<()> {
        let targets_before = self.database.link_targets()?;
        let resolution = Arc::new(in_flight_link_targets(
            targets_before.clone(),
            notes.iter().map(|entry| &entry.id),
        ));
        let state_table: Arc<HashMap<String, NoteIndexState>> =
            Arc::new(self.database.note_states()?);
        let stale_embeddings = self.stale_embeddings(&state_table).await?;
        let (write_tx, write_rx) = mpsc::channel(self.config.parallelism.max(1) * 2);
        let writer_handle = tokio::spawn(run_writer(
            Arc::clone(&self.database),
            self.embedding_store(),
            write_rx,
        ));
        let semaphore = Arc::new(Semaphore::new(self.config.parallelism.max(1)));
        let mut tasks = FuturesUnordered::new();

        let dispatch = tracing::dispatcher::get_default(|current| current.clone());
        let jobs = notes
            .into_iter()
            .map(NoteJob::Index)
            .chain(removed_notes.into_iter().map(NoteJob::Remove));

        for job in jobs {
            let indexer = self.clone();
            let resolution_clone = Arc::clone(&resolution);
            let states_clone = Arc::clone(&state_table);
            let stale_clone = Arc::clone(&stale_embeddings);
            let semaphore_clone = Arc::clone(&semaphore);
            let task_dispatch = dispatch.clone();
            let write_sender = write_tx.clone();
            tasks.push(tokio::spawn(async move {
                let fut = async move {
                    let _permit = semaphore_clone
                        .acquire_owned()
                        .await
                        .map_err(|err| anyhow!("indexer semaphore closed: {err}"))?;
                    match job {
                        NoteJob::Index(entry) => {
                            let key = entry.id.clone();
                            let outcome = indexer
                                .run_single(
                                    entry,
                                    resolution_clone,
                                    states_clone,
                                    stale_clone,
                                    write_sender,
                                )
                                .await;
                            Ok::<_, anyhow::Error>((key, outcome))
                        }
                        NoteJob::Remove(note_id) => {
                            let key = note_id.clone();
                            let dispatcher =
                                tracing::dispatcher::get_default(|current| current.clone());
                            let outcome = tokio::task::spawn_blocking(move || {
                                tracing::dispatcher::with_default(&dispatcher, || {
                                    indexer.handle_missing_note(note_id, write_sender)
                                })
                            })
                            .await
                            .map_err(|err| anyhow!("indexing task panicked: {err}"))?;
                            Ok((key, outcome))
                        }
                    }
                };
                tracing::dispatcher::with_default(&task_dispatch, || fut).await
            }));
        }

        while let Some(result) = tasks.next().await {
            let (note_id, outcome) = result??;
            match outcome {
                Ok(NoteProcessing::Indexed) => {
                    stats.indexed += 1;
                    info!(
                        note_id = note_id.as_str(),
                        "reindexed note from targeted paths"
                    );
                }
                Ok(NoteProcessing::Skipped) => {
                    stats.skipped += 1;
                    debug!(
                        note_id = note_id.as_str(),
                        "note is fresh; skipping targeted reindex"
                    );
                }
                Ok(NoteProcessing::Removed) => {
                    stats.removed += 1;
                    info!(
                        note_id = note_id.as_str(),
                        "removed note during targeted reindex"
                    );
                }
                Err(err) => {
                    stats.errors += 1;
                    error!(
                        note = note_id.as_str(),
                        error = ?err,
                        "failed during targeted reindex"
                    );
                }
            }
        }

        drop(write_tx);
        writer_handle
            .await
            .map_err(|err| anyhow!("writer task aborted: {err}"))??;

        // Links stored on unchanged notes only need resolving again when the
        // notes they can point at changed: a note appeared or went away, or a
        // title or alias changed.
        if self.database.link_targets()? != targets_before {
            self.refresh_links().await?;
        }
        Ok(())
    }

    async fn refresh_links(&self) -> Result<()> {
        let database = Arc::clone(&self.database);
        let dispatch = tracing::dispatcher::get_default(|current| current.clone());
        let changed = task::spawn_blocking(move || {
            tracing::dispatcher::with_default(&dispatch, || database.refresh_link_targets())
        })
        .await
        .context("link refresh task panicked")??;
        if changed > 0 {
            info!(
                links = changed,
                "resolved stored links against current notes"
            );
        }
        Ok(())
    }

    /// Remove a note from the index and associated vector stores.
    pub async fn remove_note(&self, note_id: &str) -> Result<bool> {
        let existed = self.database.remove_note(note_id)?;

        if existed {
            info!(note_id = note_id, "removed note from index");
        } else {
            debug!(
                note_id = note_id,
                "remove_note called for note not present in index"
            );
        }

        Ok(existed)
    }

    fn handle_missing_note(
        &self,
        note_id: String,
        write_tx: WriteSender,
    ) -> Result<NoteProcessing> {
        match submit_write(&write_tx, WriteOperation::Remove { note_id })? {
            WriteAck::Remove { existed } => {
                if existed {
                    Ok(NoteProcessing::Removed)
                } else {
                    Ok(NoteProcessing::Skipped)
                }
            }
            WriteAck::Upsert => {
                unreachable!("received upsert acknowledgement for remove job")
            }
        }
    }

    async fn prune_missing_notes(&self, known_inventory: &HashSet<String>) -> Result<Vec<String>> {
        let indexed_ids = self.database.list_note_ids()?;
        let mut removed = Vec::new();

        for note_id in indexed_ids {
            if !known_inventory.contains(&note_id) && self.remove_note(&note_id).await? {
                removed.push(note_id);
            }
        }

        Ok(removed)
    }

    async fn index_metrics_inventory(
        &self,
        inventory: &[MetricsFileEntry],
    ) -> Result<MetricPassStats> {
        let state_table = Arc::new(self.database.metric_file_states()?);
        let mut stats = MetricPassStats::default();

        for entry in inventory {
            match self
                .run_single_metrics(entry.clone(), Arc::clone(&state_table))
                .await
            {
                Ok(MetricFileProcessing::Indexed) => stats.indexed += 1,
                Ok(MetricFileProcessing::Skipped) => stats.skipped += 1,
                Ok(MetricFileProcessing::Removed) => stats.removed += 1,
                Err(err) => {
                    stats.errors += 1;
                    error!(
                        path = %entry.relative_path.display(),
                        error = ?err,
                        "failed to index metrics file"
                    );
                }
            }
        }

        Ok(stats)
    }

    async fn prune_missing_metrics(
        &self,
        known_inventory: &HashSet<String>,
    ) -> Result<Vec<String>> {
        let indexed_paths = self.database.metric_file_states()?;
        let mut removed = Vec::new();

        for relative_path in indexed_paths.keys() {
            if !known_inventory.contains(relative_path)
                && self
                    .remove_metrics_path(PathBuf::from(relative_path))
                    .await?
                    == MetricFileProcessing::Removed
            {
                removed.push(relative_path.clone());
            }
        }

        Ok(removed)
    }

    /// Notes whose stored vector does not match their current text-index generation.
    async fn stale_embeddings(
        &self,
        notes: &HashMap<String, NoteIndexState>,
    ) -> Result<Arc<HashSet<String>>> {
        match self.embedding_store() {
            Some(store) => Ok(Arc::new(store.stale_note_ids(notes).await?)),
            None => Ok(Arc::default()),
        }
    }

    fn embedding_store(&self) -> Option<EmbeddingStore> {
        self.embeddings
            .as_ref()
            .map(|pipeline| pipeline.store().clone())
    }

    async fn run_single(
        &self,
        entry: NoteInventoryEntry,
        resolution: Arc<LinkTargets>,
        index_states: Arc<HashMap<String, NoteIndexState>>,
        stale_embeddings: Arc<HashSet<String>>,
        write_tx: WriteSender,
    ) -> Result<NoteProcessing> {
        let missing_embedding = stale_embeddings.contains(&entry.id);
        if missing_embedding {
            debug!(
                note_id = %entry.id,
                "note embedding is missing or older than its text index; scheduling refresh"
            );
        }
        let indexer = self.clone();
        let dispatch = tracing::dispatcher::get_default(|current| current.clone());
        task::spawn_blocking(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                indexer.process_note(
                    &entry,
                    &resolution,
                    &index_states,
                    &write_tx,
                    missing_embedding,
                )
            })
        })
        .await
        .context("indexing task panicked")?
    }

    async fn run_single_metrics(
        &self,
        entry: MetricsFileEntry,
        index_states: Arc<HashMap<String, MetricFileState>>,
    ) -> Result<MetricFileProcessing> {
        let indexer = self.clone();
        let dispatch = tracing::dispatcher::get_default(|current| current.clone());
        task::spawn_blocking(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                indexer.process_metrics_file(&entry, &index_states)
            })
        })
        .await
        .context("metrics indexing task panicked")?
    }

    fn process_note(
        &self,
        entry: &NoteInventoryEntry,
        resolution: &LinkTargets,
        index_states: &HashMap<String, NoteIndexState>,
        write_tx: &WriteSender,
        missing_embedding: bool,
    ) -> Result<NoteProcessing> {
        let note = self
            .vault
            .load_note_from_entry(entry)
            .with_context(|| format!("failed to load note {}", entry.id))?;
        debug!(
            note_id = %entry.id,
            path = %entry.relative_path.display(),
            "processing note inventory entry"
        );

        let is_stale = note_requires_reindex(
            self.config.force,
            entry,
            index_states.get(&entry.id),
            &note.content_fingerprint(),
            missing_embedding,
        );

        if !is_stale {
            debug!(note_id = %entry.id, "note unchanged since last index; skipping");
            return Ok(NoteProcessing::Skipped);
        }

        let extraction = self.metadata.extract(&note)?;
        debug!(
            note_id = %entry.id,
            metadata_fields = extraction.metadata.len(),
            wikilinks = extraction.wikilinks.len(),
            "extracted note metadata"
        );
        let resolved_links = resolution.resolve_links(&extraction.wikilinks);
        debug!(
            note_id = %entry.id,
            link_count = resolved_links.len(),
            "resolved wikilinks for note"
        );
        let indexed_at = Utc::now();
        let metadata_count = extraction.metadata.len();
        let link_count = resolved_links.len();
        let embedding = if let Some(pipeline) = &self.embeddings {
            let context = compose_embedding_text(&note, &extraction);
            let vector = pipeline
                .generator()
                .embed_document(&context)
                .with_context(|| format!("failed to generate embedding for note {}", entry.id))?;
            Some(EmbeddingRecord {
                note_id: note.id.clone(),
                vector,
                indexed_at,
            })
        } else {
            None
        };

        let prepared = PreparedNote {
            note,
            extraction,
            resolved_links,
            indexed_at,
            embedding,
        };

        match submit_write(write_tx, WriteOperation::Upsert(Box::new(prepared)))? {
            WriteAck::Upsert => {
                info!(
                    note_id = %entry.id,
                    metadata_fields = metadata_count,
                    link_count,
                    "indexed note"
                );
                Ok(NoteProcessing::Indexed)
            }
            WriteAck::Remove { .. } => {
                unreachable!("received remove acknowledgement for upsert job")
            }
        }
    }

    fn process_metrics_file(
        &self,
        entry: &MetricsFileEntry,
        index_states: &HashMap<String, MetricFileState>,
    ) -> Result<MetricFileProcessing> {
        let relative_path = entry.relative_path.to_string_lossy().into_owned();
        let contents = fs::read(&entry.absolute_path).with_context(|| {
            format!(
                "failed to read metrics file {}",
                entry.absolute_path.display()
            )
        })?;
        let is_stale = self.config.force
            || match index_states.get(&relative_path) {
                Some(state) => {
                    state.file_modified_at < entry.file_modified_at
                        || state.content_fingerprint
                            != metrics_reader_fingerprint(
                                contents.as_slice(),
                                &entry.absolute_path,
                            )?
                }
                None => true,
            };

        if !is_stale {
            debug!(
                path = %entry.relative_path.display(),
                "metrics file unchanged since last index; skipping"
            );
            return Ok(MetricFileProcessing::Skipped);
        }

        let rows =
            parse_metrics_reader(contents.as_slice(), &entry.absolute_path).with_context(|| {
                format!(
                    "failed to parse metrics file {}",
                    entry.absolute_path.display()
                )
            })?;
        let indexed_at = Utc::now();
        let valid_records = rows
            .iter()
            .filter(|row| row.record.is_some() && !row.has_errors())
            .count();
        let issue_count = rows.iter().map(|row| row.issues.len()).sum::<usize>();

        self.database
            .upsert_metrics_file(&relative_path, entry.file_modified_at, &rows, indexed_at)
            .with_context(|| {
                format!(
                    "failed to persist metrics file {}",
                    entry.relative_path.display()
                )
            })?;
        info!(
            path = %entry.relative_path.display(),
            rows = rows.len(),
            records = valid_records,
            issues = issue_count,
            "indexed metrics file"
        );
        Ok(MetricFileProcessing::Indexed)
    }

    async fn remove_metrics_path(&self, relative_path: PathBuf) -> Result<MetricFileProcessing> {
        let relative_key = relative_path.to_string_lossy().into_owned();
        let display_path = relative_path.display().to_string();
        let database = Arc::clone(&self.database);
        let existed = task::spawn_blocking(move || database.remove_metrics_file(&relative_key))
            .await
            .context("metrics removal task panicked")??;

        if existed {
            info!(path = display_path, "removed metrics file from index");
            Ok(MetricFileProcessing::Removed)
        } else {
            debug!(
                path = display_path,
                "remove_metrics_path called for metrics file not present in index"
            );
            Ok(MetricFileProcessing::Skipped)
        }
    }
}

/// Progress event emitted during indexing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexProgressEvent {
    /// Identifier of the note that was processed.
    pub note_id: String,
    /// Number of notes processed so far (including this event).
    pub processed: u64,
    /// Total notes scheduled for the indexing run.
    pub total: u64,
    /// Whether the note was reindexed (`true`) or skipped (`false`).
    pub indexed: bool,
}

#[derive(Debug)]
enum NoteProcessing {
    Indexed,
    Skipped,
    Removed,
}

#[derive(Debug, PartialEq, Eq)]
enum MetricFileProcessing {
    Indexed,
    Skipped,
    Removed,
}

/// What the index must change to match the vault at and below some paths.
/// Built by [`Indexer::plan_paths`] and carried out by [`Indexer::apply_plan`].
#[derive(Debug, Default, Clone)]
pub struct ReindexPlan {
    notes: Vec<NoteInventoryEntry>,
    removed_notes: Vec<String>,
    metrics: Vec<MetricsFileEntry>,
    removed_metrics: Vec<String>,
}

impl ReindexPlan {
    /// Whether the paths hold nothing to index and nothing to remove.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of notes and metrics files the plan checks or removes.
    pub fn len(&self) -> usize {
        self.notes.len()
            + self.removed_notes.len()
            + self.metrics.len()
            + self.removed_metrics.len()
    }

    /// Ids of the notes the plan checks or removes, sorted.
    pub fn note_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .notes
            .iter()
            .map(|entry| entry.id.clone())
            .chain(self.removed_notes.iter().cloned())
            .collect();
        ids.sort();
        ids
    }
}

/// Link targets for resolving links while a batch is written: the indexed
/// notes plus the batch's own ids. A batch that changes titles or aliases
/// changes the targets, so the refresh that follows it resolves those.
fn in_flight_link_targets<'a>(
    mut targets: LinkTargets,
    note_ids: impl IntoIterator<Item = &'a String>,
) -> LinkTargets {
    for note_id in note_ids {
        targets.insert_note(note_id, None, &[]);
    }
    targets
}

enum NoteJob {
    Index(NoteInventoryEntry),
    Remove(String),
}

fn submit_write(write_tx: &WriteSender, op: WriteOperation) -> Result<WriteAck> {
    let (ack_tx, ack_rx) = oneshot::channel();
    write_tx
        .blocking_send(WriteJob { op, ack: ack_tx })
        .map_err(|err| anyhow!("failed to dispatch write job: {err}"))?;
    let ack = ack_rx
        .blocking_recv()
        .map_err(|err| anyhow!("writer task dropped response: {err}"))??;
    Ok(ack)
}

fn note_requires_reindex(
    force: bool,
    entry: &NoteInventoryEntry,
    state: Option<&NoteIndexState>,
    content_fingerprint: &str,
    missing_embedding: bool,
) -> bool {
    if force || missing_embedding {
        return true;
    }

    match state {
        Some(state) => {
            state.file_modified_at < entry.file_modified_at
                || state.content_fingerprint != content_fingerprint
        }
        None => true,
    }
}

async fn run_writer(
    database: Arc<IndexDatabase>,
    embeddings: Option<EmbeddingStore>,
    mut rx: mpsc::Receiver<WriteJob>,
) -> Result<()> {
    let mut embedding_buffer: Vec<EmbeddingRecord> = Vec::new();

    while let Some(WriteJob { op, ack }) = rx.recv().await {
        let db = Arc::clone(&database);
        let blocking_result = tokio::task::spawn_blocking(move || -> Result<WriterResult> {
            match op {
                WriteOperation::Upsert(prepared) => {
                    let PreparedNote {
                        note,
                        extraction,
                        resolved_links,
                        indexed_at,
                        embedding,
                    } = *prepared;
                    db.upsert_note(&note, &extraction, &resolved_links, indexed_at)?;
                    Ok(WriterResult::Upsert { embedding })
                }
                WriteOperation::Remove { note_id } => {
                    let existed = db.remove_note(&note_id)?;
                    Ok(WriterResult::Remove { existed })
                }
            }
        })
        .await
        .map_err(|err| anyhow!("writer worker panicked: {err}"))?;

        match blocking_result {
            Ok(WriterResult::Upsert { embedding }) => {
                if let Err(err) =
                    handle_embedding(embeddings.as_ref(), &mut embedding_buffer, embedding).await
                {
                    let _ = ack.send(Err(err));
                } else {
                    let _ = ack.send(Ok(WriteAck::Upsert));
                }
            }
            Ok(WriterResult::Remove { existed }) => {
                let _ = ack.send(Ok(WriteAck::Remove { existed }));
            }
            Err(err) => {
                let _ = ack.send(Err(err));
            }
        }
    }

    flush_embedding_buffer(embeddings.as_ref(), &mut embedding_buffer).await?;
    Ok(())
}

async fn handle_embedding(
    store: Option<&EmbeddingStore>,
    buffer: &mut Vec<EmbeddingRecord>,
    embedding: Option<EmbeddingRecord>,
) -> Result<()> {
    if let (Some(store), Some(record)) = (store, embedding) {
        buffer.push(record);
        if buffer.len() >= EMBEDDING_FLUSH_BATCH {
            store
                .upsert_embeddings(buffer)
                .await
                .with_context(|| "failed to persist note embeddings")?;
            buffer.clear();
        }
    }
    Ok(())
}

async fn flush_embedding_buffer(
    store: Option<&EmbeddingStore>,
    buffer: &mut Vec<EmbeddingRecord>,
) -> Result<()> {
    if let Some(store) = store {
        if !buffer.is_empty() {
            store
                .upsert_embeddings(buffer)
                .await
                .with_context(|| "failed to persist note embeddings")?;
            buffer.clear();
        }
    }
    Ok(())
}

fn compose_embedding_text(note: &NoteRecord, extraction: &MetadataExtraction) -> String {
    let mut sections = Vec::new();

    if let Some(title) = &note.title {
        let trimmed = title.trim();
        if !trimmed.is_empty() {
            sections.push(trimmed.to_string());
        }
    }

    if !extraction.metadata.is_empty() {
        let mut pairs: Vec<String> = extraction
            .metadata
            .iter()
            .map(|(key, value)| format!("{key}: {value}"))
            .collect();
        pairs.sort();
        sections.push(format!("Metadata:\n{}", pairs.join("\n")));
    }

    if !extraction.tags.is_empty() {
        sections.push(format!("Tags: {}", extraction.tags.join(", ")));
    }

    let body = note.content.trim();
    if !body.is_empty() {
        sections.push(body.to_string());
    }

    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };

    use tempfile::TempDir;
    use tokio::time::sleep;

    use crate::{
        sqlite::IndexDatabase,
        vault::{Vault, VaultConfig},
    };

    fn fixture_vault() -> Arc<Vault> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tests")
            .join("fixtures")
            .join("test-vault");
        Arc::new(Vault::new(VaultConfig::new(root)).expect("vault initialises"))
    }

    fn temp_db() -> Arc<IndexDatabase> {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("index.db");
        // Keep tempdir alive by leaking; acceptable in tests for now.
        Box::leak(Box::new(dir));
        Arc::new(IndexDatabase::open(db_path).expect("open db"))
    }

    fn temp_vault_with_note() -> (Arc<Vault>, Arc<IndexDatabase>, PathBuf) {
        let temp_dir = TempDir::new().expect("temp vault");
        let vault_dir = temp_dir.path().to_path_buf();
        Box::leak(Box::new(temp_dir));
        fs::create_dir_all(vault_dir.join(".obsidian"))
            .expect("create obsidian directory for settings");

        let raw_note_path = vault_dir.join("Sample.md");
        fs::write(
            &raw_note_path,
            "---\ncategory: temp\n---\n\n# Sample\n\nInitial content\n",
        )
        .expect("write sample note");

        let vault =
            Arc::new(Vault::new(VaultConfig::new(vault_dir.clone())).expect("vault initialises"));
        let database = temp_db();
        let canonical_note_path = vault.note_path("Sample.md");
        (vault, database, canonical_note_path)
    }

    fn temp_vault_with_metrics() -> (Arc<Vault>, Arc<IndexDatabase>, PathBuf) {
        let temp_dir = TempDir::new().expect("temp vault");
        let vault_dir = temp_dir.path().to_path_buf();
        Box::leak(Box::new(temp_dir));
        fs::create_dir_all(vault_dir.join(".obsidian"))
            .expect("create obsidian directory for settings");
        fs::create_dir_all(vault_dir.join("Metrics")).expect("create metrics directory");

        let raw_metrics_path = vault_dir.join("Metrics").join("health.metrics.ndjson");
        fs::write(
            &raw_metrics_path,
            r#"{"id":"01AAA","ts":"2026-04-14T08:30:00+00:00","key":"body.weight","value":105.6,"unit":"kg","source":"withings"}"#,
        )
        .expect("write metrics file");

        let vault =
            Arc::new(Vault::new(VaultConfig::new(vault_dir.clone())).expect("vault initialises"));
        let database = temp_db();
        let canonical_metrics_path = vault.note_path("Metrics/health.metrics.ndjson");
        (vault, database, canonical_metrics_path)
    }

    fn temp_vault_with_alias() -> (Arc<Vault>, Arc<IndexDatabase>) {
        let temp_dir = TempDir::new().expect("temp vault");
        let vault_dir = temp_dir.path().to_path_buf();
        Box::leak(Box::new(temp_dir));
        fs::create_dir_all(vault_dir.join(".obsidian"))
            .expect("create obsidian directory for settings");

        fs::write(
            vault_dir.join("Target.md"),
            "---\naliases:\n  - Alias Note\n---\n\n# Target\n",
        )
        .expect("write target note");

        fs::write(
            vault_dir.join("Source.md"),
            "---\n---\n\nReferences [[Alias Note]] and [[Target]].\n",
        )
        .expect("write source note");

        let vault =
            Arc::new(Vault::new(VaultConfig::new(vault_dir.clone())).expect("vault initialises"));
        let database = temp_db();
        (vault, database)
    }

    #[tokio::test]
    async fn index_all_persists_notes() {
        let vault = fixture_vault();
        let database = temp_db();
        let indexer = Indexer::new(vault, database.clone(), IndexerConfig::default(), None);

        let stats = indexer
            .index_all_with_observer(|_| {})
            .await
            .expect("index succeeds");

        assert!(stats.indexed > 0);
        assert!(stats.total_notes >= stats.indexed);

        let conn = database.connection().expect("connection");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
            .expect("count notes");
        assert_eq!(count, stats.indexed as i64);
    }

    #[tokio::test]
    async fn repeated_indexing_skips_fresh_notes() {
        let vault = fixture_vault();
        let database = temp_db();
        let indexer = Indexer::new(
            vault.clone(),
            database.clone(),
            IndexerConfig::default(),
            None,
        );

        let first = indexer.index_all().await.expect("first index");
        assert!(first.indexed > 0);

        let second = indexer.index_all().await.expect("second index");
        assert_eq!(second.indexed, 0);
        assert_eq!(second.skipped, first.total_notes);
    }

    #[test]
    fn note_requires_reindex_when_embeddings_are_missing() {
        let (vault, _database, _note_path) = temp_vault_with_note();
        let entry = vault
            .inventory()
            .expect("inventory")
            .into_iter()
            .find(|entry| entry.id == "Sample")
            .expect("sample note entry");
        let state = NoteIndexState {
            file_modified_at: entry.file_modified_at,
            indexed_at: entry.file_modified_at,
            content_fingerprint: "indexed".to_string(),
        };

        assert!(!note_requires_reindex(
            false,
            &entry,
            Some(&state),
            "indexed",
            false
        ));
        assert!(note_requires_reindex(
            false,
            &entry,
            Some(&state),
            "indexed",
            true
        ));
    }

    fn axis_vector(dimension: usize, axis: usize) -> Vec<f32> {
        let mut vector = vec![0.0; dimension];
        vector[axis] = 1.0;
        vector
    }

    fn prepared_note(
        vault: &Vault,
        note_id: &str,
        indexed_at: DateTime<Utc>,
        vector: Vec<f32>,
    ) -> PreparedNote {
        let entry = vault
            .inventory()
            .expect("inventory")
            .into_iter()
            .find(|entry| entry.id == note_id)
            .expect("note entry");
        let note = vault.load_note_from_entry(&entry).expect("load note");
        let extraction = MetadataExtractor::new()
            .extract(&note)
            .expect("extract metadata");
        PreparedNote {
            embedding: Some(EmbeddingRecord {
                note_id: note.id.clone(),
                vector,
                indexed_at,
            }),
            note,
            extraction,
            resolved_links: Vec::new(),
            indexed_at,
        }
    }

    async fn send_upsert(write_tx: &WriteSender, prepared: PreparedNote) -> Result<WriteAck> {
        let (ack_tx, ack_rx) = oneshot::channel();
        write_tx
            .send(WriteJob {
                op: WriteOperation::Upsert(Box::new(prepared)),
                ack: ack_tx,
            })
            .await
            .map_err(|err| anyhow!("writer closed: {err}"))?;
        ack_rx.await?
    }

    async fn write_session(
        database: &Arc<IndexDatabase>,
        store: &EmbeddingStore,
        prepared: PreparedNote,
    ) -> Result<()> {
        let (write_tx, write_rx) = mpsc::channel(2);
        let writer = tokio::spawn(run_writer(
            Arc::clone(database),
            Some(store.clone()),
            write_rx,
        ));
        send_upsert(&write_tx, prepared).await?;
        drop(write_tx);
        writer.await?
    }

    async fn next_pass_outcome(
        indexer: &Indexer,
        database: &Arc<IndexDatabase>,
        note_id: &str,
        stale_embeddings: HashSet<String>,
    ) -> NoteProcessing {
        let entry = indexer
            .vault
            .inventory()
            .expect("inventory")
            .into_iter()
            .find(|entry| entry.id == note_id)
            .expect("note entry");
        let mut resolution = LinkTargets::default();
        resolution.insert_note(note_id, None, &[]);
        let resolution = Arc::new(resolution);
        let states = Arc::new(database.note_states().expect("note states"));
        let (write_tx, write_rx) = mpsc::channel(2);
        let writer = tokio::spawn(run_writer(Arc::clone(database), None, write_rx));
        let outcome = indexer
            .run_single(
                entry,
                resolution,
                states,
                Arc::new(stale_embeddings),
                write_tx,
            )
            .await
            .expect("note processed");
        writer
            .await
            .expect("writer joins")
            .expect("writer succeeds");
        outcome
    }

    #[tokio::test]
    async fn edits_indexed_without_embeddings_are_scheduled_for_new_vectors() {
        let (vault, database, note_path) = temp_vault_with_note();
        let indexer = Indexer::new(
            Arc::clone(&vault),
            Arc::clone(&database),
            IndexerConfig::default(),
            None,
        );
        indexer.index_all().await.expect("initial index");
        let descriptor = crate::embeddings::EmbeddingDescriptor::resolve("fast")
            .expect("resolve embedding descriptor");
        let (store, _) = EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor)
            .await
            .expect("bootstrap embedding store");
        let generation = database
            .note_state("Sample")
            .expect("state query")
            .expect("state present")
            .indexed_at;
        store
            .upsert_embeddings(&[EmbeddingRecord {
                note_id: "Sample".to_string(),
                vector: axis_vector(descriptor.dimension(), 0),
                indexed_at: generation,
            }])
            .await
            .expect("upsert embedding");

        let stale = store
            .stale_note_ids(&database.note_states().expect("note states"))
            .await
            .expect("stale check");
        assert!(stale.is_empty());
        assert!(matches!(
            next_pass_outcome(&indexer, &database, "Sample", stale).await,
            NoteProcessing::Skipped
        ));

        sleep(Duration::from_millis(10)).await;
        fs::write(
            &note_path,
            "---\ncategory: temp\n---\n\n# Sample\n\nEdited while embeddings were off\n",
        )
        .expect("edit note");
        indexer
            .reindex_paths(std::slice::from_ref(&note_path))
            .await
            .expect("text-only reindex");

        let stale = store
            .stale_note_ids(&database.note_states().expect("note states"))
            .await
            .expect("stale check");
        assert!(
            stale.contains("Sample"),
            "a text reindex without a vector must mark the old vector stale"
        );
        assert!(matches!(
            next_pass_outcome(&indexer, &database, "Sample", stale).await,
            NoteProcessing::Indexed
        ));
    }

    #[tokio::test]
    async fn failed_vector_flush_leaves_a_durable_repair_marker() {
        let temp_dir = TempDir::new().expect("temp dir");
        let vault_dir = temp_dir.path().join("vault");
        fs::create_dir_all(&vault_dir).expect("create vault");
        let note_path = vault_dir.join("Sample.md");
        fs::write(&note_path, "first version").expect("write note");
        let vault = Arc::new(Vault::new(VaultConfig::new(vault_dir)).expect("vault initialises"));
        let db_path = temp_dir.path().join("index.db");
        let descriptor = crate::embeddings::EmbeddingDescriptor::resolve("fast")
            .expect("resolve embedding descriptor");
        let dimension = descriptor.dimension();

        let database = Arc::new(IndexDatabase::open(&db_path).expect("open db"));
        let (store, _) = EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor)
            .await
            .expect("bootstrap embedding store");
        write_session(
            &database,
            &store,
            prepared_note(&vault, "Sample", Utc::now(), axis_vector(dimension, 0)),
        )
        .await
        .expect("initial write succeeds");
        assert!(
            store
                .stale_note_ids(&database.note_states().expect("note states"))
                .await
                .expect("stale check")
                .is_empty()
        );

        fs::write(&note_path, "second version").expect("edit note");
        database
            .connection()
            .expect("connection")
            .execute_batch(
                "CREATE TRIGGER fail_vector_flush BEFORE INSERT ON note_embeddings_rowids
                 BEGIN SELECT RAISE(ABORT, 'injected vector flush failure'); END;",
            )
            .expect("install flush failure");
        let edited_at = Utc::now();
        let (write_tx, write_rx) = mpsc::channel(2);
        let writer = tokio::spawn(run_writer(
            Arc::clone(&database),
            Some(store.clone()),
            write_rx,
        ));
        let ack = send_upsert(
            &write_tx,
            prepared_note(&vault, "Sample", edited_at, axis_vector(dimension, 1)),
        )
        .await;
        assert!(matches!(ack, Ok(WriteAck::Upsert)));
        assert!(
            store
                .stale_note_ids(&database.note_states().expect("note states"))
                .await
                .expect("stale check")
                .contains("Sample"),
            "an acknowledged note whose vector is still buffered is already marked stale"
        );
        drop(write_tx);
        let flush = writer.await.expect("writer joins");
        let err = flush.expect_err("a failed vector flush fails the pass");
        assert!(
            format!("{err:#}").contains("failed to persist note embeddings"),
            "unexpected flush error: {err:#}"
        );
        database
            .connection()
            .expect("connection")
            .execute_batch("DROP TRIGGER fail_vector_flush")
            .expect("remove flush failure");
        drop(store);
        drop(database);

        let database = Arc::new(IndexDatabase::open(&db_path).expect("reopen db"));
        let (store, changed) = EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor)
            .await
            .expect("bootstrap embedding store");
        assert!(!changed);
        assert_eq!(
            database
                .note_state("Sample")
                .expect("state query")
                .expect("state present")
                .indexed_at
                .timestamp_micros(),
            edited_at.timestamp_micros(),
            "the text index kept the edit"
        );
        assert_eq!(
            store.vector_for_note("Sample").await.expect("vector query"),
            Some(axis_vector(dimension, 0)),
            "the old vector survived the rolled-back flush"
        );
        assert!(
            store
                .stale_note_ids(&database.note_states().expect("note states"))
                .await
                .expect("stale check")
                .contains("Sample"),
            "the stale vector is still marked after reopening the index"
        );

        write_session(
            &database,
            &store,
            prepared_note(&vault, "Sample", Utc::now(), axis_vector(dimension, 2)),
        )
        .await
        .expect("repair write succeeds");
        assert!(
            store
                .stale_note_ids(&database.note_states().expect("note states"))
                .await
                .expect("stale check")
                .is_empty()
        );
        assert_eq!(
            store.vector_for_note("Sample").await.expect("vector query"),
            Some(axis_vector(dimension, 2))
        );
    }

    #[tokio::test]
    async fn index_note_reports_a_failed_database_write() {
        let (vault, database, _) = temp_vault_with_note();
        database
            .connection()
            .expect("connection")
            .execute_batch(
                "CREATE TRIGGER fail_note_write BEFORE INSERT ON notes
             BEGIN SELECT RAISE(ABORT, 'injected note write failure'); END;",
            )
            .expect("inject write failure");
        let indexer = Indexer::new(vault, database.clone(), IndexerConfig::default(), None);

        let error = indexer
            .index_note("Sample")
            .await
            .expect_err("write must fail");
        assert!(format!("{error:#}").contains("injected note write failure"));
        assert!(database.note_state("Sample").expect("state").is_none());
    }

    #[tokio::test]
    async fn index_note_only_updates_target() {
        let (vault, database, note_path) = temp_vault_with_note();
        let other_path = vault.paths().root.join("Other.md");
        fs::write(&other_path, "otheroriginal").expect("other note");
        let indexer = Indexer::new(vault, database.clone(), IndexerConfig::default(), None);
        indexer.index_all().await.expect("initial index");
        let other_before = database.note_state("Other").expect("other state");

        fs::write(note_path, "targetfresh").expect("change target");
        fs::write(other_path, "otherfresh").expect("change other");
        indexer
            .index_note("Sample")
            .await
            .expect("single note index");

        let target = database
            .search_fts("targetfresh", 10)
            .expect("target search");
        assert_eq!(target.len(), 1);
        assert_eq!(target[0].note_id, "Sample");
        assert!(
            database
                .search_fts("otherfresh", 10)
                .expect("other search")
                .is_empty()
        );
        assert_eq!(
            database
                .search_fts("otheroriginal", 10)
                .expect("old other search")
                .len(),
            1
        );
        assert_eq!(
            database.note_state("Other").expect("other state"),
            other_before
        );
    }

    #[tokio::test]
    async fn index_all_reports_progress_events() {
        let vault = fixture_vault();
        let database = temp_db();
        let indexer = Indexer::new(vault, database, IndexerConfig::default(), None);

        let mut events = Vec::new();
        let stats = indexer
            .index_all_with_observer(|event| events.push(event))
            .await
            .expect("index succeeds");

        assert_eq!(events.len() as u64, stats.total_notes);
        assert!(events.iter().any(|event| event.indexed));
        assert_eq!(events.last().unwrap().processed, stats.total_notes);
    }

    #[tokio::test]
    async fn resolves_alias_wikilinks() {
        let (vault, database) = temp_vault_with_alias();
        let indexer = Indexer::new(
            Arc::clone(&vault),
            Arc::clone(&database),
            IndexerConfig::default(),
            None,
        );

        indexer.index_all().await.expect("index succeeds");

        let conn = database.connection().expect("connection");
        let mut stmt = conn
            .prepare(
                "SELECT target_id, reason FROM note_links WHERE source_id = 'Source' AND raw_text = 'Alias Note'",
            )
            .expect("prepare alias query");
        let mut rows = stmt.query([]).expect("execute alias query");
        let row = rows.next().expect("row present").expect("row ok");
        let target: Option<String> = row.get(0).expect("read target");
        let reason: String = row.get(1).expect("read reason");
        assert_eq!(target.as_deref(), Some("Target"));
        assert_eq!(reason, crate::LinkReason::Alias.as_str());
    }

    #[tokio::test]
    async fn reindex_paths_updates_modified_note() {
        let (vault, database, note_path) = temp_vault_with_note();
        let indexer = Indexer::new(
            Arc::clone(&vault),
            Arc::clone(&database),
            IndexerConfig::default(),
            None,
        );

        indexer.index_all().await.expect("initial index");
        let before = database
            .note_state("Sample")
            .expect("state query")
            .expect("state present")
            .indexed_at;

        assert!(vault.normalise_note_path(&note_path).is_some());

        sleep(Duration::from_millis(10)).await;
        fs::write(
            &note_path,
            "---\ncategory: temp\n---\n\n# Sample\n\nUpdated content\n",
        )
        .expect("update note");

        let stats = indexer
            .reindex_paths(&[note_path.clone()])
            .await
            .expect("targeted reindex");
        assert_eq!(stats.total_notes, 1);
        assert_eq!(stats.indexed, 1);
        assert_eq!(stats.removed, 0);

        let after = database
            .note_state("Sample")
            .expect("state query")
            .expect("state present")
            .indexed_at;
        assert!(after >= before);
    }

    #[tokio::test]
    async fn reindex_paths_removes_deleted_note() {
        let (vault, database, note_path) = temp_vault_with_note();
        let indexer = Indexer::new(
            Arc::clone(&vault),
            Arc::clone(&database),
            IndexerConfig::default(),
            None,
        );

        indexer.index_all().await.expect("initial index");

        fs::remove_file(&note_path).expect("remove note file");

        assert!(vault.normalise_note_path(&note_path).is_some());

        let stats = indexer
            .reindex_paths(&[note_path.clone()])
            .await
            .expect("targeted reindex");
        assert_eq!(stats.total_notes, 1);
        assert_eq!(stats.removed, 1);
        assert_eq!(stats.indexed, 0);

        let ids = database.list_note_ids().expect("list note ids");
        assert!(ids.is_empty());
        assert!(
            database
                .note_state("Sample")
                .expect("state query")
                .is_none()
        );
    }

    #[tokio::test]
    async fn index_all_persists_metrics_files() {
        let (vault, database, metrics_path) = temp_vault_with_metrics();
        let indexer = Indexer::new(vault, Arc::clone(&database), IndexerConfig::default(), None);

        indexer.index_all().await.expect("index succeeds");

        let relative_path = "Metrics/health.metrics.ndjson";
        assert!(
            database
                .metric_file_state(relative_path)
                .expect("metrics state query")
                .is_some()
        );

        let conn = database.connection().expect("connection");
        let record_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM metric_records WHERE source_file = ?1",
                [relative_path],
                |row| row.get(0),
            )
            .expect("count metric records");
        assert_eq!(record_count, 1);
        assert!(metrics_path.exists());
    }

    #[tokio::test]
    async fn reindex_paths_updates_modified_metrics_file() {
        let (vault, database, metrics_path) = temp_vault_with_metrics();
        let indexer = Indexer::new(
            Arc::clone(&vault),
            Arc::clone(&database),
            IndexerConfig::default(),
            None,
        );

        indexer.index_all().await.expect("initial index");
        let before = database
            .metric_file_state("Metrics/health.metrics.ndjson")
            .expect("state query")
            .expect("state present")
            .indexed_at;

        sleep(Duration::from_millis(10)).await;
        fs::write(
            &metrics_path,
            r#"{"id":"01AAB","ts":"2026-04-15T08:30:00+00:00","key":"body.weight","value":104.4,"unit":"kg","source":"withings"}"#,
        )
        .expect("update metrics file");

        let stats = indexer
            .reindex_paths(std::slice::from_ref(&metrics_path))
            .await
            .expect("targeted reindex");
        assert_eq!(stats.total_notes, 0);

        let after = database
            .metric_file_state("Metrics/health.metrics.ndjson")
            .expect("state query")
            .expect("state present")
            .indexed_at;
        assert!(after >= before);

        let conn = database.connection().expect("connection");
        let ids: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT id FROM metric_records
                     WHERE source_file = 'Metrics/health.metrics.ndjson'
                     ORDER BY source_line",
                )
                .expect("prepare metric record query");
            let rows = stmt
                .query_map([], |row| row.get(0))
                .expect("iterate metric ids");
            rows.map(|row| row.expect("metric id")).collect()
        };
        assert_eq!(ids, vec!["01AAB".to_string()]);
    }

    #[tokio::test]
    async fn reindex_paths_removes_deleted_metrics_file() {
        let (vault, database, metrics_path) = temp_vault_with_metrics();
        let indexer = Indexer::new(
            Arc::clone(&vault),
            Arc::clone(&database),
            IndexerConfig::default(),
            None,
        );

        indexer.index_all().await.expect("initial index");

        fs::remove_file(&metrics_path).expect("remove metrics file");

        assert!(vault.resolve_relative_metrics_path(&metrics_path).is_some());

        let stats = indexer
            .reindex_paths(std::slice::from_ref(&metrics_path))
            .await
            .expect("targeted reindex");
        assert_eq!(stats.total_notes, 0);
        assert_eq!(stats.errors, 0);

        assert!(
            database
                .metric_file_state("Metrics/health.metrics.ndjson")
                .expect("state query")
                .is_none()
        );
    }
}
