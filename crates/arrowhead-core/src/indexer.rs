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
    IndexingStats, NoteRecord, Vault,
    embeddings::{EmbeddingPipeline, EmbeddingRecord},
    graph::{LinkReason, LinkResolutionRecord, normalise_link_lookup},
    metadata::{MetadataExtraction, MetadataExtractor, WikiLink},
    sqlite::{IndexDatabase, LinkResolutionMaps, NoteIndexState},
    vault::NoteInventoryEntry,
    vault::{normalise_relative_path, normalise_relative_str},
};

#[cfg(feature = "vector-lancedb")]
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
        for path in paths {
            if let Some((note_id, relative)) = self.vault.normalise_note_path(path) {
                let absolute = self.vault.note_path(&relative);
                targets.insert(note_id, absolute);
            }
        }

        if targets.is_empty() {
            debug!("reindex_paths called with no resolvable markdown notes");
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

        Ok(stats)
    }

    /// Remove a note from the index and associated vector stores.
    pub async fn remove_note(&self, note_id: &str) -> Result<bool> {
        let existed = self.database.remove_note(note_id)?;

        #[cfg(feature = "vector-lancedb")]
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
        let indexer = self.clone();
        let dispatch = tracing::dispatcher::get_default(|current| current.clone());
        task::spawn_blocking(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                indexer.process_note(&entry, &resolution, &index_states, &write_tx)
            })
        })
        .await
        .context("indexing task panicked")?
    }

    fn process_note(
        &self,
        entry: &NoteInventoryEntry,
        resolution: &ResolutionContext,
        index_states: &HashMap<String, NoteIndexState>,
        write_tx: &WriteSender,
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
        let is_stale = if self.config.force {
            true
        } else if let Some(state) = state {
            state.file_modified_at < entry.file_modified_at
        } else {
            true
        };

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
            #[cfg(feature = "vector-lancedb")]
            {
                if EmbeddingPipeline::is_supported() {
                    let context = compose_embedding_text(&note, &extraction);
                    let vector =
                        pipeline
                            .generator()
                            .embed_document(&context)
                            .with_context(|| {
                                format!("failed to generate embedding for note {}", entry.id)
                            })?;
                    Some(EmbeddingRecord {
                        note_id: note.id.clone(),
                        vector,
                        indexed_at,
                    })
                } else {
                    None
                }
            }
            #[cfg(not(feature = "vector-lancedb"))]
            {
                let _ = pipeline;
                None
            }
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

#[cfg(feature = "vector-lancedb")]
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

#[cfg(not(feature = "vector-lancedb"))]
#[allow(clippy::ptr_arg)]
async fn handle_embedding(
    _pipeline: &Option<Arc<EmbeddingPipeline>>,
    _buffer: &mut Vec<EmbeddingRecord>,
    _embedding: Option<EmbeddingRecord>,
) -> Result<()> {
    Ok(())
}

#[cfg(feature = "vector-lancedb")]
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

#[cfg(not(feature = "vector-lancedb"))]
#[allow(clippy::ptr_arg)]
async fn flush_embedding_buffer(
    _pipeline: &Option<Arc<EmbeddingPipeline>>,
    _buffer: &mut Vec<EmbeddingRecord>,
) -> Result<()> {
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

#[cfg(feature = "vector-lancedb")]
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
}
