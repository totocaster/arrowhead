//! Shared runtime state for the MCP server.
//!
//! Responsible for initialising vault handles, database connections, search
//! services, and daemon health checks.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::{Arc, LazyLock},
};

use anyhow::{Context, Result, anyhow, bail};
use arrowhead_core::vault::NoteInventoryEntry;
use arrowhead_core::{
    GraphService, InventorySnapshot, MetadataMap, NoteRecord, SearchConfig, SearchService, Vault,
    VaultConfig, VaultPaths,
    sqlite::IndexDatabase,
    status::{DaemonStatus, IssueSeverity, StatusIssue},
    workspace::WorkspaceKind,
};
use arrowhead_daemon::{ControlRequest, ControlResponse, send_control_request};
use chrono::Utc;
use regex::Regex;
use serde_json::Value;
use tokio::{sync::RwLock, task};
use tracing::warn;

use crate::tools::{
    AgentsPlaybookPayload, MetadataCommonValue, MetadataFieldStats, MetadataValueKind,
    MetricsConventionsPayload, NamingPatternSummary, NoteListItem, ObsidianSettingsPayload,
    RelatedNotePayload, RelatedNotesPayload, RelatedNotesStrategy, StyleGuidePayload,
    VaultConventionsPayload, VaultStatsPayload, WorkspaceSettingsPayload,
};

use arrowhead_core::SearchResult;
use arrowhead_core::embeddings::EmbeddingPipeline;

const AGENTS_PLAYBOOK_CONTENT: &str = include_str!("../../../AGENTS.md");

/// Configuration options used to bootstrap the MCP runtime.
#[derive(Debug, Clone)]
pub struct RuntimeOptions {
    /// Root path of the Obsidian vault to operate on.
    pub vault_path: PathBuf,
    /// Identifier of the embedding model to initialise (if supported).
    pub embedding_model: Option<String>,
    /// Optional override for the daemon control socket path.
    pub daemon_socket: Option<PathBuf>,
    /// Optional override for the daemon status file.
    pub daemon_status: Option<PathBuf>,
}

impl RuntimeOptions {
    /// Create options pointing at the supplied vault path.
    pub fn new(vault_path: PathBuf) -> Self {
        Self {
            vault_path,
            embedding_model: None,
            daemon_socket: None,
            daemon_status: None,
        }
    }

    /// Set the embedding model identifier.
    #[must_use]
    pub fn with_embedding_model(mut self, model: Option<String>) -> Self {
        self.embedding_model = model;
        self
    }

    /// Override the daemon control socket path.
    #[must_use]
    pub fn with_daemon_socket(mut self, path: Option<PathBuf>) -> Self {
        self.daemon_socket = path;
        self
    }

    /// Override the daemon status file path.
    #[must_use]
    pub fn with_daemon_status(mut self, path: Option<PathBuf>) -> Self {
        self.daemon_status = path;
        self
    }
}

/// Aggregated runtime state shared by MCP handlers.
#[derive(Debug, Clone)]
pub struct McpRuntime {
    vault: Arc<Vault>,
    database: Arc<IndexDatabase>,
    graph: GraphService,
    search: SearchService,
    daemon: DaemonClient,
    semantic_enabled: bool,
    daemon_status_cache: Arc<RwLock<Option<DaemonStatus>>>,
}

