//! Indexing orchestration.

use std::{collections::HashSet, path::Path, sync::Arc};

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use futures::{StreamExt, stream::FuturesUnordered};
use tokio::{sync::Semaphore, task};
use tracing::{error, info};

use crate::{
    IndexingStats, Vault,
    embeddings::EmbeddingPipeline,
    metadata::MetadataExtractor,
    sqlite::IndexDatabase,
    vault::{normalise_relative_path, normalise_relative_str},
};

#[cfg(feature = "vector-lancedb")]
use crate::{NoteRecord, metadata::MetadataExtraction};

#[cfg(feature = "vector-lancedb")]
use crate::embeddings::EmbeddingRecord;

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
        let note_ids = self.vault.list_note_ids()?;
        let total = note_ids.len() as u64;
        let note_set: Arc<HashSet<String>> = Arc::new(note_ids.iter().cloned().collect());

        let semaphore = Arc::new(Semaphore::new(self.config.parallelism.max(1)));
        let mut tasks = FuturesUnordered::new();

        for note_id in note_ids {
            let indexer = self.clone();
            let known = Arc::clone(&note_set);
            let semaphore = Arc::clone(&semaphore);

            tasks.push(tokio::spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .map_err(|err| anyhow!("indexer semaphore closed: {err}"))?;
                let result = indexer.run_single(note_id.clone(), known).await;
                Ok::<_, anyhow::Error>((note_id, result))
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
                Ok(NoteProcessing::Indexed(embedding)) => {
                    #[cfg(feature = "vector-lancedb")]
                    if let Some(record) = embedding {
                        if EmbeddingPipeline::is_supported() {
                            if let Some(pipeline) = &self.embeddings {
                                let buffer = vec![record];
                                pipeline
                                    .store()
                                    .upsert_embeddings(&buffer)
                                    .await
                                    .with_context(|| "failed to persist note embedding")?;
                            }
                        }
                    }
                    #[cfg(not(feature = "vector-lancedb"))]
                    let _ = embedding;
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
                    observer(IndexProgressEvent {
                        note_id,
                        processed,
                        total,
                        indexed: false,
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

        Ok(stats)
    }

    /// Reindexes a single note identified by the given ID.
    pub async fn index_note(&self, note_id: &str) -> Result<()> {
        let note_set = Arc::new(self.vault.list_note_ids()?.into_iter().collect());
        self.run_single(note_id.to_string(), note_set).await?;
        Ok(())
    }

    async fn run_single(
        &self,
        note_id: String,
        known_notes: Arc<HashSet<String>>,
    ) -> Result<NoteProcessing> {
        let indexer = self.clone();
        task::spawn_blocking(move || indexer.process_note(&note_id, &known_notes))
            .await
            .context("indexing task panicked")?
    }

    fn process_note(&self, note_id: &str, known_notes: &HashSet<String>) -> Result<NoteProcessing> {
        let note = self
            .vault
            .load_note(note_id)
            .with_context(|| format!("failed to load note {note_id}"))?;

        let state = self.database.note_state(note_id)?;
        let is_stale = if self.config.force {
            true
        } else if let Some(state) = state {
            state.file_modified_at < note.file_modified_at
        } else {
            true
        };

        if !is_stale {
            return Ok(NoteProcessing::Skipped);
        }

        let extraction = self.metadata.extract(&note)?;
        let resolved_links = resolve_wikilinks(&extraction.wikilinks, known_notes);
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
                                format!("failed to generate embedding for note {note_id}")
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
        info!(%note_id, "indexed note");
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
    use std::{path::Path, sync::Arc};

    use tempfile::TempDir;

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
}
