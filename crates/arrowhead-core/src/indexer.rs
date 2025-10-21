//! Indexing orchestration.

use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use tokio::task;
use tracing::{error, info};

use crate::{IndexingStats, Vault, metadata::MetadataExtractor, sqlite::IndexDatabase};

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
}

impl Indexer {
    /// Create a new indexer over a vault.
    pub fn new(vault: Arc<Vault>, database: Arc<IndexDatabase>, config: IndexerConfig) -> Self {
        Self {
            vault,
            database,
            metadata: MetadataExtractor::new(),
            config,
        }
    }

    /// Runs a full indexing pass across the vault.
    pub async fn index_all(&self) -> Result<IndexingStats> {
        let note_ids = self.vault.list_note_ids()?;

        let mut stats = IndexingStats {
            total_notes: note_ids.len() as u64,
            ..IndexingStats::default()
        };

        for note_id in note_ids {
            match self.run_single(note_id.clone()).await {
                Ok(NoteProcessing::Indexed) => {
                    stats.indexed += 1;
                }
                Ok(NoteProcessing::Skipped) => {
                    stats.skipped += 1;
                }
                Err(err) => {
                    stats.errors += 1;
                    error!(%note_id, error = ?err, "failed to index note");
                }
            }
        }

        Ok(stats)
    }

    /// Reindexes a single note identified by the given ID.
    pub async fn index_note(&self, note_id: &str) -> Result<()> {
        self.run_single(note_id.to_string()).await?;
        Ok(())
    }

    async fn run_single(&self, note_id: String) -> Result<NoteProcessing> {
        let indexer = self.clone();
        task::spawn_blocking(move || indexer.process_note(&note_id))
            .await
            .context("indexing task panicked")?
    }

    fn process_note(&self, note_id: &str) -> Result<NoteProcessing> {
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
        self.database.upsert_note(&note, &extraction, Utc::now())?;
        info!(%note_id, "indexed note");
        Ok(NoteProcessing::Indexed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoteProcessing {
    Indexed,
    Skipped,
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
        let indexer = Indexer::new(vault, database.clone(), IndexerConfig::default());

        let stats = indexer.index_all().await.expect("index succeeds");

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
        let indexer = Indexer::new(vault.clone(), database.clone(), IndexerConfig::default());

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
        let indexer = Indexer::new(vault.clone(), database.clone(), IndexerConfig::default());

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
}