impl McpRuntime {
    /// Build a runtime from the supplied options.
    pub async fn initialise(options: RuntimeOptions) -> Result<Self> {
        let vault = Arc::new(Vault::new(VaultConfig::new(options.vault_path))?);
        vault.ensure_arrowhead_dirs()?;

        let db_path = vault.paths().arrowhead_dir.join("index.db");
        let database = Arc::new(IndexDatabase::open(&db_path)?);
        let graph = GraphService::new(Arc::clone(&database));

        let embedding_model = options.embedding_model.as_ref().and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });

        let embeddings = if let Some(model_id) = embedding_model {
            let pipeline =
                EmbeddingPipeline::initialise(vault.as_ref(), Arc::clone(&database), &model_id)
                    .await
                    .with_context(|| {
                        format!("failed to prepare embedding pipeline '{model_id}'")
                    })?;
            Some(Arc::new(pipeline))
        } else {
            None
        };
        let semantic_enabled = embeddings.is_some();
        let search = SearchService::new(
            Arc::clone(&database),
            SearchConfig::default(),
            embeddings.clone(),
        );

        let vault_paths = vault.paths().clone();
        let socket_path = options
            .daemon_socket
            .unwrap_or_else(|| vault_paths.arrowhead_dir.join("daemon/control.sock"));
        let status_path = options
            .daemon_status
            .unwrap_or_else(|| vault_paths.arrowhead_dir.join("daemon/status.json"));

        let daemon = DaemonClient::new(socket_path, status_path);

        Ok(Self {
            vault,
            database,
            graph,
            search,
            daemon,
            semantic_enabled,
            daemon_status_cache: Arc::new(RwLock::new(None)),
        })
    }

    /// Access the vault handle.
    #[must_use]
    pub fn vault(&self) -> &Arc<Vault> {
        &self.vault
    }

    /// Access the SQLite index database.
    #[must_use]
    pub fn database(&self) -> &Arc<IndexDatabase> {
        &self.database
    }

    /// Access the graph service.
    #[must_use]
    pub fn graph_service(&self) -> &GraphService {
        &self.graph
    }

    /// Access the search service.
    #[must_use]
    pub fn search_service(&self) -> &SearchService {
        &self.search
    }

    /// Access the daemon client.
    #[must_use]
    pub fn daemon(&self) -> &DaemonClient {
        &self.daemon
    }

    /// Determine whether semantic search is available.
    #[must_use]
    pub fn semantic_search_enabled(&self) -> bool {
        self.semantic_enabled
    }

    /// Build (or refresh) the vault inventory snapshot on a blocking thread.
    pub async fn inventory_snapshot(&self) -> Result<InventorySnapshot> {
        let vault = Arc::clone(&self.vault);
        task::spawn_blocking(move || vault.inventory_snapshot())
            .await
            .context("inventory snapshot task aborted")?
    }

    /// Load a note record from the vault on a blocking thread.
    pub async fn load_note_record(&self, note_id: &str) -> Result<NoteRecord> {
        let vault = Arc::clone(&self.vault);
        let note_id = note_id.to_string();
        task::spawn_blocking(move || vault.load_note(&note_id))
            .await
            .context("note load task aborted")?
    }

    /// Retrieve the most recently cached daemon status, refreshing it if required.
    pub async fn cached_daemon_status(&self) -> Option<DaemonStatus> {
        if let Some(status) = self.daemon_status_cache.read().await.clone() {
            return Some(status);
        }

        match self.daemon.status().await {
            Ok(status) => {
                let mut cache = self.daemon_status_cache.write().await;
                *cache = Some(status.clone());
                Some(status)
            }
            Err(err) => {
                warn!(error = %err, "failed to refresh daemon status");
                None
            }
        }
    }

    /// Force a daemon status refresh and update the cache.
    pub async fn refresh_daemon_status(&self) -> Result<DaemonStatus> {
        let status = self.daemon.status().await?;
        let mut cache = self.daemon_status_cache.write().await;
        *cache = Some(status.clone());
        Ok(status)
    }

    async fn note_titles(&self, note_ids: &[String]) -> Result<HashMap<String, Option<String>>> {
        if note_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let ids = note_ids.to_vec();
        let database = Arc::clone(&self.database);
        task::spawn_blocking(move || database.titles_for_notes(&ids))
            .await
            .context("note titles task aborted")?
    }

    async fn metadata_for_notes(
        &self,
        note_ids: &[String],
    ) -> Result<HashMap<String, MetadataMap>> {
        if note_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let ids = note_ids.to_vec();
        let database = Arc::clone(&self.database);
        task::spawn_blocking(move || database.metadata_for_notes(&ids))
            .await
            .context("metadata task aborted")?
    }

    /// Compute high-level vault statistics such as total notes and word counts.
    pub async fn compute_vault_stats(&self, recent_limit: usize) -> Result<VaultStatsPayload> {
        let snapshot = self.inventory_snapshot().await?;
        let total_notes = snapshot.entries().len();

        let entries: Vec<NoteInventoryEntry> = snapshot.entries().to_vec();
        let mut recent_entries = entries.clone();
        recent_entries.sort_by(|a, b| b.file_modified_at.cmp(&a.file_modified_at));
        let recent_entries = recent_entries
            .into_iter()
            .take(recent_limit.max(1))
            .collect::<Vec<_>>();

        let recent_ids: Vec<String> = recent_entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        let recent_titles = self.note_titles(&recent_ids).await?;

        let recent_notes = recent_entries
            .into_iter()
            .map(|entry| NoteListItem {
                note_id: entry.id.clone(),
                title: recent_titles.get(&entry.id).cloned().unwrap_or(None),
                relative_path: Some(entry.relative_path.clone()),
                file_modified_at: Some(entry.file_modified_at),
                created_at: entry.created_at,
            })
            .collect::<Vec<_>>();

        let total_words = if total_notes == 0 {
            0_u64
        } else {
            task::spawn_blocking(move || -> u64 {
                let mut total = 0_u64;
                for entry in entries {
                    match fs::read_to_string(&entry.absolute_path) {
                        Ok(content) => {
                            total += count_words(&content) as u64;
                        }
                        Err(err) => {
                            warn!(
                                path = %entry.absolute_path.display(),
                                error = %err,
                                "failed to read note while computing vault stats"
                            );
                        }
                    }
                }
                total
            })
            .await
            .context("word count task aborted")?
        };

        let daemon_status = self.cached_daemon_status().await;
        let indexed_notes = daemon_status.as_ref().map(|status| status.indexed_notes);
        let error_notes = daemon_status.as_ref().map(|status| status.error_notes);

        let average_words_per_note = if total_notes > 0 && total_words > 0 {
            Some(total_words as f32 / total_notes as f32)
        } else {
            None
        };

        Ok(VaultStatsPayload {
            generated_at: Utc::now(),
            total_notes,
            indexed_notes,
            error_notes,
            total_words: (total_words > 0).then_some(total_words),
            average_words_per_note,
            recent_notes: if recent_notes.is_empty() {
                None
            } else {
                Some(recent_notes)
            },
        })
    }

    /// Load the optional Arrowhead guide stored at `ARROWHEAD.md`.
    pub async fn load_style_guide(&self) -> Result<Option<StyleGuidePayload>> {
        let vault = Arc::clone(&self.vault);
        task::spawn_blocking(move || -> Result<Option<StyleGuidePayload>> {
            let paths: VaultPaths = vault.paths().clone();
            let style_path = paths.root.join("ARROWHEAD.md");
            if !style_path.exists() {
                return Ok(None);
            }

            let content = fs::read_to_string(&style_path)
                .with_context(|| format!("failed to read style guide {}", style_path.display()))?;

            let relative_path = style_path
                .strip_prefix(&paths.root)
                .unwrap_or(&style_path)
                .to_path_buf();

            Ok(Some(StyleGuidePayload {
                relative_path,
                content,
            }))
        })
        .await
        .context("style guide task aborted")?
    }

    /// Analyse the vault for naming patterns and metadata conventions.
    pub async fn compute_vault_conventions(&self) -> Result<VaultConventionsPayload> {
        let snapshot = self.inventory_snapshot().await?;
        let entries: Vec<NoteInventoryEntry> = snapshot.entries().to_vec();
        let note_ids: Vec<String> = entries.iter().map(|entry| entry.id.clone()).collect();

        let metadata_map = self.metadata_for_notes(&note_ids).await?;
        let naming_patterns = detect_naming_patterns(&entries);
        let metadata_fields = aggregate_metadata_fields(&metadata_map);
        let style_guide = self.load_style_guide().await?;
        let workspace = build_workspace_settings(self.vault.as_ref());
        let obsidian = build_obsidian_settings_legacy(self.vault.as_ref());
        let metrics = self.metrics_conventions_payload().await?;

        Ok(VaultConventionsPayload {
            naming_patterns,
            metadata_fields,
            obsidian,
            workspace,
            metrics,
            style_guide,
            agents_playbook: Some(load_agents_playbook()),
        })
    }

    async fn metrics_conventions_payload(&self) -> Result<MetricsConventionsPayload> {
        let vault = Arc::clone(&self.vault);
        let files = task::spawn_blocking(move || vault.metrics_files())
            .await
            .context("metrics discovery task aborted")??;
        let conventions = self.vault.metrics_conventions();

        Ok(MetricsConventionsPayload {
            source: conventions.source.as_str().to_string(),
            source_path: conventions.source.path().cloned(),
            root: conventions.root.clone(),
            extensions: conventions.extensions.clone(),
            default_write_file: conventions.default_write_file.clone(),
            record_reference_prefix: conventions.record_reference_prefix.clone(),
            week_start_day: conventions.week_start_day.clone(),
            day_start_hour: conventions.day_start_hour,
            files: files.into_iter().map(|entry| entry.relative_path).collect(),
        })
    }

    /// Determine notes related to the supplied anchor, choosing an appropriate strategy.
    pub async fn compute_related_notes(
        &self,
        note_id: &str,
        limit: Option<usize>,
        strategy: RelatedNotesStrategy,
    ) -> Result<RelatedNotesPayload> {
        let anchor = self.load_note_record(note_id).await?;
        let limit = limit.unwrap_or(5).max(1);

        let mut effective_strategy = match strategy {
            RelatedNotesStrategy::Auto => {
                if self.semantic_search_enabled() {
                    RelatedNotesStrategy::Semantic
                } else {
                    RelatedNotesStrategy::Graph
                }
            }
            other => other,
        };

        let mut fallback_strategy = None;
        if !self.semantic_search_enabled()
            && matches!(
                effective_strategy,
                RelatedNotesStrategy::Semantic | RelatedNotesStrategy::Hybrid
            )
        {
            fallback_strategy = Some(effective_strategy);
            effective_strategy = RelatedNotesStrategy::Graph;
        }

        let mut related = match effective_strategy {
            RelatedNotesStrategy::Graph => self.related_notes_graph(&anchor.id, limit).await?,
            RelatedNotesStrategy::Semantic => self.related_notes_semantic(&anchor, limit).await?,
            RelatedNotesStrategy::Hybrid => self.related_notes_hybrid(&anchor, limit).await?,
            RelatedNotesStrategy::Auto => {
                unreachable!("auto strategy should be resolved before dispatching")
            }
        };

        if related.is_empty() && !matches!(effective_strategy, RelatedNotesStrategy::Graph) {
            let graph_related = self.related_notes_graph(&anchor.id, limit).await?;
            if !graph_related.is_empty() {
                fallback_strategy = Some(effective_strategy);
                effective_strategy = RelatedNotesStrategy::Graph;
                related = graph_related;
            }
        }

        Ok(RelatedNotesPayload {
            note_id: Some(anchor.id),
            query: None,
            strategy: effective_strategy,
            fallback_strategy,
            related,
        })
    }

    /// Determine related notes for a free-form query when no anchor note is supplied.
    pub async fn compute_related_notes_for_query(
        &self,
        query: &str,
        limit: Option<usize>,
        strategy: RelatedNotesStrategy,
    ) -> Result<RelatedNotesPayload> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            bail!("query must not be empty");
        }

        let limit = limit.unwrap_or(5).max(1);
        let mut effective_strategy = match strategy {
            RelatedNotesStrategy::Auto => {
                if self.semantic_search_enabled() {
                    RelatedNotesStrategy::Semantic
                } else {
                    RelatedNotesStrategy::Graph
                }
            }
            other => other,
        };

        let mut fallback_strategy = None;
        if !self.semantic_search_enabled()
            && matches!(
                effective_strategy,
                RelatedNotesStrategy::Semantic | RelatedNotesStrategy::Hybrid
            )
        {
            fallback_strategy = Some(effective_strategy);
            effective_strategy = RelatedNotesStrategy::Graph;
        }

        let mut related = match effective_strategy {
            RelatedNotesStrategy::Graph => {
                let results = self.search.search_fts(trimmed, Some(limit * 2)).await?;
                build_related_from_search_results(results, "", limit)
            }
            RelatedNotesStrategy::Semantic => {
                let results = self
                    .search
                    .search_semantic(trimmed, Some(limit * 2))
                    .await?;
                build_related_from_search_results(results, "", limit)
            }
            RelatedNotesStrategy::Hybrid => {
                let results = self.search.search_hybrid(trimmed, Some(limit * 2)).await?;
                build_related_from_search_results(results, "", limit)
            }
            RelatedNotesStrategy::Auto => {
                unreachable!("auto strategy should resolve before dispatching")
            }
        };

        if related.is_empty() && !matches!(effective_strategy, RelatedNotesStrategy::Graph) {
            let results = self.search.search_fts(trimmed, Some(limit * 2)).await?;
            if !results.is_empty() {
                fallback_strategy = Some(effective_strategy);
                effective_strategy = RelatedNotesStrategy::Graph;
                related = build_related_from_search_results(results, "", limit);
            }
        }

        Ok(RelatedNotesPayload {
            note_id: None,
            query: Some(trimmed.to_string()),
            strategy: effective_strategy,
            fallback_strategy,
            related,
        })
    }

    async fn related_notes_graph(
        &self,
        anchor_id: &str,
        limit: usize,
    ) -> Result<Vec<RelatedNotePayload>> {
        let context = self.graph.context(anchor_id).await?;
        let mut accumulator: BTreeMap<String, GraphRelatedAccumulator> = BTreeMap::new();

        for edge in context.backlinks {
            if edge.source == anchor_id {
                continue;
            }
            accumulator
                .entry(edge.source.clone())
                .or_default()
                .reasons
                .insert(format!("Backlink from {}", edge.source));
        }

        for edge in context.forward_links {
            if let Some(target) = &edge.target {
                if target == anchor_id {
                    continue;
                }
                accumulator
                    .entry(target.clone())
                    .or_default()
                    .reasons
                    .insert(format!("Outbound link via [[{}]]", edge.raw));
            }
        }

        let mut items: Vec<(String, GraphRelatedAccumulator)> = accumulator.into_iter().collect();
        items.sort_by(|a, b| {
            let a_score = a.1.reasons.len();
            let b_score = b.1.reasons.len();
            b_score.cmp(&a_score).then_with(|| a.0.cmp(&b.0))
        });

        let items = items.into_iter().take(limit).collect::<Vec<_>>();
        let note_ids: Vec<String> = items.iter().map(|(id, _)| id.clone()).collect();
        let titles = self.note_titles(&note_ids).await?;
        let metadata_map = self.metadata_for_notes(&note_ids).await?;

        Ok(items
            .into_iter()
            .map(|(note_id, entry)| {
                let mut reasons: Vec<String> = entry.reasons.into_iter().collect();
                reasons.sort();
                let reason = if reasons.is_empty() {
                    None
                } else if reasons.len() == 1 {
                    reasons.into_iter().next()
                } else {
                    Some(reasons.join("; "))
                };

                RelatedNotePayload {
                    note_id: note_id.clone(),
                    title: titles.get(&note_id).cloned().unwrap_or(None),
                    score: None,
                    reason,
                    metadata: metadata_map.get(&note_id).cloned(),
                }
            })
            .collect())
    }

    async fn related_notes_semantic(
        &self,
        anchor: &NoteRecord,
        limit: usize,
    ) -> Result<Vec<RelatedNotePayload>> {
        let query = build_similarity_query(anchor);
        let results = self.search.search_semantic(&query, Some(limit * 2)).await?;
        Ok(build_related_from_search_results(
            results, &anchor.id, limit,
        ))
    }

    async fn related_notes_hybrid(
        &self,
        anchor: &NoteRecord,
        limit: usize,
    ) -> Result<Vec<RelatedNotePayload>> {
        let query = build_similarity_query(anchor);
        let results = self.search.search_hybrid(&query, Some(limit * 2)).await?;
        Ok(build_related_from_search_results(
            results, &anchor.id, limit,
        ))
    }
}

