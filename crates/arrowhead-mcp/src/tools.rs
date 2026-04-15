//! MCP tool definitions
//!
//! Schemas and helper conversions for Model Context Protocol tools.

use std::path::PathBuf;

use arrowhead_core::{
    DeletedMetricRecord, LinkEdge, MetadataMap, MetricFileSummary, MetricRecordEntry, NoteRecord,
    SearchResult,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Common helper to render link edge data for responses.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkEdgePayload {
    /// Note that contains the outbound WikiLink.
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Resolved target note identifier, if available.
    pub target: Option<String>,
    /// Raw text of the link as written in the note.
    pub raw: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional display text override provided in the link ([[target|display]] syntax).
    pub display_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional heading fragment ([[target#Heading]]) extracted from the link.
    pub heading: Option<String>,
    /// Explanation of how the link target was resolved.
    pub reason: String,
}

impl LinkEdgePayload {
    /// Convert from a core `LinkEdge`.
    pub fn from_edge(edge: &LinkEdge) -> Self {
        Self {
            source: edge.source.clone(),
            target: edge.target.clone(),
            raw: edge.raw.clone(),
            display_text: edge.display_text.clone(),
            heading: edge.heading.clone(),
            reason: edge.reason.as_str().to_string(),
        }
    }
}

/// Request parameters for graph methods that target a single note.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphNoteParams {
    /// Identifier of the note whose graph context is requested.
    pub note_id: String,
}

/// Response payload for `mcp.graph.get_context`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphContextPayload {
    /// Identifier of the note the context belongs to.
    pub note_id: String,
    /// Inbound links referencing the note.
    pub backlinks: Vec<LinkEdgePayload>,
    /// Outbound links originating from the note.
    pub forward_links: Vec<LinkEdgePayload>,
}

/// Response payload for directional graph queries (backlinks/forward links).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphLinksPayload {
    /// Identifier of the note the links relate to.
    pub note_id: String,
    /// Collected link edges in the requested direction.
    pub links: Vec<LinkEdgePayload>,
}

/// Request parameters shared by all search methods.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct SearchParams {
    /// Query string to evaluate.
    pub query: String,
    /// Optional maximum number of results to return.
    pub limit: Option<usize>,
}

/// Response payload wrapping search results.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResultsPayload {
    /// Total number of results returned in this response.
    pub total: usize,
    /// Detailed results for each matched note.
    pub results: Vec<SearchResultPayload>,
}

/// Individual search result item.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResultPayload {
    /// Identifier of the matched note.
    pub note_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional title resolved for the note.
    pub title: Option<String>,
    /// Combined relevance score reported by the search engine.
    pub score: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Raw BM25 rank returned by the FTS index (lower is better).
    pub bm25: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Relative path of the note within the vault.
    pub relative_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional preview snippet generated from the note content.
    pub preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Human-readable explanation of why the note matched.
    pub reason: Option<String>,
    /// Metadata associated with the note.
    pub metadata: MetadataMap,
}

impl SearchResultPayload {
    /// Convert from a core `SearchResult`.
    pub fn from_result(result: &SearchResult) -> Self {
        Self {
            note_id: result.note_id.clone(),
            title: result.title.clone(),
            score: result.score,
            bm25: result.bm25_score(),
            relative_path: result.relative_path.clone(),
            preview: result.preview.clone(),
            reason: result.reason.clone(),
            metadata: result.metadata.clone(),
        }
    }
}

/// Parameters for reading a specific metric record.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MetricReadParams {
    /// Stable metric id or `metric:<id>` reference.
    pub metric_id: String,
}

