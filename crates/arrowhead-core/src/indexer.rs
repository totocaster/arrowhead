//! Indexing orchestration.

use std::sync::Arc;

use anyhow::{Result, bail};

use crate::{IndexingStats, Vault};

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
    config: IndexerConfig,
}

impl Indexer {
    /// Create a new indexer over a vault.
    pub fn new(vault: Arc<Vault>, config: IndexerConfig) -> Self {
        Self { vault, config }
    }

    /// Runs a full indexing pass across the vault.
    pub async fn index_all(&self) -> Result<IndexingStats> {
        let _ = &self.vault;
        let _ = &self.config;
        bail!("indexer pipeline not implemented yet")
    }

    /// Reindexes a single note identified by the given ID.
    pub async fn index_note(&self, _note_id: &str) -> Result<()> {
        bail!("single-note indexing not implemented yet")
    }
}