#[derive(Default)]
struct GraphRelatedAccumulator {
    reasons: HashSet<String>,
}

const MAX_COMMON_VALUES: usize = 10;

static ISO_DATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}$").expect("valid iso date regex"));
static ISO_WEEK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{4}-W\d{2}$").expect("valid iso week regex"));
static KEBAB_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9]+(?:-[a-z0-9]+)+$").expect("valid kebab-case regex"));
static SNAKE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9]+(?:_[a-z0-9]+)+$").expect("valid snake_case regex"));
static PASCAL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Z][a-z0-9]+(?:[A-Z][a-z0-9]+)+$").expect("valid PascalCase regex")
});
static UPPER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z0-9][A-Z0-9 _-]*$").expect("valid uppercase regex"));

fn count_words(content: &str) -> usize {
    content
        .split_whitespace()
        .filter(|token| !token.is_empty())
        .count()
}

fn detect_naming_patterns(entries: &[NoteInventoryEntry]) -> Vec<NamingPatternSummary> {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for entry in entries {
        let pattern = classify_pattern(&entry.id);
        groups
            .entry(pattern.to_string())
            .or_default()
            .push(entry.id.clone());
    }

    let mut summaries = Vec::new();
    for (pattern, ids) in groups {
        if ids.is_empty() {
            continue;
        }
        let examples = ids.iter().take(3).cloned().collect();
        summaries.push(NamingPatternSummary {
            pattern,
            count: ids.len(),
            examples,
        });
    }

    summaries.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.pattern.cmp(&b.pattern))
    });
    summaries
}

