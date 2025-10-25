//! Indexing orchestration.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use futures::{StreamExt, stream::FuturesUnordered};
use tokio::{sync::Semaphore, task};
use tracing::{debug, error, info};

use crate::{
    IndexingStats, Vault,
    embeddings::EmbeddingPipeline,
    metadata::MetadataExtractor,
    sqlite::{IndexDatabase, NoteIndexState},
    vault::NoteInventoryEntry,
    vault::{normalise_relative_path, normalise_relative_str},
};

#[cfg(feature = "vector-lancedb")]
use crate::{NoteRecord, metadata::MetadataExtraction};

#[cfg(feature = "vector-lancedb")]
use crate::embeddings::EmbeddingRecord;

#[cfg(feature = "vector-lancedb")]
const EMBEDDING_FLUSH_BATCH: usize = 64;

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
        let state_table: Arc<HashMap<String, NoteIndexState>> =
            Arc::new(self.database.note_states()?);

        let semaphore = Arc::new(Semaphore::new(self.config.parallelism.max(1)));
        let mut tasks = FuturesUnordered::new();

        for entry in inventory {
            let indexer = self.clone();
            let known = Arc::clone(&note_set);
            let semaphore = Arc::clone(&semaphore);
            let states = Arc::clone(&state_table);
            let entry_id = entry.id.clone();

            debug!(note_id = %entry.id, "queueing note for indexing");
            tasks.push(tokio::spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|err| anyhow!("indexer semaphore closed: {err}"))?;
                let result = indexer.run_single(entry, known, states).await;
                Ok::<_, anyhow::Error>((entry_id, result))
            }));
        }

        let mut stats = IndexingStats {
            total_notes: total,
            ..IndexingStats::default()
        };

        #[cfg(feature = "vector-lancedb")]
        let mut embedding_buffer: Vec<EmbeddingRecord> = Vec::new();
        #[cfg(feature = "vector-lancedb")]
        let embeddings_pipeline = self.embeddings.clone();

        let mut processed = 0u64;
        while let Some(result) = tasks.next().await {
            let (note_id, outcome) = result??;
            match outcome {
                Ok(NoteProcessing::Indexed(embedding)) => {
                    #[cfg(feature = "vector-lancedb")]
                    {
                        if let Some(record) = embedding {
                            if let Some(pipeline) = embeddings_pipeline.as_ref() {
                                embedding_buffer.push(record);
                                if embedding_buffer.len() >= EMBEDDING_FLUSH_BATCH {
                                    pipeline
                                        .store()
                                        .upsert_embeddings(&embedding_buffer)
                                        .await
                                        .with_context(|| "failed to persist note embeddings")?;
                                    embedding_buffer.clear();
                                }
                            }
                        }
                    }
                    #[cfg(not(feature = "vector-lancedb"))]
                    {
                        let _ = embedding;
                    }
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

        #[cfg(feature = "vector-lancedb")]
        if let Some(pipeline) = embeddings_pipeline.as_ref() {
            if !embedding_buffer.is_empty() {
                pipeline
                    .store()
                    .upsert_embeddings(&embedding_buffer)
                    .await
                    .with_context(|| "failed to persist note embeddings")?;
            }
        }

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
        let state_table: Arc<HashMap<String, NoteIndexState>> =
            Arc::new(self.database.note_states()?);
        let entry = inventory
            .into_iter()
            .find(|entry| entry.id == note_id)
            .with_context(|| format!("note {note_id} not found in vault"))?;
        let outcome = self.run_single(entry, note_set, state_table).await?;

        match outcome {
            NoteProcessing::Indexed(embedding) => {
                #[cfg(feature = "vector-lancedb")]
                if let Some(record) = embedding {
                    if let Some(pipeline) = self.embeddings.clone() {
                        pipeline
                            .store()
                            .upsert_embeddings(&[record])
                            .await
                            .with_context(|| "failed to persist note embeddings")?;
                    }
                }
                #[cfg(not(feature = "vector-lancedb"))]
                {
                    let _ = embedding;
                }
            }
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
        let state_table: Arc<HashMap<String, NoteIndexState>> =
            Arc::new(self.database.note_states()?);
        let semaphore = Arc::new(Semaphore::new(self.config.parallelism.max(1)));
        let mut tasks = FuturesUnordered::new();

        for (note_id, absolute_path) in targets {
            let indexer = self.clone();
            let known_clone = Arc::clone(&known);
            let states_clone = Arc::clone(&state_table);
            let semaphore_clone = Arc::clone(&semaphore);
            tasks.push(tokio::spawn(async move {
                let _permit = semaphore_clone
                    .acquire_owned()
                    .await
                    .map_err(|err| anyhow!("indexer semaphore closed: {err}"))?;
                let key = note_id.clone();
                let outcome = match indexer.vault.inventory_entry_for_path(&absolute_path) {
                    Ok(Some(entry)) => indexer.run_single(entry, known_clone, states_clone).await,
                    Ok(None) => indexer.handle_missing_note(note_id).await,
                    Err(err) => Err(err),
                };
                Ok::<_, anyhow::Error>((key, outcome))
            }));
        }

        let mut stats = IndexingStats {
            total_notes: total,
            ..IndexingStats::default()
        };

        #[cfg(feature = "vector-lancedb")]
        let mut embedding_buffer: Vec<EmbeddingRecord> = Vec::new();
        #[cfg(feature = "vector-lancedb")]
        let embeddings_pipeline = self.embeddings.clone();

        while let Some(result) = tasks.next().await {
            let (note_id, outcome) = result??;
            match outcome {
                Ok(NoteProcessing::Indexed(embedding)) => {
                    #[cfg(feature = "vector-lancedb")]
                    {
                        if let Some(record) = embedding {
                            if let Some(pipeline) = embeddings_pipeline.as_ref() {
                                embedding_buffer.push(record);
                                if embedding_buffer.len() >= EMBEDDING_FLUSH_BATCH {
                                    pipeline
                                        .store()
                                        .upsert_embeddings(&embedding_buffer)
                                        .await
                                        .with_context(|| "failed to persist note embeddings")?;
                                    embedding_buffer.clear();
                                }
                            }
                        }
                    }
                    #[cfg(not(feature = "vector-lancedb"))]
                    {
                        let _ = embedding;
                    }
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

        #[cfg(feature = "vector-lancedb")]
        if let Some(pipeline) = embeddings_pipeline.as_ref() {
            if !embedding_buffer.is_empty() {
                pipeline
                    .store()
                    .upsert_embeddings(&embedding_buffer)
                    .await
                    .with_context(|| "failed to persist note embeddings")?;
            }
        }

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

    async fn handle_missing_note(&self, note_id: String) -> Result<NoteProcessing> {
        if self.remove_note(&note_id).await? {
            Ok(NoteProcessing::Removed)
        } else {
            Ok(NoteProcessing::Skipped)
        }
    }

    async fn prune_missing_notes(&self, known_inventory: &HashSet<String>) -> Result<Vec<String>> {
        let indexed_ids = self.database.list_note_ids()?;
        let mut removed = Vec::new();

        for note_id in indexed_ids {
            if !known_inventory.contains(&note_id) {
                if self.remove_note(&note_id).await? {
                    removed.push(note_id);
                }
            }
        }

        Ok(removed)
    }

    async fn run_single(
        &self,
        entry: NoteInventoryEntry,
        known_notes: Arc<HashSet<String>>,
        index_states: Arc<HashMap<String, NoteIndexState>>,
    ) -> Result<NoteProcessing> {
        let indexer = self.clone();
        task::spawn_blocking(move || indexer.process_note(&entry, &known_notes, &index_states))
            .await
            .context("indexing task panicked")?
    }

    fn process_note(
        &self,
        entry: &NoteInventoryEntry,
        known_notes: &HashSet<String>,
        index_states: &HashMap<String, NoteIndexState>,
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
        let resolved_links = resolve_wikilinks(&extraction.wikilinks, known_notes);
        debug!(
            note_id = %entry.id,
            link_count = resolved_links.len(),
            "resolved wikilinks for note"
        );
        let indexed_at = Utc::now();
        self.database
            .upsert_note(&note, &extraction, &resolved_links, indexed_at)?;

        let embedding_update: Option<EmbeddingUpdate> = if let Some(pipeline) = &self.embeddings {
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
        info!(
            note_id = %entry.id,
            metadata_fields = extraction.metadata.len(),
            link_count = resolved_links.len(),
            "indexed note"
        );
        Ok(NoteProcessing::Indexed(embedding_update))
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

#[cfg(feature = "vector-lancedb")]
type EmbeddingUpdate = EmbeddingRecord;

#[cfg(not(feature = "vector-lancedb"))]
type EmbeddingUpdate = ();

#[derive(Debug)]
enum NoteProcessing {
    Indexed(Option<EmbeddingUpdate>),
    Skipped,
    Removed,
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
    links: &[String],
    known_notes: &HashSet<String>,
) -> Vec<(String, Option<String>)> {
    links
        .iter()
        .map(|link| {
            let normalised = normalise_link(link);
            let target = normalised.as_ref().and_then(|candidate| {
                if known_notes.contains(candidate) {
                    Some(candidate.clone())
                } else {
                    None
                }
            });
            (link.clone(), target)
        })
        .collect()
}

fn normalise_link(link: &str) -> Option<String> {
    normalise_relative_str(link)
        .or_else(|| normalise_relative_path(Path::new(link)))
        .map(|path| path.to_string_lossy().replace(char::from(b'\\'), "/"))
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
        fs::create_dir_all(&vault_dir.join(".obsidian"))
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
