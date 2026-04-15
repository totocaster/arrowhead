//! Context aggregation across notes, metrics, and sources.

use std::{
    collections::{BTreeSet, HashSet},
    path::PathBuf,
    sync::{Arc, LazyLock},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task;

use crate::{
    GraphService, LinkEdge, LinkReason, MetricFileSummary, MetricRecordEntry, MetricsService,
    NoteRecord, SearchResult, SearchService, Vault, sqlite::IndexDatabase,
};

/// Default number of related notes returned by context queries.
pub const DEFAULT_CONTEXT_NOTE_LIMIT: usize = 5;
/// Default number of metric records returned by context queries.
pub const DEFAULT_CONTEXT_METRIC_LIMIT: usize = 10;

static METRIC_REFERENCE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"metric:([A-Za-z0-9][A-Za-z0-9._:-]*)").expect("valid metric reference regex")
});
static DATE_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d{4}-\d{2}-\d{2})(?:$|[-_])").expect("valid date prefix regex")
});

/// Context target classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextTargetKind {
    /// Note-centric context.
    Note,
    /// Metric key or metric record context.
    Metric,
    /// Metric source context.
    Source,
}

/// High-level summary section for every context response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextSummary {
    /// Classification of the target being described.
    pub kind: ContextTargetKind,
    /// Stable target identifier.
    pub target: String,
    /// Optional human-friendly label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Number of notes surfaced in this payload.
    pub note_count: usize,
    /// Number of metric records surfaced in this payload.
    pub metric_count: usize,
    /// Number of relationship links surfaced in this payload.
    pub link_count: usize,
    /// Number of attention items surfaced in this payload.
    pub attention_count: usize,
}

/// Lightweight note summary included in context responses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextNoteItem {
    /// Note identifier.
    pub note_id: String,
    /// Optional title resolved from frontmatter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional vault-relative path for the note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relative_path: Option<PathBuf>,
    /// Optional file modification timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_modified_at: Option<DateTime<Utc>>,
    /// Optional short preview or excerpt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Optional explanation for why the note is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Relationship bucket classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextLinkKind {
    /// Explicit reference extracted directly from source content.
    Explicit,
    /// Structural relationship inferred from graph or date alignment.
    Structural,
    /// Looser relatedness surfaced from search matches.
    Related,
}

/// Generic relationship entry surfaced by context responses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextLink {
    /// Classification for the relationship.
    pub kind: ContextLinkKind,
    /// Stable `from` entity label.
    pub from: String,
    /// Stable `to` entity label.
    pub to: String,
    /// Human-readable explanation for the relationship.
    pub reason: String,
    /// Optional confidence for inferred links.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

/// Item that deserves user or agent attention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextAttentionItem {
    /// Stable attention item kind.
    pub kind: String,
    /// Human-readable message.
    pub message: String,
    /// Optional related note identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note_id: Option<String>,
    /// Optional related metric identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric_id: Option<String>,
    /// Optional related source file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_file: Option<PathBuf>,
    /// Optional related 1-based source line number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_line: Option<usize>,
}

/// Historical section of a context payload.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextHistory {
    /// Notes related to the target over time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<ContextNoteItem>,
    /// Metrics related to the target over time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metrics: Vec<MetricRecordEntry>,
}

/// Activity section of a context payload.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextActivity {
    /// Note activity associated with the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<ContextNoteItem>,
    /// Metric activity associated with the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metrics: Vec<MetricRecordEntry>,
    /// Metrics files touched by the surfaced records.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<MetricFileSummary>,
}

/// Links section of a context payload.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextLinks {
    /// Surface-level relationships relevant to the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<ContextLink>,
}

/// Attention section of a context payload.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextAttention {
    /// Validation, ambiguity, and unresolved-link warnings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<ContextAttentionItem>,
}

/// Related section of a context payload.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextRelated {
    /// Notes adjacent to the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<ContextNoteItem>,
    /// Metric keys adjacent to the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metric_keys: Vec<String>,
    /// Sources adjacent to the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
}

/// Stable context payload shared by CLI JSON and MCP responses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextPayload {
    /// High-level summary of the target and payload size.
    pub summary: ContextSummary,
    /// Historical context for the target.
    pub history: ContextHistory,
    /// Immediate activity around the target.
    pub activity: ContextActivity,
    /// Explicit and structural relationships.
    pub links: ContextLinks,
    /// Validation and unresolved-link items requiring attention.
    pub attention: ContextAttention,
    /// Nearby notes, metric keys, and sources.
    pub related: ContextRelated,
}

/// Context aggregation service spanning notes and metrics.
#[derive(Debug, Clone)]
pub struct ContextService {
    vault: Arc<Vault>,
    database: Arc<IndexDatabase>,
    graph: GraphService,
    metrics: MetricsService,
    search: SearchService,
}