fn classify_pattern(note_id: &str) -> &'static str {
    let leaf = note_id.rsplit('/').next().map(str::trim).unwrap_or(note_id);

    if leaf.is_empty() {
        return "Misc";
    }

    if ISO_DATE_RE.is_match(leaf) {
        "ISO date (YYYY-MM-DD)"
    } else if ISO_WEEK_RE.is_match(leaf) {
        "ISO week (YYYY-Www)"
    } else if leaf.chars().all(|c| c.is_ascii_digit()) {
        "Numeric identifiers"
    } else if KEBAB_RE.is_match(leaf) {
        "kebab-case"
    } else if SNAKE_RE.is_match(leaf) {
        "snake_case"
    } else if PASCAL_RE.is_match(leaf) {
        "PascalCase"
    } else if is_title_case(leaf) {
        "Title Case"
    } else if UPPER_RE.is_match(leaf) {
        "Uppercase identifiers"
    } else {
        "Mixed"
    }
}

fn is_title_case(value: &str) -> bool {
    let mut words = value.split_whitespace().filter(|word| !word.is_empty());
    let mut seen = false;
    for word in words.by_ref() {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            if !first.is_uppercase() {
                return false;
            }
            if !chars.all(|c| c.is_lowercase() || c == '-' || c == '\'') {
                return false;
            }
            seen = true;
        } else {
            return false;
        }
    }
    seen
}