/// Parameters for creating a metric record.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MetricCreateParams {
    /// Optional explicit target metrics file relative to the vault root.
    pub file_path: Option<PathBuf>,
    /// Optional stable metric id. Arrowhead generates one when omitted.
    pub id: Option<String>,
    /// RFC 3339 timestamp recorded for the metric event.
    pub ts: String,
    /// Metric key.
    pub key: String,
    /// Numeric metric value.
    pub value: f64,
    /// Source that produced the metric.
    pub source: String,
    /// Optional YYYY-MM-DD date bucket.
    pub date: Option<String>,
    /// Optional unit string.
    pub unit: Option<String>,
    /// Optional provenance id.
    pub origin_id: Option<String>,
    /// Optional human-authored note.
    pub note: Option<String>,
    /// Optional structured context object.
    pub context: Option<Map<String, Value>>,
    #[serde(default)]
    /// Optional tags attached to the metric row.
    pub tags: Vec<String>,
}

/// Parameters for updating a metric record.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MetricUpdateParams {
    /// Stable metric id or `metric:<id>` reference.
    pub metric_id: String,
    /// Optional replacement RFC 3339 timestamp.
    pub ts: Option<String>,
    /// Optional replacement metric key.
    pub key: Option<String>,
    /// Optional replacement numeric value.
    pub value: Option<f64>,
    /// Optional replacement source.
    pub source: Option<String>,
    /// Optional replacement YYYY-MM-DD date.
    pub date: Option<String>,
    #[serde(default)]
    /// Clear the `date` field entirely.
    pub clear_date: bool,
    /// Optional replacement unit.
    pub unit: Option<String>,
    #[serde(default)]
    /// Clear the `unit` field entirely.
    pub clear_unit: bool,
    /// Optional replacement provenance id.
    pub origin_id: Option<String>,
    #[serde(default)]
    /// Clear the `originId` field entirely.
    pub clear_origin_id: bool,
    /// Optional replacement note text.
    pub note: Option<String>,
    #[serde(default)]
    /// Clear the `note` field entirely.
    pub clear_note: bool,
    /// Optional replacement context object.
    pub context: Option<Map<String, Value>>,
    #[serde(default)]
    /// Clear the `context` field entirely.
    pub clear_context: bool,
    #[serde(default)]
    /// Replace the entire tag list with these values.
    pub tags: Vec<String>,
    #[serde(default)]
    /// Clear the tag list entirely.
    pub clear_tags: bool,
}

/// Parameters for deleting a metric record.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MetricDeleteParams {
    /// Stable metric id or `metric:<id>` reference.
    pub metric_id: String,
    #[serde(default)]
    /// Safety confirmation flag; must be true to delete.
    pub confirm: bool,
}

/// Response payload for `mcp.metrics.list_files`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsFilesPayload {
    /// Indexed metrics files.
    pub files: Vec<MetricFileSummary>,
}

/// Response payload for `mcp.metrics.read`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricReadPayload {
    /// Indexed metric record.
    pub record: MetricRecordEntry,
}

/// Response payload confirming a metric deletion.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricDeletePayload {
    /// Deleted metric descriptor.
    pub deleted: DeletedMetricRecord,
}

/// Response payload for `mcp.metrics.search`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsSearchResultsPayload {
    /// Total number of results returned in this response.
    pub total: usize,
    /// Detailed metrics records matching the query.
    pub results: Vec<MetricRecordEntry>,
}

/// Parameters for reading a specific note.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NoteReadParams {
    /// Identifier of the note to read.
    pub note_id: String,
}

/// Parameters for listing notes.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct NotesListParams {
    /// When true, omit additional note metadata and return identifiers only.
    pub ids_only: bool,
    /// Maximum number of entries to return.
    pub limit: Option<usize>,
}

/// Parameters for metadata lookups.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NoteMetadataParams {
    /// Identifier of the note whose metadata should be returned.
    pub note_id: String,
}

/// Response payload for `mcp.notes.read`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteContentPayload {
    /// Identifier of the note.
    pub note_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional note title.
    pub title: Option<String>,
    /// Parsed metadata extracted from the note.
    pub metadata: MetadataMap,
    /// Markdown body content with frontmatter removed.
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Raw note text including frontmatter (when requested).
    pub raw: Option<String>,
    /// Vault-relative filesystem path of the note.
    pub relative_path: PathBuf,
    /// Last modification timestamp recorded for the note file.
    pub file_modified_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Timestamp capturing when the note was created, if known.
    pub created_at: Option<DateTime<Utc>>,
}