impl ContextService {
    /// Construct a context service backed by the supplied vault, database, and search service.
    pub fn new(vault: Arc<Vault>, database: Arc<IndexDatabase>, search: SearchService) -> Self {
        Self {
            vault,
            database: Arc::clone(&database),
            graph: GraphService::new(Arc::clone(&database)),
            metrics: MetricsService::new(database),
            search,
        }
    }

    /// Build context around a note identifier.
    pub async fn note(
        &self,
        note_id: &str,
        note_limit: Option<usize>,
        metric_limit: Option<usize>,
    ) -> Result<ContextPayload> {
        self.ensure_note_indexed(note_id).await?;
        let note_limit = note_limit.unwrap_or(DEFAULT_CONTEXT_NOTE_LIMIT).max(1);
        let metric_limit = metric_limit.unwrap_or(DEFAULT_CONTEXT_METRIC_LIMIT).max(1);
        let note = self.load_note(note_id).await?;
        let anchor_note = note_item_from_note_record(&note, Some("Anchor note".to_string()));

        let graph_context = self.graph.context(note_id).await?;
        let unresolved_links = graph_context
            .forward_links
            .iter()
            .filter(|edge| edge.target.is_none())
            .cloned()
            .collect::<Vec<_>>();

        let mut related_note_ids = Vec::new();
        for edge in &graph_context.backlinks {
            if edge.source != note.id {
                related_note_ids.push(edge.source.clone());
            }
        }
        for edge in &graph_context.forward_links {
            if let Some(target) = edge.target.as_ref() {
                if target != &note.id {
                    related_note_ids.push(target.clone());
                }
            }
        }

        let mut related_notes = self
            .note_items_for_ids(&related_note_ids, Some("Graph link"))
            .await?;
        trim_note_items(&mut related_notes, note_limit);

        let mut seen_note_ids = related_notes
            .iter()
            .map(|item| item.note_id.clone())
            .collect::<HashSet<_>>();
        seen_note_ids.insert(note.id.clone());
        if related_notes.len() < note_limit {
            for term in related_note_search_terms(&note) {
                let search_results = self
                    .search_notes_by_phrase(&term, note_limit * 2, &seen_note_ids, "Textual match")
                    .await?;
                merge_note_items(
                    &mut related_notes,
                    search_results,
                    note_limit,
                    &mut seen_note_ids,
                );
                if related_notes.len() >= note_limit {
                    break;
                }
            }
        }

        let explicit_metric_ids = extract_metric_references(&note);
        let note_dates = extract_note_dates(&note);
        let (mut metrics, mut links) = self
            .note_metric_links(&note.id, &explicit_metric_ids, &note_dates, metric_limit)
            .await?;

        let mut attention = attention_items_for_metrics(&metrics);
        attention.extend(attention_items_for_unresolved_links(&unresolved_links));
        let files = self.metric_files_for_records(&metrics).await?;
        let graph_links = graph_context
            .backlinks
            .iter()
            .chain(graph_context.forward_links.iter())
            .filter(|edge| edge.target.is_some())
            .map(context_link_from_graph_edge)
            .collect::<Vec<_>>();
        links.extend(graph_links);

        sort_metric_records(&mut metrics);
        trim_metric_records(&mut metrics, metric_limit);

        Ok(ContextPayload {
            summary: ContextSummary {
                kind: ContextTargetKind::Note,
                target: note.id.clone(),
                label: note.title.clone(),
                note_count: 1 + related_notes.len(),
                metric_count: metrics.len(),
                link_count: links.len(),
                attention_count: attention.len(),
            },
            history: ContextHistory {
                notes: related_notes.clone(),
                metrics: metrics.clone(),
            },
            activity: ContextActivity {
                notes: vec![anchor_note],
                metrics: metrics.clone(),
                files,
            },
            links: ContextLinks { items: links },
            attention: ContextAttention { items: attention },
            related: ContextRelated {
                notes: related_notes,
                metric_keys: unique_metric_keys(&metrics),
                sources: unique_metric_sources(&metrics),
            },
        })
    }