fn aggregate_metadata_fields(
    metadata_map: &HashMap<String, MetadataMap>,
) -> Vec<MetadataFieldStats> {
    #[derive(Clone)]
    struct ValueAggregate {
        value: Value,
        count: usize,
    }

    #[derive(Default)]
    struct FieldAggregate {
        note_count: usize,
        value_kinds: Vec<MetadataValueKind>,
        value_counts: HashMap<String, ValueAggregate>,
    }

    let mut aggregates: BTreeMap<String, FieldAggregate> = BTreeMap::new();

    for metadata in metadata_map.values() {
        for (field, value) in metadata {
            let entry = aggregates.entry(field.clone()).or_default();
            entry.note_count += 1;

            let kind = classify_value_kind(value);
            if !entry.value_kinds.contains(&kind) {
                entry.value_kinds.push(kind);
            }

            if matches!(
                kind,
                MetadataValueKind::String | MetadataValueKind::Number | MetadataValueKind::Boolean
            ) {
                if let Ok(serialised) = serde_json::to_string(value) {
                    let aggregate =
                        entry
                            .value_counts
                            .entry(serialised)
                            .or_insert_with(|| ValueAggregate {
                                value: value.clone(),
                                count: 0,
                            });
                    aggregate.count += 1;
                }
            }
        }
    }

    let mut summaries = Vec::new();
    for (field, mut aggregate) in aggregates {
        sort_value_kinds(&mut aggregate.value_kinds);

        let mut common_values = aggregate
            .value_counts
            .into_values()
            .map(|entry| MetadataCommonValue {
                value: entry.value,
                count: entry.count,
            })
            .collect::<Vec<_>>();

        common_values.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.value.to_string().cmp(&b.value.to_string()))
        });
        common_values.truncate(MAX_COMMON_VALUES);

        summaries.push(MetadataFieldStats {
            field,
            note_count: aggregate.note_count,
            value_kinds: aggregate.value_kinds,
            common_values,
        });
    }

    summaries.sort_by(|a, b| {
        b.note_count
            .cmp(&a.note_count)
            .then_with(|| a.field.cmp(&b.field))
    });
    summaries
}

