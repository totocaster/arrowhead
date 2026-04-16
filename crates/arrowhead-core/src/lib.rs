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

pub mod context;
pub mod embeddings;
pub mod graph;
pub mod indexer;
pub mod metadata;
pub mod metrics;
pub mod metrics_mutation;
pub mod metrics_service;
pub mod query;
pub mod search;
pub mod sqlite;
pub mod status;
pub mod types;
pub mod vault;
pub mod workspace;

// Re-export commonly used types for convenience across crates.
pub use context::{
    ContextActivity, ContextAttention, ContextAttentionItem, ContextHistory, ContextLink,
    ContextLinkKind, ContextMetricItem, ContextNoteItem, ContextPayload, ContextPivot,
    ContextRelated, ContextService, ContextSummary, ContextTargetKind,
    DEFAULT_CONTEXT_METRIC_LIMIT, DEFAULT_CONTEXT_NOTE_LIMIT, MonthContextSelector,
    WeekContextSelector,
};
pub use graph::{GraphContext, GraphService, LinkEdge, LinkReason, LinkResolutionRecord};
pub use indexer::IndexProgressEvent;
pub use metrics::{
    DEFAULT_DAY_START_HOUR, DEFAULT_METRIC_REFERENCE_PREFIX, DEFAULT_METRICS_EXTENSION,
    DEFAULT_METRICS_ROOT, DEFAULT_METRICS_WRITE_FILE_NAME, DEFAULT_WEEK_START_DAY, MetricIssueCode,
    MetricIssueSeverity, MetricRecord, MetricValidationIssue, MetricValidationStatus,
    MetricsConfigFile, MetricsConventions, MetricsConventionsSource, MetricsFileEntry,
    ParsedMetricRow, parse_metrics_file, parse_metrics_line, parse_metrics_reader,
};
pub use metrics_mutation::{
    AssignedMetricIdsFile, AssignedMetricIdsSummary, CreatedMetricFile, DeletedMetricFile,
    DeletedMetricRecord, MetricCreateRequest, MetricUpdateRequest, MetricsMutationService,
    PatchValue, RenamedMetricFile,
};
pub use metrics_service::{
    DEFAULT_METRICS_SEARCH_LIMIT, MetricFileSummary, MetricRecordEntry, MetricsQuery,
    MetricsService, parse_metrics_query,
};
pub use search::{SearchConfig, SearchResult, SearchService};
pub use status::{
    ActivityState, ActivityStatus, DAEMON_STATUS_VERSION, DaemonStatus, DownloadState,
    DownloadStatus, IssueSeverity, StatusFrame, StatusIssue,
};
pub use types::{IndexingStats, MetadataMap, NoteId, NoteRecord, VaultPaths};
pub use vault::{InventorySnapshot, Vault, VaultConfig, VaultSettings};
pub use workspace::{WORKSPACE_CONFIG_FILE, WorkspaceFile, WorkspaceKind, WorkspaceSource};