    /// Build context around a metric id or key.
    pub async fn metric(
        &self,
        metric_ref_or_key: &str,
        range: Option<&str>,
        note_limit: Option<usize>,
        metric_limit: Option<usize>,
    ) -> Result<ContextPayload> {
        let note_limit = note_limit.unwrap_or(DEFAULT_CONTEXT_NOTE_LIMIT).max(1);
        let metric_limit = metric_limit.unwrap_or(DEFAULT_CONTEXT_METRIC_LIMIT).max(1);
        let target = metric_ref_or_key.trim();
        if target.is_empty() {
            bail!("metric target must not be empty");
        }

        let exact_record = self.metrics.read_record(target).await?;
        let (mut metrics, target_value, label, note_terms, record_target) =
            if let Some(record) = exact_record {
                let mut records = self
                    .metrics
                    .search(
                        &build_metrics_field_query("key", &record.record.key, range),
                        Some(metric_limit),
                    )
                    .await?;
                prepend_metric_record(&mut records, record.clone());
                (
                    records,
                    format!("metric:{}", record.record.id),
                    Some(format!(
                        "{} from {}",
                        record.record.key, record.record.source
                    )),
                    vec![
                        format!("metric:{}", record.record.id),
                        record.record.key.clone(),
                    ],
                    Some(record.record.id.clone()),
                )
            } else {
                if target.starts_with("metric:") {
                    bail!("metric {target} was not found in the index");
                }
                let records = self
                    .metrics
                    .search(
                        &build_metrics_field_query("key", target, range),
                        Some(metric_limit),
                    )
                    .await?;
                if records.is_empty() {
                    bail!("metric key `{target}` was not found in the index");
                }
                let record_count = records.len();
                (
                    records,
                    target.to_string(),
                    Some(format!(
                        "{} ({} record{})",
                        target,
                        record_count,
                        if record_count == 1 { "" } else { "s" }
                    )),
                    vec![target.to_string()],
                    None,
                )
            };

        sort_metric_records(&mut metrics);
        trim_metric_records(&mut metrics, metric_limit);

        let mut related_notes = Vec::new();
        let mut seen_note_ids = HashSet::new();
        for term in &note_terms {
            let reason = if term.starts_with("metric:") {
                "Explicit metric reference"
            } else {
                "Note text matches metric key"
            };
            let search_results = self
                .search_notes_by_phrase(term, note_limit * 2, &seen_note_ids, reason)
                .await?;
            merge_note_items(
                &mut related_notes,
                search_results,
                note_limit,
                &mut seen_note_ids,
            );
            if related_notes.len() >= note_limit {
                break;
            }
        }
        let date_notes = self
            .note_items_for_metric_dates(&metrics, "Same day as metric activity")
            .await?;
        merge_note_items(
            &mut related_notes,
            date_notes,
            note_limit,
            &mut seen_note_ids,
        );

        let mut links = Vec::new();
        for record in &metrics {
            links.push(ContextLink {
                kind: ContextLinkKind::Structural,
                from: format!("metric:{}", record.record.id),
                to: format!("source:{}", record.record.source),
                reason: "Metric record source".to_string(),
                confidence: None,
            });
        }
        for note in &related_notes {
            let kind = if record_target.is_some() {
                ContextLinkKind::Explicit
            } else {
                ContextLinkKind::Related
            };
            links.push(ContextLink {
                kind,
                from: format!("note:{}", note.note_id),
                to: target_value.clone(),
                reason: note
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Related note".to_string()),
                confidence: None,
            });
        }

        let attention = attention_items_for_metrics(&metrics);
        let files = self.metric_files_for_records(&metrics).await?;

        Ok(ContextPayload {
            summary: ContextSummary {
                kind: ContextTargetKind::Metric,
                target: target_value.clone(),
                label,
                note_count: related_notes.len(),
                metric_count: metrics.len(),
                link_count: links.len(),
                attention_count: attention.len(),
            },
            history: ContextHistory {
                notes: related_notes.clone(),
                metrics: metrics.clone(),
            },
            activity: ContextActivity {
                notes: related_notes.clone(),
                metrics: metrics.clone(),
                files,
            },
            links: ContextLinks { items: links },
            attention: ContextAttention { items: attention },
            related: ContextRelated {
                notes: related_notes,
                metric_keys: unique_metric_keys(&metrics),
                sources: unique_metric_sources(&metrics),
            },
        })
    }

    /// Build context around a source name.
    pub async fn source(
        &self,
        source: &str,
        range: Option<&str>,
        note_limit: Option<usize>,
        metric_limit: Option<usize>,
    ) -> Result<ContextPayload> {
        let source = source.trim();
        if source.is_empty() {
            bail!("source must not be empty");
        }

        let note_limit = note_limit.unwrap_or(DEFAULT_CONTEXT_NOTE_LIMIT).max(1);
        let metric_limit = metric_limit.unwrap_or(DEFAULT_CONTEXT_METRIC_LIMIT).max(1);
        let mut metrics = self
            .metrics
            .search(
                &build_metrics_field_query("source", source, range),
                Some(metric_limit),
            )
            .await?;
        if metrics.is_empty() {
            bail!("source `{source}` was not found in indexed metrics");
        }
        sort_metric_records(&mut metrics);
        trim_metric_records(&mut metrics, metric_limit);

        let mut related_notes = self
            .note_items_for_metric_dates(&metrics, "Same day as metric activity")
            .await?;
        let mut seen_note_ids = related_notes
            .iter()
            .map(|item| item.note_id.clone())
            .collect::<HashSet<_>>();
        trim_note_items(&mut related_notes, note_limit);
        if related_notes.len() < note_limit {
            let search_notes = self
                .search_notes_by_literal_phrase(
                    source,
                    note_limit * 3,
                    &seen_note_ids,
                    "Note text matches source",
                )
                .await?;
            merge_note_items(
                &mut related_notes,
                search_notes,
                note_limit,
                &mut seen_note_ids,
            );
        }

        let mut links = metrics
            .iter()
            .map(|record| ContextLink {
                kind: ContextLinkKind::Structural,
                from: format!("source:{source}"),
                to: format!("metric:{}", record.record.id),
                reason: "Metric record belongs to the requested source".to_string(),
                confidence: None,
            })
            .collect::<Vec<_>>();
        for note in &related_notes {
            links.push(ContextLink {
                kind: ContextLinkKind::Related,
                from: format!("note:{}", note.note_id),
                to: format!("source:{source}"),
                reason: note
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Related note".to_string()),
                confidence: None,
            });
        }

        let attention = attention_items_for_metrics(&metrics);
        let files = self.metric_files_for_records(&metrics).await?;

        Ok(ContextPayload {
            summary: ContextSummary {
                kind: ContextTargetKind::Source,
                target: source.to_string(),
                label: Some(format!(
                    "{} ({} record{})",
                    source,
                    metrics.len(),
                    if metrics.len() == 1 { "" } else { "s" }
                )),
                note_count: related_notes.len(),
                metric_count: metrics.len(),
                link_count: links.len(),
                attention_count: attention.len(),
            },
            history: ContextHistory {
                notes: related_notes.clone(),
                metrics: metrics.clone(),
            },
            activity: ContextActivity {
                notes: related_notes.clone(),
                metrics: metrics.clone(),
                files,
            },
            links: ContextLinks { items: links },
            attention: ContextAttention { items: attention },
            related: ContextRelated {
                notes: related_notes,
                metric_keys: unique_metric_keys(&metrics),
                sources: vec![source.to_string()],
            },
        })
    }

