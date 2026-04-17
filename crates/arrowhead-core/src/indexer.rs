//! Indexing orchestration.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use futures::{StreamExt, stream::FuturesUnordered};
use serde_json::Value;
use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    task,
};
use tracing::{debug, error, info};

use crate::{
    IndexingStats, MetricsFileEntry, NoteRecord, Vault,
    embeddings::{EmbeddingPipeline, EmbeddingRecord, EmbeddingStore},
    graph::{LinkReason, LinkResolutionRecord, normalise_link_lookup},
    metadata::{MetadataExtraction, MetadataExtractor, WikiLink},
    metrics::parse_metrics_file,
    sqlite::{IndexDatabase, LinkResolutionMaps, MetricFileState, NoteIndexState},
    vault::NoteInventoryEntry,
    vault::{normalise_relative_path, normalise_relative_str},
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
    Upsert(PreparedNote),
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
        let note_set: Arc<HashSet<String>> =
            Arc::new(inventory.iter().map(|entry| entry.id.clone()).collect());
        let resolution_maps = self.collect_resolution_maps(&inventory)?;
        let resolution = Arc::new(ResolutionContext::new(
            Arc::clone(&note_set),
            resolution_maps,
        ));
        let state_table: Arc<HashMap<String, NoteIndexState>> =
            Arc::new(self.database.note_states()?);
        let (write_tx, write_rx) = mpsc::channel(self.config.parallelism.max(1) * 2);
        let writer_handle = tokio::spawn(run_writer(
            Arc::clone(&self.database),
            self.embeddings.clone(),
            write_rx,
        ));

        let semaphore = Arc::new(Semaphore::new(self.config.parallelism.max(1)));
        let mut tasks = FuturesUnordered::new();

        for entry in inventory {
            let indexer = self.clone();
            let resolution = Arc::clone(&resolution);
            let semaphore = Arc::clone(&semaphore);
            let states = Arc::clone(&state_table);
            let entry_id = entry.id.clone();
            let write_sender = write_tx.clone();

            debug!(note_id = %entry.id, "queueing note for indexing");
            tasks.push(tokio::spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|err| anyhow!("indexer semaphore closed: {err}"))?;
                let result = indexer
                    .run_single(entry, resolution, states, write_sender)
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

        let pruned = self.prune_missing_notes(note_set.as_ref()).await?;
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
        let metric_stats = self.index_metrics_inventory(&metric_inventory).await?;
        let pruned_metrics = self.prune_missing_metrics(&known_metric_paths).await?;
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
        let inventory = self.vault.inventory()?;
        let note_set: Arc<HashSet<String>> =
            Arc::new(inventory.iter().map(|entry| entry.id.clone()).collect());
        let mut resolution_maps = self.database.link_resolution_maps()?;
        let state_table: Arc<HashMap<String, NoteIndexState>> =
            Arc::new(self.database.note_states()?);
        let entry = inventory
            .iter()
            .find(|entry| entry.id == note_id)
            .cloned()
            .with_context(|| format!("note {note_id} not found in vault"))?;

        let note = self
            .vault
            .load_note_from_entry(&entry)
            .with_context(|| format!("failed to load note {note_id} for resolution hints"))?;
        let extraction = self.metadata.extract(&note)?;
        let aliases = extract_aliases(&extraction);
        resolution_maps.ingest_note(&note.id, note.title.as_deref(), &aliases);

        let resolution = Arc::new(ResolutionContext::new(
            Arc::clone(&note_set),
            resolution_maps,
        ));
        let (write_tx, write_rx) = mpsc::channel(2);
        let writer_handle = tokio::spawn(run_writer(
            Arc::clone(&self.database),
            self.embeddings.clone(),
            write_rx,
        ));
        let outcome = self
            .run_single(entry, resolution, state_table, write_tx.clone())
            .await?;
        drop(write_tx);
        writer_handle
            .await
            .map_err(|err| anyhow!("writer task aborted: {err}"))??;

        match outcome {
            NoteProcessing::Indexed => {}
            NoteProcessing::Skipped => {}
            NoteProcessing::Removed => {
                debug!(
                    note_id = note_id,
                    "note removed during single-note indexing"
                );
            }
        }
        info!(note_id = note_id, "completed single-note indexing");
        Ok(())
    }

    /// Incrementally reindex the supplied filesystem paths.
    pub async fn reindex_paths(&self, paths: &[PathBuf]) -> Result<IndexingStats> {
        let mut targets: HashMap<String, PathBuf> = HashMap::new();
        let mut metric_targets: Vec<PathBuf> = Vec::new();
        for path in paths {
            if let Some((note_id, relative)) = self.vault.normalise_note_path(path) {
                let absolute = self.vault.note_path(&relative);
                targets.insert(note_id, absolute);
                continue;
            }

            if self.vault.resolve_relative_metrics_path(path).is_some() {
                metric_targets.push(path.clone());
            }
        }

        if targets.is_empty() && metric_targets.is_empty() {
            debug!("reindex_paths called with no resolvable note or metrics paths");
            return Ok(IndexingStats::default());
        }

        let total = targets.len() as u64;
        info!(total_notes = total, "starting targeted reindex");

        let mut known_notes: HashSet<String> = self.database.list_note_ids()?.into_iter().collect();
        for note_id in targets.keys() {
            known_notes.insert(note_id.clone());
        }
        let known = Arc::new(known_notes);
        let mut resolution_maps = self.database.link_resolution_maps()?;
        for absolute_path in targets.values() {
            if let Some(entry) = self.vault.inventory_entry_for_path(absolute_path)? {
                let note = self.vault.load_note_from_entry(&entry).with_context(|| {
                    format!("failed to load note {} for resolution hints", entry.id)
                })?;
                let extraction = self.metadata.extract(&note)?;
                let aliases = extract_aliases(&extraction);
                resolution_maps.ingest_note(&note.id, note.title.as_deref(), &aliases);
            }
        }
        let resolution = Arc::new(ResolutionContext::new(Arc::clone(&known), resolution_maps));
        let state_table: Arc<HashMap<String, NoteIndexState>> =
            Arc::new(self.database.note_states()?);
        let (write_tx, write_rx) = mpsc::channel(self.config.parallelism.max(1) * 2);
        let writer_handle = tokio::spawn(run_writer(
            Arc::clone(&self.database),
            self.embeddings.clone(),
            write_rx,
        ));
        let semaphore = Arc::new(Semaphore::new(self.config.parallelism.max(1)));
        let mut tasks = FuturesUnordered::new();

        let dispatch = tracing::dispatcher::get_default(|current| current.clone());

        for (note_id, absolute_path) in targets {
            let indexer = self.clone();
            let resolution_clone = Arc::clone(&resolution);
            let states_clone = Arc::clone(&state_table);
            let semaphore_clone = Arc::clone(&semaphore);
            let task_dispatch = dispatch.clone();
            let write_sender = write_tx.clone();
            tasks.push(tokio::spawn(async move {
                let fut = async move {
                    let _permit = semaphore_clone
                        .acquire_owned()
                        .await
                        .map_err(|err| anyhow!("indexer semaphore closed: {err}"))?;
                    let key = note_id.clone();
                    let outcome = match indexer.vault.inventory_entry_for_path(&absolute_path) {
                        Ok(Some(entry)) => {
                            indexer
                                .run_single(
                                    entry,
                                    resolution_clone,
                                    states_clone,
                                    write_sender.clone(),
                                )
                                .await
                        }
                        Ok(None) => {
                            let dispatcher =
                                tracing::dispatcher::get_default(|current| current.clone());
                            let indexer_clone = indexer.clone();
                            let write_clone = write_sender.clone();
                            tokio::task::spawn_blocking(move || {
                                tracing::dispatcher::with_default(&dispatcher, || {
                                    indexer_clone.handle_missing_note(note_id.clone(), write_clone)
                                })
                            })
                            .await
                            .map_err(|err| anyhow!("indexing task panicked: {err}"))?
                        }
                        Err(err) => Err(err),
                    };
                    Ok::<_, anyhow::Error>((key, outcome))
                };
                tracing::dispatcher::with_default(&task_dispatch, || fut).await
            }));
        }

        let mut stats = IndexingStats {
            total_notes: total,
            ..IndexingStats::default()
        };

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

        if !metric_targets.is_empty() {
            let metric_stats = self.reindex_metric_paths(&metric_targets).await?;
            if metric_stats.indexed > 0
                || metric_stats.skipped > 0
                || metric_stats.removed > 0
                || metric_stats.errors > 0
            {
                info!(
                    indexed = metric_stats.indexed,
                    skipped = metric_stats.skipped,
                    removed = metric_stats.removed,
                    errors = metric_stats.errors,
                    "completed targeted metrics reindex"
                );
            }
            stats.errors += metric_stats.errors;
        }

        Ok(stats)
    }

    /// Remove a note from the index and associated vector stores.
    pub async fn remove_note(&self, note_id: &str) -> Result<bool> {
        let existed = self.database.remove_note(note_id)?;

        if existed {
            if let Some(pipeline) = &self.embeddings {
                pipeline
                    .store()
                    .delete_embeddings(&[note_id.to_string()])
                    .await
                    .with_context(|| format!("failed to delete embeddings for note {note_id}"))?;
            }
        }

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

    async fn reindex_metric_paths(&self, paths: &[PathBuf]) -> Result<MetricPassStats> {
        let state_table = Arc::new(self.database.metric_file_states()?);
        let mut seen = HashSet::new();
        let mut stats = MetricPassStats::default();

        for path in paths {
            let Some(relative_path) = self.vault.resolve_relative_metrics_path(path) else {
                continue;
            };
            if !seen.insert(relative_path.clone()) {
                continue;
            }

            match self.vault.metrics_entry_for_path(path)? {
                Some(entry) => match self
                    .run_single_metrics(entry, Arc::clone(&state_table))
                    .await
                {
                    Ok(MetricFileProcessing::Indexed) => stats.indexed += 1,
                    Ok(MetricFileProcessing::Skipped) => stats.skipped += 1,
                    Ok(MetricFileProcessing::Removed) => stats.removed += 1,
                    Err(err) => {
                        stats.errors += 1;
                        error!(
                            path = %relative_path.display(),
                            error = ?err,
                            "failed to reindex metrics file"
                        );
                    }
                },
                None => match self.remove_metrics_path(relative_path.clone()).await? {
                    MetricFileProcessing::Removed => stats.removed += 1,
                    MetricFileProcessing::Skipped => stats.skipped += 1,
                    MetricFileProcessing::Indexed => {
                        unreachable!("metrics removal should not produce indexed outcome")
                    }
                },
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

    fn collect_resolution_maps(
        &self,
        inventory: &[NoteInventoryEntry],
    ) -> Result<LinkResolutionMaps> {
        let mut maps = self.database.link_resolution_maps()?;

        for entry in inventory {
            let note = self
                .vault
                .load_note_from_entry(entry)
                .with_context(|| format!("failed to load note {}", entry.id))?;
            let extraction = self.metadata.extract(&note)?;
            let aliases = extract_aliases(&extraction);
            maps.ingest_note(&note.id, note.title.as_deref(), &aliases);
        }

        Ok(maps)
    }

    async fn run_single(
        &self,
        entry: NoteInventoryEntry,
        resolution: Arc<ResolutionContext>,
        index_states: Arc<HashMap<String, NoteIndexState>>,
        write_tx: WriteSender,
    ) -> Result<NoteProcessing> {
        let missing_embedding = note_requires_embedding_backfill(
            self.embeddings.as_ref().map(|pipeline| pipeline.store()),
            self.config.force,
            &entry,
            index_states.get(&entry.id),
        )
        .await?;
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
        resolution: &ResolutionContext,
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

        let state = index_states.get(&entry.id).cloned();
        let is_stale =
            note_requires_reindex(self.config.force, entry, state.as_ref(), missing_embedding);

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
        let resolved_links = resolve_wikilinks(&entry.id, &extraction.wikilinks, resolution);
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

        match submit_write(write_tx, WriteOperation::Upsert(prepared))? {
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
        let state = index_states.get(&relative_path).cloned();
        let is_stale = if self.config.force {
            true
        } else if let Some(state) = state {
            state.file_modified_at < entry.file_modified_at
        } else {
            true
        };

        if !is_stale {
            debug!(
                path = %entry.relative_path.display(),
                "metrics file unchanged since last index; skipping"
            );
            return Ok(MetricFileProcessing::Skipped);
        }

        let rows = parse_metrics_file(&entry.absolute_path).with_context(|| {
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

#[derive(Debug, Clone)]
struct ResolutionContext {
    note_ids: Arc<HashSet<String>>,
    lowercase_ids: Arc<HashMap<String, String>>,
    maps: Arc<LinkResolutionMaps>,
}

impl ResolutionContext {
    fn new(note_ids: Arc<HashSet<String>>, maps: LinkResolutionMaps) -> Self {
        let lowercase_ids = note_ids
            .iter()
            .map(|id| (normalise_link_lookup(id), id.clone()))
            .collect();
        Self {
            note_ids,
            lowercase_ids: Arc::new(lowercase_ids),
            maps: Arc::new(maps),
        }
    }

    fn resolve(&self, candidate: &str) -> (Option<String>, LinkReason) {
        if let Some(id) = self.resolve_direct(candidate) {
            return (Some(id), LinkReason::Direct);
        }
        if let Some(id) = self.resolve_title(candidate) {
            return (Some(id), LinkReason::Title);
        }
        if let Some(id) = self.resolve_alias(candidate) {
            return (Some(id), LinkReason::Alias);
        }
        (None, LinkReason::Unresolved)
    }

    fn resolve_direct(&self, candidate: &str) -> Option<String> {
        if self.note_ids.contains(candidate) {
            return Some(candidate.to_string());
        }
        let key = normalise_link_lookup(candidate);
        self.lowercase_ids.get(&key).cloned()
    }

    fn resolve_title(&self, candidate: &str) -> Option<String> {
        let key = normalise_link_lookup(candidate);
        self.maps.titles.get(&key).cloned()
    }

    fn resolve_alias(&self, candidate: &str) -> Option<String> {
        let key = normalise_link_lookup(candidate);
        match self.maps.aliases.get(&key) {
            Some(ids) if ids.len() == 1 => Some(ids[0].clone()),
            _ => None,
        }
    }
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
    missing_embedding: bool,
) -> bool {
    if force || missing_embedding {
        return true;
    }

    match state {
        Some(state) => state.file_modified_at < entry.file_modified_at,
        None => true,
    }
}

async fn note_requires_embedding_backfill(
    store: Option<&EmbeddingStore>,
    force: bool,
    entry: &NoteInventoryEntry,
    state: Option<&NoteIndexState>,
) -> Result<bool> {
    let Some(store) = store else {
        return Ok(false);
    };

    if force {
        return Ok(false);
    }

    let Some(state) = state else {
        return Ok(false);
    };

    if state.file_modified_at < entry.file_modified_at {
        return Ok(false);
    }

    let missing_embedding = !store.has_embedding_for_note(&entry.id).await?;
    if missing_embedding {
        debug!(
            note_id = %entry.id,
            "note is indexed but missing embeddings; scheduling backfill"
        );
    }
    Ok(missing_embedding)
}

async fn run_writer(
    database: Arc<IndexDatabase>,
    embeddings: Option<Arc<EmbeddingPipeline>>,
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
                    } = prepared;
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
                    handle_embedding(&embeddings, &mut embedding_buffer, embedding).await
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

    flush_embedding_buffer(&embeddings, &mut embedding_buffer).await?;
    Ok(())
}

async fn handle_embedding(
    pipeline: &Option<Arc<EmbeddingPipeline>>,
    buffer: &mut Vec<EmbeddingRecord>,
    embedding: Option<EmbeddingRecord>,
) -> Result<()> {
    if let (Some(pipeline), Some(record)) = (pipeline.as_ref(), embedding) {
        buffer.push(record);
        if buffer.len() >= EMBEDDING_FLUSH_BATCH {
            pipeline
                .store()
                .upsert_embeddings(buffer)
                .await
                .with_context(|| "failed to persist note embeddings")?;
            buffer.clear();
        }
    }
    Ok(())
}

async fn flush_embedding_buffer(
    pipeline: &Option<Arc<EmbeddingPipeline>>,
    buffer: &mut Vec<EmbeddingRecord>,
) -> Result<()> {
    if let Some(pipeline) = pipeline.as_ref() {
        if !buffer.is_empty() {
            pipeline
                .store()
                .upsert_embeddings(buffer)
                .await
                .with_context(|| "failed to persist note embeddings")?;
            buffer.clear();
        }
    }
    Ok(())
}

fn extract_aliases(extraction: &MetadataExtraction) -> Vec<String> {
    extraction
        .metadata
        .get("aliases")
        .and_then(|value| match value {
            Value::Array(items) => Some(
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(|alias| alias.trim()))
                    .filter(|alias| !alias.is_empty())
                    .map(|alias| alias.to_string())
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
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
            .map(|(key, value)| format!("{}: {}", key, value))
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

fn resolve_wikilinks(
    _source_id: &str,
    links: &[WikiLink],
    context: &ResolutionContext,
) -> Vec<LinkResolutionRecord> {
    links
        .iter()
        .map(|link| {
            let (target, reason) = match normalise_link(&link.target) {
                Some(candidate) => context.resolve(&candidate),
                None => (None, LinkReason::Unresolved),
            };
            LinkResolutionRecord {
                raw: link.raw.clone(),
                target,
                display: link.display.clone(),
                heading: link.heading.clone(),
                reason,
            }
        })
        .collect()
}

fn normalise_link(link: &str) -> Option<String> {
    let mut candidate =
        normalise_relative_str(link).or_else(|| normalise_relative_path(Path::new(link)))?;

    if candidate.as_os_str().is_empty() {
        return None;
    }

    if let Some(ext) = candidate.extension() {
        if ext.eq_ignore_ascii_case("md") {
            candidate.set_extension("");
        }
    }

    let value = candidate
        .to_string_lossy()
        .replace(char::from(b'\\'), "/")
        .trim()
        .trim_matches('/')
        .to_string();

    if value.is_empty() { None } else { Some(value) }
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
        };

        assert!(!note_requires_reindex(false, &entry, Some(&state), false));
        assert!(note_requires_reindex(false, &entry, Some(&state), true));
    }

    #[tokio::test]
    async fn note_requires_embedding_backfill_for_fresh_indexed_notes_without_vectors() {
        let (vault, database, _note_path) = temp_vault_with_note();
        let entry = vault
            .inventory()
            .expect("inventory")
            .into_iter()
            .find(|entry| entry.id == "Sample")
            .expect("sample note entry");
        let state = NoteIndexState {
            file_modified_at: entry.file_modified_at,
            indexed_at: entry.file_modified_at,
        };

        let descriptor = crate::embeddings::EmbeddingDescriptor::resolve("fast")
            .expect("resolve embedding descriptor");
        let (store, _) =
            crate::embeddings::EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor)
                .await
                .expect("bootstrap embedding store");

        assert!(
            note_requires_embedding_backfill(Some(&store), false, &entry, Some(&state))
                .await
                .expect("backfill check"),
            "fresh indexed note without an embedding should be reindexed"
        );

        let record = EmbeddingRecord {
            note_id: entry.id.clone(),
            vector: vec![0.25; descriptor.dimension()],
            indexed_at: entry.file_modified_at,
        };
        store
            .upsert_embeddings(&[record])
            .await
            .expect("upsert embedding");

        assert!(
            !note_requires_embedding_backfill(Some(&store), false, &entry, Some(&state))
                .await
                .expect("backfill check"),
            "fresh indexed note with an embedding should stay skippable"
        );
    }

    #[tokio::test]
    async fn index_note_only_updates_target() {
        let vault = fixture_vault();
        let database = temp_db();
        let indexer = Indexer::new(
            vault.clone(),
            database.clone(),
            IndexerConfig::default(),
            None,
        );

        indexer.index_all().await.expect("initial index");

        let before = database.note_state("Photography Equipment").expect("state");
        assert!(before.is_some());

        // Simulate stale note by forcing reindex of single note with force config.
        indexer
            .index_note("Photography Equipment")
            .await
            .expect("single note index");

        let after = database.note_state("Photography Equipment").expect("state");
        assert!(after.is_some());
        assert!(after.unwrap().indexed_at >= before.unwrap().indexed_at);
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
        assert_eq!(reason, LinkReason::Alias.as_str());
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
