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
pub mod metrics;
pub mod query;
pub mod search;
pub mod sqlite;
pub mod status;
pub mod types;
pub mod vault;
pub mod workspace;

// Re-export commonly used types for convenience across crates.
pub use graph::{GraphContext, GraphService, LinkEdge, LinkReason, LinkResolutionRecord};
pub use indexer::IndexProgressEvent;
pub use metrics::{
    DEFAULT_DAY_START_HOUR, DEFAULT_METRIC_REFERENCE_PREFIX, DEFAULT_METRICS_EXTENSION,
    DEFAULT_METRICS_ROOT, DEFAULT_METRICS_WRITE_FILE_NAME, DEFAULT_WEEK_START_DAY,
    MetricsConfigFile, MetricsConventions, MetricsConventionsSource, MetricsFileEntry,
};
pub use search::{SearchConfig, SearchResult, SearchService};
pub use status::{
    ActivityState, ActivityStatus, DAEMON_STATUS_VERSION, DaemonStatus, DownloadState,
    DownloadStatus, IssueSeverity, StatusFrame, StatusIssue,
};
pub use types::{IndexingStats, MetadataMap, NoteId, NoteRecord, VaultPaths};
pub use vault::{InventorySnapshot, Vault, VaultConfig, VaultSettings};
pub use workspace::{WORKSPACE_CONFIG_FILE, WorkspaceFile, WorkspaceKind, WorkspaceSource};