    async fn ensure_note_indexed(&self, note_id: &str) -> Result<()> {
        let database = Arc::clone(&self.database);
        let note_id = note_id.to_string();
        let lookup_id = note_id.clone();
        let indexed = task::spawn_blocking(move || database.note_state(&lookup_id))
            .await
            .context("note state task aborted")??;
        if indexed.is_none() {
            bail!(
                "note {note_id} is not indexed. Run `arrowhead index start` to refresh the index."
            );
        }
        Ok(())
    }

    async fn load_note(&self, note_id: &str) -> Result<NoteRecord> {
        let vault = Arc::clone(&self.vault);
        let note_id = note_id.to_string();
        task::spawn_blocking(move || vault.load_note(&note_id))
            .await
            .context("note load task aborted")?
    }

    async fn note_items_for_ids(
        &self,
        note_ids: &[String],
        reason: Option<&str>,
    ) -> Result<Vec<ContextNoteItem>> {
        if note_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut ordered_ids = Vec::new();
        let mut seen = HashSet::new();
        for note_id in note_ids {
            if seen.insert(note_id.clone()) {
                ordered_ids.push(note_id.clone());
            }
        }

        let vault = Arc::clone(&self.vault);
        let database = Arc::clone(&self.database);
        let reason = reason.map(str::to_string);
        task::spawn_blocking(move || -> Result<Vec<ContextNoteItem>> {
            let snapshot = vault.inventory_snapshot()?;
            let titles = database.titles_for_notes(&ordered_ids)?;
            let relative_paths = database.relative_paths_for_notes(&ordered_ids)?;
            let mut items = Vec::new();
            for note_id in ordered_ids {
                let entry = snapshot.get_by_id(&note_id);
                let preview = database.note_excerpt(&note_id, 240)?;
                items.push(ContextNoteItem {
                    note_id: note_id.clone(),
                    title: titles.get(&note_id).cloned().unwrap_or(None),
                    relative_path: entry
                        .map(|item| item.relative_path.clone())
                        .or_else(|| relative_paths.get(&note_id).map(PathBuf::from)),
                    file_modified_at: entry.map(|item| item.file_modified_at),
                    preview,
                    reason: reason.clone(),
                });
            }
            Ok(items)
        })
        .await
        .context("note summary task aborted")?
    }

    async fn search_notes_by_phrase(
        &self,
        phrase: &str,
        limit: usize,
        exclude_ids: &HashSet<String>,
        reason: &str,
    ) -> Result<Vec<ContextNoteItem>> {
        let query = exact_phrase_query(phrase);
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let results = self.search.search_fts(&query, Some(limit.max(1))).await?;
        Ok(results
            .into_iter()
            .filter(|result| !exclude_ids.contains(&result.note_id))
            .map(|result| note_item_from_search_result(result, Some(reason.to_string())))
            .collect())
    }

    async fn search_notes_by_literal_phrase(
        &self,
        phrase: &str,
        limit: usize,
        exclude_ids: &HashSet<String>,
        reason: &str,
    ) -> Result<Vec<ContextNoteItem>> {
        let candidates = self
            .search_notes_by_phrase(phrase, limit.max(1) * 2, exclude_ids, reason)
            .await?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let vault = Arc::clone(&self.vault);
        let note_ids = candidates
            .iter()
            .map(|item| item.note_id.clone())
            .collect::<Vec<_>>();
        let phrase = phrase.to_string();
        let matching_ids = task::spawn_blocking(move || -> HashSet<String> {
            let mut matches = HashSet::new();
            for note_id in note_ids {
                let Ok(note) = vault.load_note(&note_id) else {
                    continue;
                };
                if note_contains_literal_phrase(&note, &phrase) {
                    matches.insert(note_id);
                }
            }
            matches
        })
        .await
        .context("literal note match task aborted")?;

        Ok(candidates
            .into_iter()
            .filter(|item| matching_ids.contains(&item.note_id))
            .take(limit.max(1))
            .collect())
    }