impl NoteContentPayload {
    /// Construct from a note record.
    pub fn from_record(record: &NoteRecord, raw: Option<String>) -> Self {
        Self {
            note_id: record.id.clone(),
            title: record.title.clone(),
            metadata: record.metadata.clone(),
            content: record.content.clone(),
            raw,
            relative_path: record.relative_path.clone(),
            file_modified_at: record.file_modified_at,
            created_at: record.created_at,
        }
    }
}

/// List item returned from `mcp.notes.list`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteListItem {
    /// Identifier of the note entry.
    pub note_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional note title.
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Vault-relative path for the note file.
    pub relative_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Last modification timestamp if known.
    pub file_modified_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Creation timestamp if available.
    pub created_at: Option<DateTime<Utc>>,
}

/// Response wrapper for `mcp.notes.list`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotesListPayload {
    /// Collection of note summaries.
    pub notes: Vec<NoteListItem>,
}

/// Response payload for `mcp.notes.metadata`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteMetadataPayload {
    /// Identifier of the note.
    pub note_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional note title.
    pub title: Option<String>,
    /// Parsed metadata map.
    pub metadata: MetadataMap,
}

/// Summary entry returned when reporting orphan notes.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrphanNotePayload {
    /// Identifier of the orphan note.
    pub note_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional note title if available from the index.
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Vault-relative path to the note file.
    pub relative_path: Option<PathBuf>,
}

/// Response payload for `mcp.graph.find_orphans`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphOrphansPayload {
    /// Count of notes without inbound or outbound links.
    pub total: usize,
    /// Collected orphan note summaries.
    pub notes: Vec<OrphanNotePayload>,
}

/// Response payload for `mcp.graph.find_unresolved`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphUnresolvedPayload {
    /// Number of unresolved WikiLinks discovered.
    pub total: usize,
    /// Detailed unresolved link entries.
    pub links: Vec<LinkEdgePayload>,
}

/// Parameters for creating a new note.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NoteCreateParams {
    /// Explicit identifier for the new note. When omitted the title is normalised.
    pub note_id: Option<String>,
    /// Optional title stored in metadata; falls back to the identifier.
    pub title: Option<String>,
    /// Category helper mirroring CLI ergonomics.
    pub category: Option<String>,
    /// Markdown body content. Empty by default.
    pub content: Option<String>,
    #[serde(default)]
    /// Additional metadata to merge into the note frontmatter.
    pub metadata: Option<MetadataMap>,
}

/// Parameters for updating an existing note.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NoteUpdateParams {
    /// Identifier of the note to update.
    pub note_id: String,
    #[serde(default)]
    /// Replacement title written into metadata when supplied.
    pub title: Option<String>,
    #[serde(default)]
    /// Replacement Markdown body; omitted to retain existing content.
    pub content: Option<String>,
    #[serde(default)]
    /// Metadata entries to merge with the existing frontmatter.
    pub metadata: Option<MetadataMap>,
}

/// Parameters for deleting a note.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NoteDeleteParams {
    /// Identifier of the note slated for removal.
    pub note_id: String,
    #[serde(default)]
    /// Safety confirmation flag; must be true to delete.
    pub confirm: bool,
}

/// Response payload confirming a note deletion.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteDeletePayload {
    /// Identifier of the deleted note.
    pub note_id: String,
    /// Indicates whether the note file was removed.
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    /// Vault-relative directories that were pruned as a result of the deletion.
    pub pruned_directories: Vec<PathBuf>,
}

/// Strategy options for computing related notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelatedNotesStrategy {
    /// Automatically select the best strategy for the environment.
    Auto,
    /// Force semantic vector lookups.
    Semantic,
    /// Prefer graph-based neighbourhood analysis.
    Graph,
    /// Combine graph and semantic signals when available.
    Hybrid,
}

