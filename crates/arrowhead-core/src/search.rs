//! Search coordination across FTS, semantic, and hybrid strategies.

use anyhow::{Result, bail};

use crate::{MetadataMap, NoteId};

/// Unified search result payload spanning the different search modes.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    /// Identifier of the matched note.
    pub note_id: NoteId,
    /// Combined relevance score (mode-specific meaning).
    pub score: f32,
    /// Optional snippet or preview text.
    pub preview: Option<String>,
    /// Metadata attached to the note, useful for display.
    pub metadata: MetadataMap,
}

impl SearchResult {
    /// Create a result placeholder while the real implementation is pending.
    pub fn placeholder(note_id: NoteId) -> Self {
        Self {
            note_id,
            score: 0.0,
            preview: None,
            metadata: MetadataMap::default(),
        }
    }
}

/// Configuration parameters shared by all search modes.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchConfig {
    /// Default number of results to return when a limit is not provided.
    pub default_limit: usize,
    /// Minimum similarity score for semantic matches.
    pub semantic_threshold: f32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            default_limit: 10,
            semantic_threshold: 0.3,
        }
    }
}

/// Public entry point for executing searches.
#[derive(Debug, Clone)]
pub struct SearchService {
    config: SearchConfig,
}

impl SearchService {
    /// Create a new search service with the supplied configuration.
    pub fn new(config: SearchConfig) -> Self {
        Self { config }
    }

    /// Execute a full-text search query.
    pub async fn search_fts(
        &self,
        _query: &str,
        _limit: Option<usize>,
    ) -> Result<Vec<SearchResult>> {
        bail!("full-text search not implemented yet")
    }

    /// Execute a semantic similarity search.
    pub async fn search_semantic(
        &self,
        _query: &str,
        _limit: Option<usize>,
    ) -> Result<Vec<SearchResult>> {
        bail!("semantic search not implemented yet")
    }

    /// Execute a hybrid search, combining semantic and keyword results.
    pub async fn search_hybrid(
        &self,
        _query: &str,
        _limit: Option<usize>,
    ) -> Result<Vec<SearchResult>> {
        bail!("hybrid search not implemented yet")
    }

    /// Access the current search configuration.
    pub fn config(&self) -> &SearchConfig {
        &self.config
    }
}