    async fn note_items_for_metric_dates(
        &self,
        records: &[MetricRecordEntry],
        reason: &str,
    ) -> Result<Vec<ContextNoteItem>> {
        let dates = records
            .iter()
            .filter_map(|record| record.record.date)
            .collect::<BTreeSet<_>>();
        if dates.is_empty() {
            return Ok(Vec::new());
        }

        let vault = Arc::clone(&self.vault);
        let database = Arc::clone(&self.database);
        let reason = reason.to_string();
        task::spawn_blocking(move || -> Result<Vec<ContextNoteItem>> {
            let snapshot = vault.inventory_snapshot()?;
            let mut note_ids = Vec::new();
            let date_strings = dates
                .iter()
                .map(ToString::to_string)
                .collect::<HashSet<_>>();
            for entry in snapshot.entries() {
                if date_strings.contains(&entry.id) {
                    note_ids.push(entry.id.clone());
                }
            }

            let titles = database.titles_for_notes(&note_ids)?;
            let mut items = Vec::new();
            for note_id in note_ids {
                let entry = snapshot.get_by_id(&note_id);
                items.push(ContextNoteItem {
                    note_id: note_id.clone(),
                    title: titles.get(&note_id).cloned().unwrap_or(None),
                    relative_path: entry.map(|item| item.relative_path.clone()),
                    file_modified_at: entry.map(|item| item.file_modified_at),
                    preview: database.note_excerpt(&note_id, 240)?,
                    reason: Some(reason.clone()),
                });
            }
            Ok(items)
        })
        .await
        .context("note-by-date task aborted")?
    }

    async fn note_metric_links(
        &self,
        note_id: &str,
        explicit_metric_ids: &[String],
        note_dates: &BTreeSet<NaiveDate>,
        metric_limit: usize,
    ) -> Result<(Vec<MetricRecordEntry>, Vec<ContextLink>)> {
        let mut metrics = Vec::new();
        let mut links = Vec::new();
        let mut seen = HashSet::new();

        for metric_id in explicit_metric_ids {
            if let Some(record) = self.metrics.read_record(metric_id).await? {
                if seen.insert(record.record.id.clone()) {
                    links.push(ContextLink {
                        kind: ContextLinkKind::Explicit,
                        from: format!("note:{note_id}"),
                        to: format!("metric:{}", record.record.id),
                        reason: "Note contains explicit metric reference".to_string(),
                        confidence: None,
                    });
                    metrics.push(record);
                }
            }
        }

        for date in note_dates {
            let results = self
                .metrics
                .search(&format!("date:{date}"), Some(metric_limit * 2))
                .await?;
            for record in results {
                if seen.insert(record.record.id.clone()) {
                    links.push(ContextLink {
                        kind: ContextLinkKind::Structural,
                        from: format!("note:{note_id}"),
                        to: format!("metric:{}", record.record.id),
                        reason: format!("Note date matches metric date {date}"),
                        confidence: None,
                    });
                    metrics.push(record);
                }
            }
        }

        Ok((metrics, links))
    }

    async fn metric_files_for_records(
        &self,
        records: &[MetricRecordEntry],
    ) -> Result<Vec<MetricFileSummary>> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let paths = records
            .iter()
            .map(|record| record.source_file.clone())
            .collect::<HashSet<_>>();
        let mut files = self.metrics.list_files().await?;
        files.retain(|file| paths.contains(&file.relative_path));
        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Ok(files)
    }
}

fn note_item_from_note_record(note: &NoteRecord, reason: Option<String>) -> ContextNoteItem {
    ContextNoteItem {
        note_id: note.id.clone(),
        title: note.title.clone(),
        relative_path: Some(note.relative_path.clone()),
        file_modified_at: Some(note.file_modified_at),
        preview: preview_from_text(&note.content, 240),
        reason,
    }
}

fn note_item_from_search_result(result: SearchResult, reason: Option<String>) -> ContextNoteItem {
    ContextNoteItem {
        note_id: result.note_id,
        title: result.title,
        relative_path: result.relative_path.map(PathBuf::from),
        file_modified_at: None,
        preview: result.preview,
        reason: reason.or(result.reason),
    }
}

fn preview_from_text(text: &str, limit: usize) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= limit {
        return Some(trimmed.to_string());
    }
    let mut preview = trimmed.chars().take(limit).collect::<String>();
    preview.push_str("...");
    Some(preview)
}

fn trim_note_items(items: &mut Vec<ContextNoteItem>, limit: usize) {
    if items.len() > limit {
        items.truncate(limit);
    }
}