impl Default for RelatedNotesStrategy {
    fn default() -> Self {
        Self::Auto
    }
}

fn default_related_notes_strategy() -> RelatedNotesStrategy {
    RelatedNotesStrategy::Auto
}

/// Parameters for `mcp.discovery.get_related_notes`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct RelatedNotesParams {
    /// Anchor note identifier used as the similarity seed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Free-form query when no anchor note is provided.
    pub query: Option<String>,
    #[serde(default)]
    /// Maximum number of related notes to return.
    pub limit: Option<usize>,
    #[serde(default = "default_related_notes_strategy")]
    /// Strategy hint controlling which signals to prioritise.
    pub strategy: RelatedNotesStrategy,
}

/// Individual related note entry.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelatedNotePayload {
    /// Identifier of the related note.
    pub note_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional title associated with the note.
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional similarity score reported by the strategy.
    pub score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Explanation of why this note was surfaced.
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Selected metadata fields that may help drive follow-up prompts.
    pub metadata: Option<MetadataMap>,
}

/// Response payload for `mcp.discovery.get_related_notes`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelatedNotesPayload {
    /// Identifier of the anchor note supplied by the client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Query string supplied by the client.
    pub query: Option<String>,
    /// Strategy that produced the related notes.
    pub strategy: RelatedNotesStrategy,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Strategy used as a fallback when the requested one was unavailable.
    pub fallback_strategy: Option<RelatedNotesStrategy>,
    /// Collection of related notes ranked by relevance.
    pub related: Vec<RelatedNotePayload>,
}

/// Optional parameters controlling vault statistics aggregation.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct VaultStatsParams {
    /// Maximum number of recent notes to include in the response.
    pub recent_limit: Option<usize>,
}

/// Aggregated vault statistics for `mcp.discovery.get_vault_stats`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultStatsPayload {
    /// Timestamp when the statistics snapshot was generated.
    pub generated_at: DateTime<Utc>,
    /// Total number of markdown notes discovered in the vault.
    pub total_notes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Count of notes currently indexed by the Arrowhead daemon.
    pub indexed_notes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Count of notes that reported indexing errors (if known).
    pub error_notes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Approximate aggregate word count across the vault.
    pub total_words: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Average word count per note.
    pub average_words_per_note: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional summary of recently modified notes.
    pub recent_notes: Option<Vec<NoteListItem>>,
}

/// Summary of naming patterns detected in the vault.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NamingPatternSummary {
    /// Human-readable description of the naming pattern.
    pub pattern: String,
    /// Number of notes matching the pattern.
    pub count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    /// Representative examples illustrating the pattern.
    pub examples: Vec<String>,
}

/// Enumeration of metadata value kinds observed for a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetadataValueKind {
    /// JSON string values.
    String,
    /// Numeric values (integer or floating point).
    Number,
    /// Boolean values.
    Boolean,
    /// Ordered lists.
    Array,
    /// Nested JSON object.
    Object,
    /// Explicit null placeholder.
    Null,
}

/// Common metadata value entry along with frequency.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataCommonValue {
    /// Captured metadata value.
    pub value: Value,
    /// Number of notes that contained this value.
    pub count: usize,
}

/// Aggregated metadata statistics for a field.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataFieldStats {
    /// Field name as it appears in note frontmatter.
    pub field: String,
    /// Number of notes that specified the field.
    pub note_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    /// Value categories observed for the field.
    pub value_kinds: Vec<MetadataValueKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    /// Most common values ordered by frequency.
    pub common_values: Vec<MetadataCommonValue>,
}

/// User-provided Arrowhead guide surfaced via MCP.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StyleGuidePayload {
    /// Vault-relative path to the style guide document.
    pub relative_path: PathBuf,
    /// Raw Markdown content of the style guide.
    pub content: String,
}

