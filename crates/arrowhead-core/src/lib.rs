//! Arrowhead Core Library
//!
//! Core functionality for Obsidian vault indexing, search, and graph navigation.
//!
//! This library provides:
//! - Vault operations (read/write markdown notes)
//! - Metadata extraction from YAML frontmatter and inline tags
//! - Full-text search with SQLite FTS5
//! - Semantic search with vector embeddings
//! - WikiLinks graph navigation
//! - Smart indexing with staleness detection

#![warn(missing_docs)]

pub mod embeddings;
pub mod graph;
pub mod indexer;
pub mod metadata;
pub mod search;
pub mod sqlite;
pub mod status;
pub mod types;
pub mod vault;

// Re-export commonly used types for convenience across crates.
pub use graph::{GraphContext, GraphService, LinkEdge, LinkReason, LinkResolutionRecord};
pub use indexer::IndexProgressEvent;
pub use search::{SearchConfig, SearchResult, SearchService};
pub use status::{
    ActivityState, ActivityStatus, DEAMON_STATUS_VERSION, DeamonStatus, DownloadState,
    DownloadStatus, IssueSeverity, StatusIssue,
};
pub use types::{IndexingStats, MetadataMap, NoteId, NoteRecord, VaultPaths};
pub use vault::{InventorySnapshot, Vault, VaultConfig, VaultSettings};