fn merge_note_items(
    target: &mut Vec<ContextNoteItem>,
    incoming: Vec<ContextNoteItem>,
    limit: usize,
    seen: &mut HashSet<String>,
) {
    for item in incoming {
        if target.len() >= limit {
            break;
        }
        if seen.insert(item.note_id.clone()) {
            target.push(item);
        }
    }
}

fn trim_metric_records(records: &mut Vec<MetricRecordEntry>, limit: usize) {
    if records.len() > limit {
        records.truncate(limit);
    }
}

fn sort_metric_records(records: &mut [MetricRecordEntry]) {
    records.sort_by(|left, right| {
        right
            .record
            .ts
            .cmp(&left.record.ts)
            .then_with(|| left.source_file.cmp(&right.source_file))
            .then_with(|| left.source_line.cmp(&right.source_line))
    });
}

fn prepend_metric_record(records: &mut Vec<MetricRecordEntry>, record: MetricRecordEntry) {
    let mut deduped = Vec::with_capacity(records.len() + 1);
    deduped.push(record);
    for item in records.drain(..) {
        if deduped
            .iter()
            .any(|existing| existing.record.id == item.record.id)
        {
            continue;
        }
        deduped.push(item);
    }
    *records = deduped;
}

fn context_link_from_graph_edge(edge: &LinkEdge) -> ContextLink {
    let reason = match edge.reason {
        LinkReason::Direct => "WikiLink direct match",
        LinkReason::Title => "WikiLink title match",
        LinkReason::Alias => "WikiLink alias match",
        LinkReason::Unresolved => "Unresolved WikiLink",
    };
    ContextLink {
        kind: ContextLinkKind::Structural,
        from: format!("note:{}", edge.source),
        to: edge
            .target
            .as_ref()
            .map(|value| format!("note:{value}"))
            .unwrap_or_else(|| edge.raw.clone()),
        reason: reason.to_string(),
        confidence: None,
    }
}

fn attention_items_for_unresolved_links(links: &[LinkEdge]) -> Vec<ContextAttentionItem> {
    links
        .iter()
        .map(|edge| ContextAttentionItem {
            kind: "unresolved_link".to_string(),
            message: format!("Unresolved WikiLink `[[{}]]` in {}", edge.raw, edge.source),
            note_id: Some(edge.source.clone()),
            metric_id: None,
            source_file: None,
            source_line: None,
        })
        .collect()
}

fn attention_items_for_metrics(records: &[MetricRecordEntry]) -> Vec<ContextAttentionItem> {
    records
        .iter()
        .flat_map(|record| {
            record.issues.iter().map(|issue| ContextAttentionItem {
                kind: "metric_issue".to_string(),
                message: issue.message.clone(),
                note_id: None,
                metric_id: Some(record.record.id.clone()),
                source_file: Some(record.source_file.clone()),
                source_line: Some(record.source_line),
            })
        })
        .collect()
}