/// Agent playbook bundled with Arrowhead distributions.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentsPlaybookPayload {
    /// Repository-relative path to the agent playbook document.
    pub relative_path: PathBuf,
    /// Raw Markdown content of the playbook.
    pub content: String,
}

/// Workspace configuration surfaced to conventions tooling.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSettingsPayload {
    /// Workspace flavour (e.g., `obsidian` or `generic`).
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Attachments directory relative to the vault root.
    pub attachments_folder: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    /// User-defined ignore list derived from Obsidian preferences.
    pub ignored_folders: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Daily note file name template if configured.
    pub daily_note_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Preferred internal link style (e.g., with or without file extension).
    pub link_style: Option<String>,
}

/// Metrics conventions surfaced to discovery tooling.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsConventionsPayload {
    /// Where the conventions were resolved from.
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Filesystem path that backed the resolved conventions, if any.
    pub source_path: Option<PathBuf>,
    /// Relative root directory searched for metrics files.
    pub root: PathBuf,
    /// File suffixes recognised as metrics files.
    pub extensions: Vec<String>,
    /// Relative default write file used when a target is omitted.
    pub default_write_file: PathBuf,
    /// Prefix used for references such as `metric:<id>`.
    pub record_reference_prefix: String,
    /// Week start day used by metrics time windows.
    pub week_start_day: String,
    /// Hour offset that determines when a new metrics day starts.
    pub day_start_hour: u8,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    /// Metrics files currently discovered under the configured root.
    pub files: Vec<PathBuf>,
}

/// Legacy Obsidian-specific payload retained for backward compatibility.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObsidianSettingsPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Attachments directory relative to the vault root.
    pub attachments_folder: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    /// User-defined ignore list derived from Obsidian preferences.
    pub ignored_folders: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Daily note file name template if configured.
    pub daily_note_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Preferred internal link style (e.g., with or without file extension).
    pub link_style: Option<String>,
}

/// Response payload for `mcp.discovery.get_vault_conventions`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultConventionsPayload {
    /// Detected naming patterns across the vault.
    pub naming_patterns: Vec<NamingPatternSummary>,
    /// Aggregated metadata field statistics.
    pub metadata_fields: Vec<MetadataFieldStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Legacy Obsidian-specific payload retained for older clients.
    pub obsidian: Option<ObsidianSettingsPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Snapshot of workspace settings useful for reasoning about note structure.
    pub workspace: Option<WorkspaceSettingsPayload>,
    /// Resolved metrics conventions and discovered metrics files.
    pub metrics: MetricsConventionsPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional user-authored style guide surfaced to agents.
    pub style_guide: Option<StyleGuidePayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Arrowhead agent operating manual bundled with the CLI.
    pub agents_playbook: Option<AgentsPlaybookPayload>,
}

/// Parameters supplied to the MCP `initialize` handshake.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitializeParams {
    /// Protocol version requested by the client.
    pub protocol_version: String,
    #[serde(default)]
    /// Capabilities advertised by the client.
    pub capabilities: Value,
    /// Descriptor of the connecting client.
    pub client_info: ImplementationDescriptor,
}

/// Basic implementation descriptor used by both client and server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImplementationDescriptor {
    /// Programmatic identifier for the implementation.
    pub name: String,
    /// Human-readable display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Semantic version string.
    pub version: String,
}

/// Capability flags advertised during `initialize`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilitiesPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional experimental capability payloads.
    pub experimental: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional logging capability descriptor.
    pub logging: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional completions capability descriptor.
    pub completions: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional prompt capability descriptor.
    pub prompts: Option<ListCapabilityPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional resource capability descriptor.
    pub resources: Option<ResourceCapabilityPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional tool capability descriptor.
    pub tools: Option<ToolCapabilityPayload>,
}

/// Capability payload describing list-style features.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListCapabilityPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Indicates whether list change notifications are supported.
    pub list_changed: Option<bool>,
}