fn load_agents_playbook() -> AgentsPlaybookPayload {
    AgentsPlaybookPayload {
        relative_path: PathBuf::from("AGENTS.md"),
        content: AGENTS_PLAYBOOK_CONTENT.to_string(),
    }
}

fn classify_value_kind(value: &Value) -> MetadataValueKind {
    match value {
        Value::Null => MetadataValueKind::Null,
        Value::Bool(_) => MetadataValueKind::Boolean,
        Value::Number(_) => MetadataValueKind::Number,
        Value::String(_) => MetadataValueKind::String,
        Value::Array(_) => MetadataValueKind::Array,
        Value::Object(_) => MetadataValueKind::Object,
    }
}

fn sort_value_kinds(kinds: &mut [MetadataValueKind]) {
    fn order(kind: &MetadataValueKind) -> u8 {
        match kind {
            MetadataValueKind::String => 0,
            MetadataValueKind::Number => 1,
            MetadataValueKind::Boolean => 2,
            MetadataValueKind::Array => 3,
            MetadataValueKind::Object => 4,
            MetadataValueKind::Null => 5,
        }
    }

    kinds.sort_by_key(order);
}

fn build_workspace_settings(vault: &Vault) -> Option<WorkspaceSettingsPayload> {
    let attachments = vault.settings().attachments_folder().map(PathBuf::from);
    let ignored = vault.settings().ignored_folders().to_vec();
    let daily_note_format = vault.settings().daily_note_format().map(str::to_string);
    let link_style = vault.settings().link_style().map(str::to_string);

    if attachments.is_none()
        && ignored.is_empty()
        && daily_note_format.is_none()
        && link_style.is_none()
    {
        return None;
    }

    let kind = match vault.workspace_kind() {
        WorkspaceKind::Obsidian => "obsidian",
        WorkspaceKind::Generic => "generic",
    }
    .to_string();

    Some(WorkspaceSettingsPayload {
        kind,
        attachments_folder: attachments,
        ignored_folders: ignored,
        daily_note_format,
        link_style,
    })
}