fn unique_metric_keys(records: &[MetricRecordEntry]) -> Vec<String> {
    records
        .iter()
        .map(|record| record.record.key.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn unique_metric_sources(records: &[MetricRecordEntry]) -> Vec<String> {
    records
        .iter()
        .map(|record| record.record.source.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn extract_metric_references(note: &NoteRecord) -> Vec<String> {
    let mut references = BTreeSet::new();
    let metadata_json = serde_json::to_string(&note.metadata).unwrap_or_default();
    let haystack = format!(
        "{}\n{}\n{}\n{}",
        note.id,
        note.title.clone().unwrap_or_default(),
        note.content,
        metadata_json
    );
    for capture in METRIC_REFERENCE_RE.captures_iter(&haystack) {
        if let Some(matched) = capture.get(1) {
            let cleaned = matched
                .as_str()
                .trim_matches(|ch: char| {
                    matches!(ch, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}')
                })
                .trim();
            if !cleaned.is_empty() {
                references.insert(cleaned.to_string());
            }
        }
    }
    references.into_iter().collect()
}

fn extract_note_dates(note: &NoteRecord) -> BTreeSet<NaiveDate> {
    let mut dates = BTreeSet::new();
    if let Some(captures) = DATE_PREFIX_RE.captures(&note.id) {
        if let Some(matched) = captures.get(1) {
            if let Ok(date) = NaiveDate::parse_from_str(matched.as_str(), "%Y-%m-%d") {
                dates.insert(date);
            }
        }
    }

    if let Some(Value::String(value)) = note.metadata.get("date") {
        if let Ok(date) = NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d") {
            dates.insert(date);
        }
    }

    dates
}

fn related_note_search_terms(note: &NoteRecord) -> Vec<String> {
    let mut terms = Vec::new();
    if let Some(title) = note.title.as_ref() {
        let trimmed = title.trim();
        if !trimmed.is_empty() {
            terms.push(trimmed.to_string());
        }
    }
    let trimmed_id = note.id.trim();
    if !trimmed_id.is_empty() && !terms.iter().any(|term| term == trimmed_id) {
        terms.push(trimmed_id.to_string());
    }
    terms
}

fn exact_phrase_query(term: &str) -> String {
    let cleaned = term.trim().replace('"', "");
    if cleaned.is_empty() {
        String::new()
    } else {
        format!("\"{cleaned}\"")
    }
}

fn note_contains_literal_phrase(note: &NoteRecord, phrase: &str) -> bool {
    let needle = normalize_literal_phrase(phrase);
    if needle.is_empty() {
        return false;
    }

    let mut haystack = String::new();
    haystack.push_str(&note.id);
    if let Some(title) = note.title.as_ref() {
        haystack.push(' ');
        haystack.push_str(title);
    }
    haystack.push(' ');
    haystack.push_str(&note.content);

    let normalized_haystack = normalize_literal_phrase(&haystack);
    normalized_haystack
        .split_whitespace()
        .collect::<Vec<_>>()
        .windows(needle.split_whitespace().count())
        .any(|window| window.join(" ") == needle)
}

fn normalize_literal_phrase(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn build_metrics_field_query(field: &str, value: &str, range: Option<&str>) -> String {
    let value = value.trim().replace('"', "");
    let mut query = if value.contains(char::is_whitespace) {
        format!("{field}:\"{value}\"")
    } else {
        format!("{field}:{value}")
    };
    if let Some(range) = range.map(str::trim).filter(|value| !value.is_empty()) {
        query.push(' ');
        query.push_str("date:");
        query.push_str(range);
    }
    query
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, fs, io::Cursor};

    use super::*;
    use crate::{
        LinkResolutionRecord, MetadataMap, MetricsConfigFile, SearchConfig, VaultConfig,
        graph::LinkReason,
        metadata::{MetadataExtraction, MetadataExtractor},
        parse_metrics_reader,
        workspace::{WORKSPACE_CONFIG_FILE, WorkspaceFile, write_workspace_file},
    };
    use tempfile::TempDir;

    fn build_service() -> (TempDir, ContextService) {
        let dir = TempDir::new().expect("temp dir");
        let vault_root = dir.path().join("vault");
        fs::create_dir_all(&vault_root).expect("create vault root");
        fs::create_dir_all(vault_root.join(".arrowhead")).expect("create arrowhead dir");
        write_workspace_file(
            &vault_root.join(".arrowhead").join(WORKSPACE_CONFIG_FILE),
            &WorkspaceFile {
                attachments_dir: None,
                ignored_folders: Vec::new(),
                daily_note_format: None,
                link_style: None,
                metrics: Some(MetricsConfigFile {
                    root: Some("Metrics".to_string()),
                    extensions: vec![".metrics.ndjson".to_string()],
                    default_write_file: Some("Metrics/All.metrics.ndjson".to_string()),
                    record_reference_prefix: Some("metric:".to_string()),
                    week_start_day: None,
                    day_start_hour: None,
                }),
            },
        )
        .expect("write workspace");

        let vault = Arc::new(Vault::new(VaultConfig::new(vault_root)).expect("vault"));
        vault.ensure_arrowhead_dirs().expect("ensure dirs");
        let database = Arc::new(
            IndexDatabase::open(vault.paths().arrowhead_dir.join("index.db")).expect("db"),
        );

        seed_notes(vault.as_ref(), database.as_ref());
        seed_metrics(database.as_ref());

        let search = SearchService::new(Arc::clone(&database), SearchConfig::default(), None);
        let service = ContextService::new(vault, database, search);
        (dir, service)
    }

    fn seed_notes(vault: &Vault, database: &IndexDatabase) {
        let notes = vec![
            build_note(
                vault,
                "Project Hub",
                Some("Project Hub"),
                "Track body.weight in [[2026-04-14]] and metric:01AAA from withings.",
            ),
            build_note(
                vault,
                "2026-04-14",
                Some("2026-04-14"),
                "Daily note for body.weight updates.",
            ),
            build_note(
                vault,
                "Related Note",
                Some("Related Note"),
                "See [[Project Hub]] for the latest withings import.",
            ),
        ];
        let note_ids = notes
            .iter()
            .map(|note| note.id.clone())
            .collect::<HashSet<_>>();

        for note in notes {
            let extraction = MetadataExtractor::new()
                .extract(&note)
                .expect("extract metadata");
            let resolved_links = make_resolved_links(&extraction, &note_ids);
            database
                .upsert_note(&note, &extraction, &resolved_links, Utc::now())
                .expect("upsert note");
        }
    }

    fn build_note(vault: &Vault, note_id: &str, title: Option<&str>, body: &str) -> NoteRecord {
        let mut metadata = MetadataMap::default();
        if let Some(title) = title {
            metadata.insert("title".to_string(), Value::String(title.to_string()));
        }
        vault
            .write_note(note_id, &metadata, body)
            .expect("write note");
        vault.load_note(note_id).expect("load note")
    }

    fn make_resolved_links(
        extraction: &MetadataExtraction,
        note_ids: &HashSet<String>,
    ) -> Vec<LinkResolutionRecord> {
        extraction
            .wikilinks
            .iter()
            .map(|link| {
                let target = note_ids.contains(&link.target).then(|| link.target.clone());
                LinkResolutionRecord {
                    raw: link.raw.clone(),
                    target,
                    display: link.display.clone(),
                    heading: link.heading.clone(),
                    reason: if note_ids.contains(&link.target) {
                        LinkReason::Direct
                    } else {
                        LinkReason::Unresolved
                    },
                }
            })
            .collect()
    }

    fn seed_metrics(database: &IndexDatabase) {
        let rows = parse_metrics_reader(
            Cursor::new(
                concat!(
                    r#"{"id":"01AAA","ts":"2026-04-14T08:30:00+00:00","date":"2026-04-14","key":"body.weight","value":105.6,"unit":"kg","source":"withings","note":"Morning weigh-in"}"#,
                    "\n",
                    r#"{"id":"01AAB","ts":"2026-04-15T08:30:00+00:00","date":"2026-04-15","key":"body.weight","value":105.2,"unit":"kg","source":"withings","note":"Follow-up weigh-in"}"#
                ),
            ),
            PathBuf::from("Metrics/All.metrics.ndjson").as_path(),
        )
        .expect("parse metrics");
        database
            .upsert_metrics_file("Metrics/All.metrics.ndjson", Utc::now(), &rows, Utc::now())
            .expect("upsert metrics");
    }

    #[tokio::test]
    async fn note_context_surfaces_related_metrics_and_notes() {
        let (_dir, service) = build_service();
        let payload = service
            .note("Project Hub", Some(5), Some(5))
            .await
            .expect("note context");

        assert_eq!(payload.summary.kind, ContextTargetKind::Note);
        assert_eq!(payload.summary.target, "Project Hub");
        assert_eq!(payload.activity.metrics.len(), 1);
        assert!(
            payload
                .links
                .items
                .iter()
                .any(|link| link.to == "metric:01AAA"),
            "expected explicit metric link"
        );
        assert!(
            payload
                .related
                .notes
                .iter()
                .any(|note| note.note_id == "2026-04-14" || note.note_id == "Related Note"),
            "expected related graph note"
        );
    }

    #[tokio::test]
    async fn metric_context_accepts_key_targets() {
        let (_dir, service) = build_service();
        let payload = service
            .metric("body.weight", None, Some(5), Some(5))
            .await
            .expect("metric context");

        assert_eq!(payload.summary.kind, ContextTargetKind::Metric);
        assert_eq!(payload.summary.target, "body.weight");
        assert_eq!(payload.activity.metrics.len(), 2);
        assert!(
            payload
                .related
                .sources
                .iter()
                .any(|source| source == "withings"),
            "expected source from metric history"
        );
        assert!(
            payload
                .related
                .notes
                .iter()
                .any(|note| note.note_id == "Project Hub"),
            "expected textual note match"
        );
    }

    #[tokio::test]
    async fn source_context_surfaces_metric_keys() {
        let (_dir, service) = build_service();
        let payload = service
            .source("withings", None, Some(5), Some(5))
            .await
            .expect("source context");

        assert_eq!(payload.summary.kind, ContextTargetKind::Source);
        assert_eq!(payload.summary.target, "withings");
        assert!(
            payload
                .related
                .metric_keys
                .iter()
                .any(|key| key == "body.weight"),
            "expected related metric key"
        );
        assert!(
            payload.summary.metric_count >= 1,
            "expected metrics for source context"
        );
    }

    #[test]
    fn literal_phrase_matching_avoids_stemmed_false_positives() {
        let mut metadata = MetadataMap::default();
        metadata.insert(
            "title".to_string(),
            Value::String("Restaurant Menu".to_string()),
        );
        let note = NoteRecord {
            id: "Restaurant Menu".to_string(),
            title: Some("Restaurant Menu".to_string()),
            metadata,
            content: "Parmigiana [[with]] sulguni and walnuts.".to_string(),
            relative_path: PathBuf::from("Restaurant Menu.md"),
            file_modified_at: Utc::now(),
            created_at: None,
        };

        assert!(note_contains_literal_phrase(
            &build_note_record("Withings Log", "Tracking Withings weight syncs."),
            "withings"
        ));
        assert!(
            !note_contains_literal_phrase(&note, "withings"),
            "stemmed `with` matches should not satisfy a literal source lookup"
        );
    }

    fn build_note_record(note_id: &str, body: &str) -> NoteRecord {
        let mut metadata = MetadataMap::default();
        metadata.insert("title".to_string(), Value::String(note_id.to_string()));
        NoteRecord {
            id: note_id.to_string(),
            title: Some(note_id.to_string()),
            metadata,
            content: body.to_string(),
            relative_path: PathBuf::from(format!("{note_id}.md")),
            file_modified_at: Utc::now(),
            created_at: None,
        }
    }
}