/// Capability payload describing resource support.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceCapabilityPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Indicates subscription support for resource updates.
    pub subscribe: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Indicates whether list change notifications are supported.
    pub list_changed: Option<bool>,
}

/// Capability payload describing tool support.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCapabilityPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Indicates whether list change notifications are supported.
    pub list_changed: Option<bool>,
}

/// Snapshot of daemon health exposed during initialization.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStatusPayload {
    /// When the daemon status snapshot was recorded.
    pub updated_at: DateTime<Utc>,
    /// Total number of notes indexed by the daemon.
    pub indexed_notes: u64,
    /// Number of notes currently in an error state.
    pub error_notes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional description of the current activity.
    pub activity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Number of queued jobs if the daemon is busy.
    pub queued_jobs: Option<usize>,
}

/// Response payload for the spec `initialize` handshake.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResultPayload {
    /// Negotiated protocol version.
    pub protocol_version: String,
    /// Server capabilities advertised to the client.
    pub capabilities: ServerCapabilitiesPayload,
    /// Descriptor for the running Arrowhead MCP server.
    pub server_info: ImplementationDescriptor,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional textual instructions for clients and LLMs.
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional daemon status summary.
    pub daemon_status: Option<DaemonStatusPayload>,
}

/// Descriptor of a single MCP tool surfaced by Arrowhead.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDescriptor {
    /// Fully qualified tool identifier (e.g., `mcp.notes.read`).
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional human-friendly title.
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Short description of what the tool does.
    pub description: Option<String>,
    /// JSON Schema describing request parameters.
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// JSON Schema describing the response payload.
    pub output_schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional tool annotations surfaced to hosts.
    pub annotations: Option<Value>,
}

/// Response payload for `tools/list`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsListPayload {
    /// Tools exposed by the Arrowhead MCP server keyed by method name.
    pub tools: Vec<ToolDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Cursor to continue pagination when more tools are available.
    pub next_cursor: Option<String>,
}

/// Parameters for `tools/call`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CallToolParams {
    /// Name of the tool being invoked.
    pub name: String,
    #[serde(default)]
    /// Arguments supplied to the tool invocation.
    pub arguments: serde_json::Map<String, Value>,
    #[serde(default, rename = "_meta")]
    #[allow(dead_code)]
    /// Undocumented metadata field sent by some hosts; ignored for compatibility.
    pub meta: Option<Value>,
}

/// Text content block returned from tool invocations.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContentPayload {
    #[serde(rename = "type")]
    kind: &'static str,
    /// Text payload provided to the host/LLM.
    pub text: String,
}

impl TextContentPayload {
    /// Construct a text content block from the provided string.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            kind: "text",
            text: text.into(),
        }
    }
}

/// Response payload for `tools/call`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResultPayload {
    /// Content segments produced by the tool invocation.
    pub content: Vec<TextContentPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional structured data representation of the result.
    pub structured_content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Indicates whether the tool reported a recoverable error.
    pub is_error: Option<bool>,
}

impl CallToolResultPayload {
    /// Build a tool result from a structured payload.
    pub fn from_value(result: Value) -> Self {
        Self::from_value_with_message(result, None)
    }

    /// Build a tool result and optionally prepend a human-readable message.
    pub fn from_value_with_message(result: Value, message: Option<String>) -> Self {
        let structured_content = match &result {
            Value::Object(_) => Some(result.clone()),
            _ => None,
        };

        let mut content = Vec::new();
        if let Some(message) = message {
            if !message.is_empty() {
                content.push(TextContentPayload::new(message));
            }
        }

        let text = match &result {
            Value::Null => "null".to_string(),
            Value::Bool(flag) => flag.to_string(),
            Value::Number(num) => num.to_string(),
            Value::String(s) => s.clone(),
            Value::Array(_) | Value::Object(_) => {
                serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string())
            }
        };

        content.push(TextContentPayload::new(text));

        Self {
            content,
            structured_content,
            is_error: None,
        }
    }
}