fn build_obsidian_settings_legacy(vault: &Vault) -> Option<ObsidianSettingsPayload> {
    if !matches!(vault.workspace_kind(), WorkspaceKind::Obsidian) {
        return None;
    }

    let attachments = vault.settings().attachments_folder().map(PathBuf::from);
    let ignored = vault.settings().ignored_folders().to_vec();
    let daily_note_format = vault.settings().daily_note_format().map(str::to_string);
    let link_style = vault.settings().link_style().map(str::to_string);

    if attachments.is_none()
        && ignored.is_empty()
        && daily_note_format.is_none()
        && link_style.is_none()
    {
        return None;
    }

    Some(ObsidianSettingsPayload {
        attachments_folder: attachments,
        ignored_folders: ignored,
        daily_note_format,
        link_style,
    })
}

fn build_similarity_query(note: &NoteRecord) -> String {
    let mut segments = Vec::new();
    if let Some(title) = &note.title {
        segments.push(title.clone());
    }

    if let Some(tags) = note.metadata.get("tags").and_then(Value::as_array) {
        let joined = tags
            .iter()
            .filter_map(Value::as_str)
            .map(|tag| format!("#{tag}"))
            .collect::<Vec<_>>()
            .join(" ");
        if !joined.is_empty() {
            segments.push(joined);
        }
    }

    if segments.len() < 2 {
        let snippet = note
            .content
            .split_whitespace()
            .take(40)
            .collect::<Vec<_>>()
            .join(" ");
        if !snippet.is_empty() {
            segments.push(snippet);
        }
    }

    let mut query = segments.join(" ");
    if query.len() > 512 {
        query.truncate(512);
    }
    query
}

fn build_related_from_search_results(
    results: Vec<SearchResult>,
    anchor_id: &str,
    limit: usize,
) -> Vec<RelatedNotePayload> {
    let mut related = Vec::new();
    let mut seen = HashSet::new();

    for result in results {
        if result.note_id == anchor_id || !seen.insert(result.note_id.clone()) {
            continue;
        }

        related.push(RelatedNotePayload {
            note_id: result.note_id.clone(),
            title: result.title.clone(),
            score: Some(result.score),
            reason: result.reason.clone(),
            metadata: Some(result.metadata.clone()),
        });

        if related.len() >= limit {
            break;
        }
    }

    related
}

/// Helper that communicates with the Arrowhead daemon.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket_path: PathBuf,
    status_path: PathBuf,
}

impl DaemonClient {
    /// Construct a client pointing at the supplied socket and status paths.
    pub fn new(socket_path: PathBuf, status_path: PathBuf) -> Self {
        Self {
            socket_path,
            status_path,
        }
    }

    /// Path to the control socket.
    #[must_use]
    pub fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }

    /// Fetch the latest daemon status, falling back to the cached status file.
    pub async fn status(&self) -> Result<DaemonStatus> {
        match send_control_request(&self.socket_path, ControlRequest::StatusSnapshot).await {
            Ok(ControlResponse::Status { status }) => Ok(status),
            Ok(ControlResponse::Error { message }) => {
                bail!("arrowhead daemon reported an error: {message}");
            }
            Ok(ControlResponse::ShutdownAck) => {
                bail!(
                    "arrowhead daemon acknowledged shutdown; restart it with `arrowhead index start`."
                );
            }
            Err(err) => {
                let status_path = self.status_path.clone();
                let fallback =
                    task::spawn_blocking(move || DaemonStatus::load_from_path(&status_path))
                        .await
                        .context("status fallback task aborted")??;

                if let Some(mut status) = fallback {
                    let code = "daemon_offline_cached_status";
                    let already_recorded = status.issues.iter().any(|issue| issue.code == code);
                    if !already_recorded {
                        let mut issue = StatusIssue::new(
                            code,
                            "Using cached daemon status because the live daemon could not be reached.",
                            IssueSeverity::Warning,
                        );
                        issue.detail = Some(err.to_string());
                        status.issues.push(issue);
                    }
                    Ok(status)
                } else {
                    Err(anyhow!(
                        "arrowhead daemon is not running (socket {} unreachable: {}). Start it with `arrowhead index start` and retry.",
                        self.socket_path.display(),
                        err
                    ))
                }
            }
        }
    }
}
