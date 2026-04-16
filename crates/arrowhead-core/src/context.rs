//! Context aggregation across notes, metrics, and sources.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, Months, NaiveDate, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task;

use crate::{
    GraphService, LinkEdge, LinkReason, MetricFileSummary, MetricRecordEntry, MetricsService,
    NoteRecord, SearchResult, SearchService, Vault,
    query::{DateRange, parse_absolute_date, parse_relative_range},
    sqlite::{IndexDatabase, IndexedNoteRecord},
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
static ABSOLUTE_DATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{4}-\d{2}-\d{2}\b").expect("valid absolute date regex"));
static METRIC_KEY_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)+\b").expect("valid metric key regex")
});

/// Context target classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextTargetKind {
    /// Day-oriented context.
    Day,
    /// Week-oriented context.
    Week,
    /// Month-oriented context.
    Month,
    /// Recently changed context.
    Changed,
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
    /// Optional file creation timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    /// Optional short preview or excerpt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Optional explanation for why the note is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Relationship bucket classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// Lightweight metric summary surfaced by context responses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextMetricItem {
    /// Stable metric identifier.
    pub metric_id: String,
    /// Metric key such as `body.weight`.
    pub key: String,
    /// Metric value.
    pub value: f64,
    /// Optional unit associated with the value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Metric source identifier.
    pub source: String,
    /// Optional metrics date.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<NaiveDate>,
    /// Metric timestamp.
    pub ts: DateTime<Utc>,
    /// Optional explanation for why the metric is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Aggregated per-day metric trend surfaced by context responses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextMetricRollup {
    /// Metric key such as `body.weight`.
    pub key: String,
    /// Optional source when the trend is source-specific.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Optional unit associated with the rollup values.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Number of active day buckets represented by the full rollup.
    pub active_day_count: usize,
    /// Number of individual records folded into the rollup.
    pub matching_record_count: usize,
    /// Optional explanation for why the rollup is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Most recent daily buckets for this trend.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buckets: Vec<ContextMetricRollupBucket>,
}

/// One day bucket within a context metric rollup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextMetricRollupBucket {
    /// Calendar day for the bucket.
    pub date: NaiveDate,
    /// Aggregated value for the bucket.
    pub value: f64,
    /// Number of records folded into the bucket.
    pub record_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ContextEvidenceKind {
    Explicit,
    Structural,
    Inferred,
}

#[derive(Debug, Clone)]
struct NoteMetricEvidence {
    note: ContextNoteItem,
    kind: ContextEvidenceKind,
    link_reason: String,
    confidence: Option<f32>,
}

#[derive(Debug, Clone)]
struct MetricRecordEvidence {
    record: MetricRecordEntry,
    kind: ContextEvidenceKind,
    link_reason: String,
    confidence: Option<f32>,
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
    /// Notes created inside the context window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes_created: Vec<ContextNoteItem>,
    /// Notes updated inside the context window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes_updated: Vec<ContextNoteItem>,
    /// Metric activity associated with the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metrics: Vec<MetricRecordEntry>,
    /// Link activity associated with the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<ContextLink>,
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
    /// Day identifiers adjacent to the target or active within the window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<String>,
    /// Notes adjacent to the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<ContextNoteItem>,
    /// Metrics adjacent to the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metrics: Vec<ContextMetricItem>,
    /// Aggregated metric rollups adjacent to the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metric_rollups: Vec<ContextMetricRollup>,
    /// Metric keys adjacent to the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metric_keys: Vec<String>,
    /// Sources adjacent to the target.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
}

/// Suggested follow-up command or read to continue exploration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextPivot {
    /// Stable pivot kind.
    pub kind: String,
    /// Target identifier for the next step.
    pub target: String,
    /// Concrete command suggestion.
    pub command: String,
    /// Why this follow-up is worth exploring.
    pub reason: String,
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
    /// Suggested next steps for further exploration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pivots: Vec<ContextPivot>,
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

/// Selects which week should be inspected by `ContextService::week`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeekContextSelector {
    /// Week containing the current day.
    ThisWeek,
    /// Week preceding the current day.
    LastWeek,
    /// Week containing the supplied day.
    ContainingDay(NaiveDate),
}

/// Selects which month should be inspected by `ContextService::month`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonthContextSelector {
    /// Month containing the current day.
    ThisMonth,
    /// Month preceding the current day.
    LastMonth,
    /// Month containing the supplied day.
    ContainingDay(NaiveDate),
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
        if related_notes.len() < note_limit {
            if let Ok(search_results) = self
                .search
                .related_to_note(&note.id, Some(note_limit * 2))
                .await
            {
                let semantic_matches = search_results
                    .into_iter()
                    .map(|result| note_item_from_search_result(result, None))
                    .collect::<Vec<_>>();
                merge_note_items(
                    &mut related_notes,
                    semantic_matches,
                    note_limit,
                    &mut seen_note_ids,
                );
            }
        }

        let explicit_metric_ids = extract_metric_references(&note);
        let note_dates = extract_note_dates(&note);
        let metric_evidence = self
            .note_metric_record_evidence(&note, &explicit_metric_ids, &note_dates, metric_limit)
            .await?;
        let metric_reasons = metric_reasons_from_evidence(&metric_evidence);
        let metrics = metric_evidence
            .iter()
            .map(|evidence| evidence.record.clone())
            .collect::<Vec<_>>();
        let mut links = metric_evidence
            .iter()
            .map(|evidence| ContextLink {
                kind: context_evidence_link_kind(evidence.kind),
                from: format!("note:{}", note.id),
                to: format!("metric:{}", evidence.record.record.id),
                reason: evidence.link_reason.clone(),
                confidence: evidence.confidence,
            })
            .collect::<Vec<_>>();
        links.extend(note_day_links(&note.id, &note_dates));

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
        dedup_context_links(&mut links);
        let related_metric_items = metric_items_from_records_dedup_by_key_with_reasons(
            &metrics,
            Some(&metric_reasons),
            metric_limit,
        );
        let pivot_days = note_dates
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let pivots =
            build_note_pivots(&note.id, &pivot_days, &related_notes, &related_metric_items);

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
                notes_created: Vec::new(),
                notes_updated: vec![note_item_from_note_record(
                    &note,
                    Some("Anchor note".to_string()),
                )],
                metrics: metrics.clone(),
                links: Vec::new(),
                files,
            },
            links: ContextLinks { items: links },
            attention: ContextAttention { items: attention },
            related: ContextRelated {
                days: pivot_days,
                notes: related_notes,
                metrics: related_metric_items,
                metric_rollups: Vec::new(),
                metric_keys: unique_metric_keys(&metrics),
                sources: unique_metric_sources(&metrics),
            },
            pivots,
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
        let (mut all_metrics, target_value, label) = if let Some(record) = exact_record {
            let mut records = self
                .metrics
                .search(
                    &build_metrics_field_query("key", &record.record.key, range),
                    None,
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
            )
        } else {
            if target.starts_with("metric:") {
                bail!("metric {target} was not found in the index");
            }
            let records = self
                .metrics
                .search(&build_metrics_field_query("key", target, range), None)
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
            )
        };

        sort_metric_records(&mut all_metrics);
        let metric_rollups =
            metric_rollups_for_metric_target(&all_metrics, metric_limit.clamp(1, 5));
        let mut metrics = all_metrics.clone();
        sort_metric_records(&mut metrics);
        trim_metric_records(&mut metrics, metric_limit);

        let note_evidence = self
            .metric_note_evidence(&target_value, &metrics, note_limit)
            .await?;
        let related_notes = note_evidence
            .iter()
            .map(|evidence| evidence.note.clone())
            .collect::<Vec<_>>();

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
        for evidence in &note_evidence {
            links.push(ContextLink {
                kind: context_evidence_link_kind(evidence.kind),
                from: format!("note:{}", evidence.note.note_id),
                to: target_value.clone(),
                reason: evidence.link_reason.clone(),
                confidence: evidence.confidence,
            });
        }

        let attention = attention_items_for_metrics(&metrics);
        let files = self.metric_files_for_records(&metrics).await?;
        let mut exclude_metric_keys = metrics
            .iter()
            .map(|record| record.record.key.clone())
            .collect::<HashSet<_>>();
        if !target.starts_with("metric:") {
            exclude_metric_keys.insert(target.to_string());
        }
        let related_metric_items = self
            .related_metric_items_for_records(
                &metrics,
                &exclude_metric_keys,
                metric_limit,
                "Recorded on the same day as the target metric on",
            )
            .await?;
        let related_metric_keys = if related_metric_items.is_empty() {
            unique_metric_keys(&metrics)
        } else {
            related_metric_items
                .iter()
                .map(|metric| metric.key.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        };
        let related_days = active_days_from_notes_and_metrics(&related_notes, &metrics)
            .into_iter()
            .collect::<Vec<_>>();
        let pivots = build_metric_pivots(
            &target_value,
            &metrics,
            &related_notes,
            &related_days,
            &related_metric_items,
            &metric_rollups,
            range,
        );

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
                notes_created: Vec::new(),
                notes_updated: Vec::new(),
                metrics: metrics.clone(),
                links: Vec::new(),
                files,
            },
            links: ContextLinks { items: links },
            attention: ContextAttention { items: attention },
            related: ContextRelated {
                days: related_days,
                notes: related_notes,
                metrics: related_metric_items,
                metric_rollups,
                metric_keys: related_metric_keys,
                sources: unique_metric_sources(&metrics),
            },
            pivots,
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

        let note_evidence = self
            .source_note_evidence(source, &metrics, note_limit)
            .await?;
        let related_notes = note_evidence
            .iter()
            .map(|evidence| evidence.note.clone())
            .collect::<Vec<_>>();

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
        for evidence in &note_evidence {
            links.push(ContextLink {
                kind: context_evidence_link_kind(evidence.kind),
                from: format!("note:{}", evidence.note.note_id),
                to: format!("source:{source}"),
                reason: evidence.link_reason.clone(),
                confidence: evidence.confidence,
            });
        }

        let attention = attention_items_for_metrics(&metrics);
        let files = self.metric_files_for_records(&metrics).await?;
        let related_metric_items = metric_items_from_records_dedup_by_key(
            &metrics,
            Some("Metric emitted by requested source".to_string()),
            metric_limit,
        );
        let related_days = active_days_from_notes_and_metrics(&related_notes, &metrics)
            .into_iter()
            .collect::<Vec<_>>();
        let pivots = build_source_pivots(
            source,
            &metrics,
            &related_notes,
            &related_days,
            &related_metric_items,
        );

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
                notes_created: Vec::new(),
                notes_updated: Vec::new(),
                metrics: metrics.clone(),
                links: Vec::new(),
                files,
            },
            links: ContextLinks { items: links },
            attention: ContextAttention { items: attention },
            related: ContextRelated {
                days: related_days,
                notes: related_notes,
                metrics: related_metric_items,
                metric_rollups: Vec::new(),
                metric_keys: unique_metric_keys(&metrics),
                sources: vec![source.to_string()],
            },
            pivots,
        })
    }

    /// Build context around a specific day.
    pub async fn day(
        &self,
        day: &str,
        note_limit: Option<usize>,
        metric_limit: Option<usize>,
    ) -> Result<ContextPayload> {
        let day = parse_day(day)?;
        let note_limit = note_limit.unwrap_or(DEFAULT_CONTEXT_NOTE_LIMIT).max(1);
        let metric_limit = metric_limit.unwrap_or(DEFAULT_CONTEXT_METRIC_LIMIT).max(1);
        let day_range = date_range_for_day(day);
        let note_dates = BTreeSet::from([day]);

        let mut history_notes = self
            .note_items_for_dates(&note_dates, "Daily note for requested day")
            .await?;
        let mut created_notes = self
            .note_items_for_created_range(&day_range, "Created during requested day")
            .await?;
        let created_ids = created_notes
            .iter()
            .map(|note| note.note_id.clone())
            .collect::<HashSet<_>>();
        let mut updated_notes = self
            .note_items_for_modified_range(&day_range, "Modified during requested day")
            .await?;
        updated_notes.retain(|note| !created_ids.contains(&note.note_id));
        trim_note_items(&mut history_notes, note_limit);
        trim_note_items(&mut created_notes, note_limit);
        trim_note_items(&mut updated_notes, note_limit);
        let mut metrics = self
            .metrics
            .search(&format!("date:{day}"), Some(metric_limit))
            .await?;
        sort_metric_records(&mut metrics);
        trim_metric_records(&mut metrics, metric_limit);

        let mut backlink_notes = self
            .note_items_for_backlinks(&day.to_string(), note_limit, "Links to this day")
            .await?;
        trim_note_items(&mut backlink_notes, note_limit);
        let mut activity_links = self
            .note_links_for_notes(
                &created_notes,
                "Source note was created on the requested day",
            )
            .await?;
        activity_links.extend(
            self.note_links_for_notes(
                &updated_notes,
                "Source note was updated on the requested day",
            )
            .await?,
        );
        dedup_context_links(&mut activity_links);
        trim_context_links(&mut activity_links, note_limit.saturating_mul(2).max(1));
        let mut links = build_day_relationship_links(
            &day.to_string(),
            &history_notes,
            &backlink_notes,
            &metrics,
        );
        links.extend(activity_links.clone());
        dedup_context_links(&mut links);
        trim_context_links(
            &mut links,
            note_limit
                .saturating_mul(3)
                .saturating_add(metric_limit)
                .max(1),
        );
        let related_days = self.adjacent_active_days(day).await?;
        let files = self.metric_files_for_records(&metrics).await?;
        let mut attention = attention_items_for_metrics(&metrics);
        attention.sort_by(|left, right| left.message.cmp(&right.message));
        let note_leads = merged_note_items(
            vec![
                history_notes.clone(),
                backlink_notes.clone(),
                created_notes.clone(),
                updated_notes.clone(),
            ],
            note_limit,
        );
        let pivots = build_day_pivots(&day.to_string(), &note_leads, &metrics, &related_days);

        Ok(ContextPayload {
            summary: ContextSummary {
                kind: ContextTargetKind::Day,
                target: day.to_string(),
                label: Some(day.to_string()),
                note_count: unique_note_count([
                    history_notes.as_slice(),
                    created_notes.as_slice(),
                    updated_notes.as_slice(),
                ]),
                metric_count: metrics.len(),
                link_count: links.len(),
                attention_count: attention.len(),
            },
            history: ContextHistory {
                notes: history_notes,
                metrics: metrics.clone(),
            },
            activity: ContextActivity {
                notes: note_leads,
                notes_created: created_notes,
                notes_updated: updated_notes,
                metrics: metrics.clone(),
                links: activity_links,
                files,
            },
            links: ContextLinks { items: links },
            attention: ContextAttention { items: attention },
            related: ContextRelated {
                days: related_days,
                notes: backlink_notes,
                metrics: Vec::new(),
                metric_rollups: Vec::new(),
                metric_keys: unique_metric_keys(&metrics),
                sources: unique_metric_sources(&metrics),
            },
            pivots,
        })
    }

    /// Build context around a calendar week.
    pub async fn week(
        &self,
        selector: WeekContextSelector,
        note_limit: Option<usize>,
        metric_limit: Option<usize>,
    ) -> Result<ContextPayload> {
        let note_limit = note_limit.unwrap_or(DEFAULT_CONTEXT_NOTE_LIMIT).max(1);
        let metric_limit = metric_limit.unwrap_or(DEFAULT_CONTEXT_METRIC_LIMIT).max(1);
        let (start, end) = week_bounds(selector);
        let week_range = date_range_for_span(start, end);
        let note_dates = dates_in_range(&week_range)?;

        let mut history_notes = self
            .note_items_for_dates(&note_dates, "Daily note in requested week")
            .await?;
        let mut created_notes = self
            .note_items_for_created_range(&week_range, "Created during requested week")
            .await?;
        let created_ids = created_notes
            .iter()
            .map(|note| note.note_id.clone())
            .collect::<HashSet<_>>();
        let mut updated_notes = self
            .note_items_for_modified_range(&week_range, "Modified during requested week")
            .await?;
        updated_notes.retain(|note| !created_ids.contains(&note.note_id));
        trim_note_items(&mut history_notes, note_limit);
        trim_note_items(&mut created_notes, note_limit);
        trim_note_items(&mut updated_notes, note_limit);
        let mut metrics = self
            .metrics
            .search(
                &format!(
                    "date:{}..{}",
                    start.format("%Y-%m-%d"),
                    end.format("%Y-%m-%d")
                ),
                Some(metric_limit),
            )
            .await?;
        sort_metric_records(&mut metrics);
        trim_metric_records(&mut metrics, metric_limit);

        let related_notes = merged_note_items(
            vec![
                history_notes.clone(),
                created_notes.clone(),
                updated_notes.clone(),
            ],
            note_limit,
        );
        let files = self.metric_files_for_records(&metrics).await?;
        let mut activity_links = self
            .note_links_for_notes(
                &created_notes,
                "Source note was created during the requested week",
            )
            .await?;
        activity_links.extend(
            self.note_links_for_notes(
                &updated_notes,
                "Source note was updated during the requested week",
            )
            .await?,
        );
        dedup_context_links(&mut activity_links);
        trim_context_links(&mut activity_links, note_limit.saturating_mul(2).max(1));
        let active_days = active_days_from_notes_and_metrics(&related_notes, &metrics)
            .into_iter()
            .collect::<Vec<_>>();
        let attention = attention_items_for_metrics(&metrics);
        let links = build_window_links(
            &format!("week:{}..{}", start, end),
            "Requested week contains note activity",
            &related_notes,
            "Requested week contains metric activity",
            &metrics,
        );
        let pivots = build_day_pivots(&start.to_string(), &related_notes, &metrics, &active_days);

        Ok(ContextPayload {
            summary: ContextSummary {
                kind: ContextTargetKind::Week,
                target: format!("{start}..{end}"),
                label: Some(format!("Week of {start}")),
                note_count: unique_note_count([
                    history_notes.as_slice(),
                    created_notes.as_slice(),
                    updated_notes.as_slice(),
                ]),
                metric_count: metrics.len(),
                link_count: links.len(),
                attention_count: attention.len(),
            },
            history: ContextHistory {
                notes: history_notes,
                metrics: metrics.clone(),
            },
            activity: ContextActivity {
                notes: related_notes.clone(),
                notes_created: created_notes,
                notes_updated: updated_notes,
                metrics: metrics.clone(),
                links: activity_links,
                files,
            },
            links: ContextLinks { items: links },
            attention: ContextAttention { items: attention },
            related: ContextRelated {
                days: active_days,
                notes: related_notes,
                metrics: Vec::new(),
                metric_rollups: Vec::new(),
                metric_keys: unique_metric_keys(&metrics),
                sources: unique_metric_sources(&metrics),
            },
            pivots,
        })
    }

    /// Build context around a calendar month.
    pub async fn month(
        &self,
        selector: MonthContextSelector,
        note_limit: Option<usize>,
        metric_limit: Option<usize>,
    ) -> Result<ContextPayload> {
        let note_limit = note_limit.unwrap_or(DEFAULT_CONTEXT_NOTE_LIMIT).max(1);
        let metric_limit = metric_limit.unwrap_or(DEFAULT_CONTEXT_METRIC_LIMIT).max(1);
        let (start, end) = month_bounds(selector)?;
        let month_range = date_range_for_span(start, end);
        let note_dates = dates_in_range(&month_range)?;

        let mut history_notes = self
            .note_items_for_dates(&note_dates, "Daily note in requested month")
            .await?;
        let mut created_notes = self
            .note_items_for_created_range(&month_range, "Created during requested month")
            .await?;
        let created_ids = created_notes
            .iter()
            .map(|note| note.note_id.clone())
            .collect::<HashSet<_>>();
        let mut updated_notes = self
            .note_items_for_modified_range(&month_range, "Modified during requested month")
            .await?;
        updated_notes.retain(|note| !created_ids.contains(&note.note_id));
        trim_note_items(&mut history_notes, note_limit);
        trim_note_items(&mut created_notes, note_limit);
        trim_note_items(&mut updated_notes, note_limit);
        let all_metrics = self
            .metrics
            .search(
                &format!(
                    "date:{}..{}",
                    start.format("%Y-%m-%d"),
                    end.format("%Y-%m-%d")
                ),
                None,
            )
            .await?;
        let metric_rollups = metric_rollups_for_window(
            &all_metrics,
            metric_limit.clamp(1, 4),
            metric_limit.clamp(1, 5),
            Some("Daily trend inside the requested month".to_string()),
        );
        let mut metrics = all_metrics.clone();
        sort_metric_records(&mut metrics);
        trim_metric_records(&mut metrics, metric_limit);

        let related_notes = merged_note_items(
            vec![
                history_notes.clone(),
                created_notes.clone(),
                updated_notes.clone(),
            ],
            note_limit,
        );
        let files = self.metric_files_for_records(&all_metrics).await?;
        let mut activity_links = self
            .note_links_for_notes(
                &created_notes,
                "Source note was created during the requested month",
            )
            .await?;
        activity_links.extend(
            self.note_links_for_notes(
                &updated_notes,
                "Source note was updated during the requested month",
            )
            .await?,
        );
        dedup_context_links(&mut activity_links);
        trim_context_links(&mut activity_links, note_limit.saturating_mul(2).max(1));
        let active_days = active_days_from_notes_and_metrics(&related_notes, &all_metrics)
            .into_iter()
            .collect::<Vec<_>>();
        let attention = attention_items_for_metrics(&all_metrics);
        let links = build_window_links(
            &format!("month:{}..{}", start, end),
            "Requested month contains note activity",
            &related_notes,
            "Requested month contains metric activity",
            &metrics,
        );
        let month_range_label = format!("{start}..{end}");
        let pivots = build_month_pivots(
            &month_range_label,
            &related_notes,
            &metrics,
            &active_days,
            &metric_rollups,
        );

        Ok(ContextPayload {
            summary: ContextSummary {
                kind: ContextTargetKind::Month,
                target: format!("{start}..{end}"),
                label: Some(start.format("%B %Y").to_string()),
                note_count: unique_note_count([
                    history_notes.as_slice(),
                    created_notes.as_slice(),
                    updated_notes.as_slice(),
                ]),
                metric_count: metrics.len(),
                link_count: links.len(),
                attention_count: attention.len(),
            },
            history: ContextHistory {
                notes: history_notes,
                metrics: metrics.clone(),
            },
            activity: ContextActivity {
                notes: related_notes.clone(),
                notes_created: created_notes,
                notes_updated: updated_notes,
                metrics: metrics.clone(),
                links: activity_links,
                files,
            },
            links: ContextLinks { items: links },
            attention: ContextAttention { items: attention },
            related: ContextRelated {
                days: active_days,
                notes: related_notes,
                metrics: Vec::new(),
                metric_rollups,
                metric_keys: unique_metric_keys(&all_metrics),
                sources: unique_metric_sources(&all_metrics),
            },
            pivots,
        })
    }

    /// Build context around recently changed notes and metrics.
    pub async fn changed(
        &self,
        days: usize,
        note_limit: Option<usize>,
        metric_limit: Option<usize>,
    ) -> Result<ContextPayload> {
        if days == 0 {
            bail!("changed window must cover at least one day");
        }

        let note_limit = note_limit.unwrap_or(DEFAULT_CONTEXT_NOTE_LIMIT).max(1);
        let metric_limit = metric_limit.unwrap_or(DEFAULT_CONTEXT_METRIC_LIMIT).max(1);
        let changed_range = parse_relative_range(&format!("past{days}d"), Utc::now())?
            .context("failed to build changed context window")?;

        let mut created_notes = self
            .note_items_for_created_range(&changed_range, "Created recently")
            .await?;
        let created_ids = created_notes
            .iter()
            .map(|note| note.note_id.clone())
            .collect::<HashSet<_>>();
        let mut updated_notes = self
            .note_items_for_modified_range(&changed_range, "Modified recently")
            .await?;
        updated_notes.retain(|note| !created_ids.contains(&note.note_id));
        let note_dates = active_dates_from_notes_and_metrics(&updated_notes, &[]);
        let mut history_notes = self
            .note_items_for_dates(&note_dates, "Daily note in changed window")
            .await?;
        trim_note_items(&mut history_notes, note_limit);
        trim_note_items(&mut created_notes, note_limit);
        trim_note_items(&mut updated_notes, note_limit);
        let (start, end) = date_bounds_for_range(&changed_range)?;
        let mut metrics = self
            .metrics
            .search(
                &format!(
                    "date:{}..{}",
                    start.format("%Y-%m-%d"),
                    end.format("%Y-%m-%d")
                ),
                Some(metric_limit),
            )
            .await?;
        sort_metric_records(&mut metrics);
        trim_metric_records(&mut metrics, metric_limit);

        let related_notes = merged_note_items(
            vec![
                history_notes.clone(),
                created_notes.clone(),
                updated_notes.clone(),
            ],
            note_limit,
        );
        let files = merge_metric_files(
            self.metric_files_for_records(&metrics).await?,
            self.metric_files_for_modified_range(&changed_range).await?,
        );
        let mut activity_links = self
            .note_links_for_notes(&created_notes, "Source note was created recently")
            .await?;
        activity_links.extend(
            self.note_links_for_notes(&updated_notes, "Source note was updated recently")
                .await?,
        );
        dedup_context_links(&mut activity_links);
        trim_context_links(&mut activity_links, note_limit.saturating_mul(2).max(1));
        let active_days = active_days_from_notes_and_metrics(&related_notes, &metrics)
            .into_iter()
            .collect::<Vec<_>>();
        let attention = attention_items_for_metrics(&metrics);
        let links = build_window_links(
            &format!("changed:past{days}d"),
            "Recently changed note",
            &related_notes,
            "Recently recorded metric",
            &metrics,
        );
        let pivots = build_day_pivots(
            &format!("past{days}d"),
            &related_notes,
            &metrics,
            &active_days,
        );

        Ok(ContextPayload {
            summary: ContextSummary {
                kind: ContextTargetKind::Changed,
                target: format!("past{days}d"),
                label: Some(format!(
                    "Changed in past {days} day{}",
                    if days == 1 { "" } else { "s" }
                )),
                note_count: unique_note_count([
                    history_notes.as_slice(),
                    created_notes.as_slice(),
                    updated_notes.as_slice(),
                ]),
                metric_count: metrics.len(),
                link_count: links.len(),
                attention_count: attention.len(),
            },
            history: ContextHistory {
                notes: history_notes,
                metrics: metrics.clone(),
            },
            activity: ContextActivity {
                notes: related_notes.clone(),
                notes_created: created_notes,
                notes_updated: updated_notes,
                metrics: metrics.clone(),
                links: activity_links,
                files,
            },
            links: ContextLinks { items: links },
            attention: ContextAttention { items: attention },
            related: ContextRelated {
                days: active_days,
                notes: related_notes,
                metrics: Vec::new(),
                metric_rollups: Vec::new(),
                metric_keys: unique_metric_keys(&metrics),
                sources: unique_metric_sources(&metrics),
            },
            pivots,
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
                    created_at: entry.and_then(|item| item.created_at),
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
            .filter(|result| !is_context_noise_search_result(result))
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

    async fn note_items_for_dates(
        &self,
        dates: &BTreeSet<NaiveDate>,
        reason: &str,
    ) -> Result<Vec<ContextNoteItem>> {
        if dates.is_empty() {
            return Ok(Vec::new());
        }

        let vault = Arc::clone(&self.vault);
        let database = Arc::clone(&self.database);
        let reason = reason.to_string();
        let dates = dates.clone();
        task::spawn_blocking(move || -> Result<Vec<ContextNoteItem>> {
            let snapshot = vault.inventory_snapshot()?;
            let date_strings = dates
                .iter()
                .map(ToString::to_string)
                .collect::<HashSet<_>>();
            let mut note_ids = snapshot
                .entries()
                .iter()
                .filter(|entry| date_strings.contains(&entry.id))
                .map(|entry| entry.id.clone())
                .collect::<Vec<_>>();
            note_ids.sort();

            let titles = database.titles_for_notes(&note_ids)?;
            let relative_paths = database.relative_paths_for_notes(&note_ids)?;
            let mut items = Vec::new();
            for note_id in note_ids {
                let entry = snapshot.get_by_id(&note_id);
                items.push(ContextNoteItem {
                    note_id: note_id.clone(),
                    title: titles.get(&note_id).cloned().unwrap_or(None),
                    relative_path: entry
                        .map(|item| item.relative_path.clone())
                        .or_else(|| relative_paths.get(&note_id).map(PathBuf::from)),
                    file_modified_at: entry.map(|item| item.file_modified_at),
                    created_at: entry.and_then(|item| item.created_at),
                    preview: database.note_excerpt(&note_id, 240)?,
                    reason: Some(reason.clone()),
                });
            }
            Ok(items)
        })
        .await
        .context("note-by-date task aborted")?
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
        self.note_items_for_dates(&dates, reason).await
    }

    async fn note_items_for_created_range(
        &self,
        range: &DateRange,
        reason: &str,
    ) -> Result<Vec<ContextNoteItem>> {
        let database = Arc::clone(&self.database);
        let reason = reason.to_string();
        let range = range.clone();
        task::spawn_blocking(move || -> Result<Vec<ContextNoteItem>> {
            let (start_micros, end_micros) = range_bounds_micros(&range)?;
            let entries = database.notes_created_between(start_micros, end_micros)?;
            indexed_notes_to_context_items(&database, entries, &reason)
        })
        .await
        .context("note-by-created task aborted")?
    }

    async fn note_items_for_modified_range(
        &self,
        range: &DateRange,
        reason: &str,
    ) -> Result<Vec<ContextNoteItem>> {
        let database = Arc::clone(&self.database);
        let reason = reason.to_string();
        let range = range.clone();
        task::spawn_blocking(move || -> Result<Vec<ContextNoteItem>> {
            let (start_micros, end_micros) = range_bounds_micros(&range)?;
            let entries = database.notes_modified_between(start_micros, end_micros)?;
            indexed_notes_to_context_items(&database, entries, &reason)
        })
        .await
        .context("note-by-modified task aborted")?
    }

    async fn note_items_for_backlinks(
        &self,
        note_id: &str,
        limit: usize,
        reason: &str,
    ) -> Result<Vec<ContextNoteItem>> {
        let backlinks = self.graph.backlinks(note_id).await?;
        let mut backlink_ids = backlinks
            .into_iter()
            .map(|edge| edge.source)
            .collect::<Vec<_>>();
        backlink_ids.sort();
        backlink_ids.dedup();
        let mut items = self.note_items_for_ids(&backlink_ids, Some(reason)).await?;
        trim_note_items(&mut items, limit);
        Ok(items)
    }

    async fn note_links_for_notes(
        &self,
        notes: &[ContextNoteItem],
        note_reason: &str,
    ) -> Result<Vec<ContextLink>> {
        let mut note_ids = notes
            .iter()
            .filter(|note| !is_context_noise_note_item(note))
            .map(|note| note.note_id.clone())
            .collect::<Vec<_>>();
        note_ids.sort();
        note_ids.dedup();

        let mut links = Vec::new();
        for note_id in note_ids {
            let edges = self.graph.forward_links(&note_id).await?;
            for edge in edges {
                let mut link = context_link_from_graph_edge(&edge);
                link.reason = format!("{note_reason}; {}", link.reason);
                links.push(link);
            }
        }
        Ok(links)
    }

    async fn metric_note_evidence(
        &self,
        metric_target: &str,
        metric_records: &[MetricRecordEntry],
        note_limit: usize,
    ) -> Result<Vec<NoteMetricEvidence>> {
        if metric_records.is_empty() || note_limit == 0 {
            return Ok(Vec::new());
        }

        let metric_key = metric_records
            .first()
            .map(|record| record.record.key.clone())
            .unwrap_or_else(|| metric_target.to_string());
        let explicit_terms = metric_records
            .iter()
            .map(|record| format!("metric:{}", record.record.id))
            .collect::<BTreeSet<_>>();

        let mut evidence_by_note = HashMap::new();
        let empty_excludes = HashSet::new();

        for term in explicit_terms {
            let results = self
                .search_notes_by_phrase(
                    &term,
                    note_limit.saturating_mul(3).max(1),
                    &empty_excludes,
                    "Explicit metric reference",
                )
                .await?;
            for note in results {
                let reason = format!("Note explicitly references matching metric record `{term}`");
                upsert_note_metric_evidence(
                    &mut evidence_by_note,
                    NoteMetricEvidence {
                        note,
                        kind: ContextEvidenceKind::Explicit,
                        link_reason: reason,
                        confidence: None,
                    },
                );
            }
        }

        let date_notes = self
            .note_items_for_metric_dates(metric_records, "Same day as metric activity")
            .await?;
        for note in date_notes {
            let reason = note
                .reason
                .clone()
                .unwrap_or_else(|| "Same day as metric activity".to_string());
            upsert_note_metric_evidence(
                &mut evidence_by_note,
                NoteMetricEvidence {
                    note,
                    kind: ContextEvidenceKind::Structural,
                    link_reason: reason,
                    confidence: None,
                },
            );
        }

        let text_matches = self
            .search_notes_by_literal_phrase(
                &metric_key,
                note_limit.saturating_mul(4).max(1),
                &empty_excludes,
                "Note text matches metric key",
            )
            .await?;
        for note in text_matches {
            let reason = note
                .reason
                .clone()
                .unwrap_or_else(|| "Note text matches metric key".to_string());
            upsert_note_metric_evidence(
                &mut evidence_by_note,
                NoteMetricEvidence {
                    note,
                    kind: ContextEvidenceKind::Inferred,
                    link_reason: reason,
                    confidence: Some(0.35),
                },
            );
        }

        let mut evidence = evidence_by_note.into_values().collect::<Vec<_>>();
        sort_note_metric_evidence(&mut evidence);
        if evidence.len() > note_limit {
            evidence.truncate(note_limit);
        }
        Ok(evidence)
    }

    async fn source_note_evidence(
        &self,
        source: &str,
        metric_records: &[MetricRecordEntry],
        note_limit: usize,
    ) -> Result<Vec<NoteMetricEvidence>> {
        if metric_records.is_empty() || note_limit == 0 {
            return Ok(Vec::new());
        }

        let mut evidence_by_note = HashMap::new();
        let empty_excludes = HashSet::new();
        let source_phrase_reason = format!("Note explicitly mentions source `{source}`");
        let source_mentions = self
            .search_notes_by_literal_phrase(
                source,
                note_limit.saturating_mul(4).max(1),
                &empty_excludes,
                "Note text matches source",
            )
            .await?;
        for note in source_mentions {
            upsert_note_metric_evidence(
                &mut evidence_by_note,
                NoteMetricEvidence {
                    note,
                    kind: ContextEvidenceKind::Explicit,
                    link_reason: source_phrase_reason.clone(),
                    confidence: None,
                },
            );
        }

        let date_notes = self
            .note_items_for_metric_dates(metric_records, "Same day as source metric activity")
            .await?;
        for note in date_notes {
            let reason = note
                .reason
                .clone()
                .unwrap_or_else(|| "Same day as source metric activity".to_string());
            upsert_note_metric_evidence(
                &mut evidence_by_note,
                NoteMetricEvidence {
                    note,
                    kind: ContextEvidenceKind::Structural,
                    link_reason: reason,
                    confidence: None,
                },
            );
        }

        let metric_keys = metric_records
            .iter()
            .map(|record| record.record.key.clone())
            .collect::<BTreeSet<_>>();
        for metric_key in metric_keys {
            let text_matches = self
                .search_notes_by_literal_phrase(
                    &metric_key,
                    note_limit.saturating_mul(3).max(1),
                    &empty_excludes,
                    "Note text matches source metric key",
                )
                .await?;
            for note in text_matches {
                let reason = format!(
                    "Note text mentions metric key `{metric_key}` emitted by source `{source}`"
                );
                upsert_note_metric_evidence(
                    &mut evidence_by_note,
                    NoteMetricEvidence {
                        note,
                        kind: ContextEvidenceKind::Inferred,
                        link_reason: reason,
                        confidence: Some(0.3),
                    },
                );
            }
        }

        let mut evidence = evidence_by_note.into_values().collect::<Vec<_>>();
        sort_note_metric_evidence(&mut evidence);
        if evidence.len() > note_limit {
            evidence.truncate(note_limit);
        }
        Ok(evidence)
    }

    async fn related_metric_items_for_records(
        &self,
        records: &[MetricRecordEntry],
        exclude_keys: &HashSet<String>,
        limit: usize,
        reason_prefix: &str,
    ) -> Result<Vec<ContextMetricItem>> {
        if records.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }

        let mut items = Vec::new();
        let mut seen_keys = HashSet::new();
        for record in records {
            let Some(date) = record.record.date else {
                continue;
            };
            let related = self
                .metrics
                .search(&format!("date:{date}"), Some(limit.max(1) * 4))
                .await?;
            for candidate in related {
                if exclude_keys.contains(&candidate.record.key) {
                    continue;
                }
                if seen_keys.insert(candidate.record.key.clone()) {
                    items.push(context_metric_item_from_record(
                        &candidate,
                        Some(format!("{reason_prefix} {date}")),
                    ));
                }
                if items.len() >= limit {
                    return Ok(items);
                }
            }
        }

        Ok(items)
    }

    async fn note_metric_record_evidence(
        &self,
        note: &NoteRecord,
        explicit_metric_ids: &[String],
        note_dates: &BTreeSet<NaiveDate>,
        metric_limit: usize,
    ) -> Result<Vec<MetricRecordEvidence>> {
        if metric_limit == 0 {
            return Ok(Vec::new());
        }

        let mut evidence_by_metric = HashMap::new();
        for metric_id in explicit_metric_ids {
            if let Some(record) = self.metrics.read_record(metric_id).await? {
                upsert_metric_record_evidence(
                    &mut evidence_by_metric,
                    MetricRecordEvidence {
                        record,
                        kind: ContextEvidenceKind::Explicit,
                        link_reason: "Note contains explicit metric reference".to_string(),
                        confidence: None,
                    },
                );
            }
        }

        for date in note_dates {
            let results = self
                .metrics
                .search(
                    &format!("date:{date}"),
                    Some(metric_limit.saturating_mul(2).max(1)),
                )
                .await?;
            for record in results {
                upsert_metric_record_evidence(
                    &mut evidence_by_metric,
                    MetricRecordEvidence {
                        record,
                        kind: ContextEvidenceKind::Structural,
                        link_reason: format!("Note references day {date} with recorded metrics"),
                        confidence: None,
                    },
                );
            }
        }

        let stronger_keys = evidence_by_metric
            .values()
            .filter(|evidence| evidence.kind != ContextEvidenceKind::Inferred)
            .map(|evidence| evidence.record.record.key.clone())
            .collect::<HashSet<_>>();
        for metric_key in extract_metric_key_mentions(note) {
            if stronger_keys.contains(&metric_key) {
                continue;
            }
            let mut results = self
                .metrics
                .search(
                    &build_metrics_field_query("key", &metric_key, None),
                    Some(metric_limit.saturating_mul(2).max(1)),
                )
                .await?;
            if results.is_empty() {
                continue;
            }
            sort_metric_records(&mut results);
            if let Some(record) = results.into_iter().next() {
                upsert_metric_record_evidence(
                    &mut evidence_by_metric,
                    MetricRecordEvidence {
                        record,
                        kind: ContextEvidenceKind::Inferred,
                        link_reason: format!("Note text mentions metric key `{metric_key}`"),
                        confidence: Some(0.35),
                    },
                );
            }
        }

        let mut evidence = evidence_by_metric.into_values().collect::<Vec<_>>();
        sort_metric_record_evidence(&mut evidence);
        if evidence.len() > metric_limit {
            evidence.truncate(metric_limit);
        }
        Ok(evidence)
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

    async fn metric_files_for_modified_range(
        &self,
        range: &DateRange,
    ) -> Result<Vec<MetricFileSummary>> {
        let mut files = self.metrics.list_files().await?;
        files.retain(|file| range_contains_timestamp(range, file.file_modified_at));
        files.sort_by(|left, right| {
            right
                .file_modified_at
                .cmp(&left.file_modified_at)
                .then_with(|| left.relative_path.cmp(&right.relative_path))
        });
        Ok(files)
    }

    async fn adjacent_active_days(&self, day: NaiveDate) -> Result<Vec<String>> {
        let previous = day - Duration::days(1);
        let next = day + Duration::days(1);
        let mut related = Vec::new();
        if self.day_has_activity(previous).await? {
            related.push(previous.to_string());
        }
        if self.day_has_activity(next).await? {
            related.push(next.to_string());
        }
        Ok(related)
    }

    async fn day_has_activity(&self, day: NaiveDate) -> Result<bool> {
        let notes = self
            .note_items_for_dates(&BTreeSet::from([day]), "Adjacent day")
            .await?;
        if !notes.is_empty() {
            return Ok(true);
        }

        let metrics = self.metrics.search(&format!("date:{day}"), Some(1)).await?;
        Ok(!metrics.is_empty())
    }
}

fn parse_day(input: &str) -> Result<NaiveDate> {
    let parsed = parse_absolute_date(input.trim())
        .with_context(|| format!("invalid day `{}`", input.trim()))?;
    Ok(parsed.instant.date_naive())
}

fn date_range_for_day(day: NaiveDate) -> DateRange {
    date_range_for_span(day, day)
}

fn date_range_for_span(start: NaiveDate, end: NaiveDate) -> DateRange {
    let start_dt = start
        .and_hms_opt(0, 0, 0)
        .expect("valid start of day")
        .and_utc();
    let end_dt = end
        .and_hms_opt(23, 59, 59)
        .expect("valid end of day")
        .and_utc()
        + Duration::microseconds(999_999);
    DateRange::new(
        Some(crate::query::DateRangeBound {
            value: start_dt,
            inclusive: true,
        }),
        Some(crate::query::DateRangeBound {
            value: end_dt,
            inclusive: true,
        }),
    )
}

fn date_bounds_for_range(range: &DateRange) -> Result<(NaiveDate, NaiveDate)> {
    let start = range
        .start
        .as_ref()
        .map(|bound| bound.value.date_naive())
        .context("range is missing a start bound")?;
    let end = range
        .end
        .as_ref()
        .map(|bound| bound.value.date_naive())
        .context("range is missing an end bound")?;
    Ok((start, end))
}

fn dates_in_range(range: &DateRange) -> Result<BTreeSet<NaiveDate>> {
    let (start, end) = date_bounds_for_range(range)?;
    let mut dates = BTreeSet::new();
    let mut cursor = start;
    while cursor <= end {
        dates.insert(cursor);
        cursor += Duration::days(1);
    }
    Ok(dates)
}

fn week_bounds(selector: WeekContextSelector) -> (NaiveDate, NaiveDate) {
    let anchor = match selector {
        WeekContextSelector::ThisWeek => Utc::now().date_naive(),
        WeekContextSelector::LastWeek => Utc::now().date_naive() - Duration::weeks(1),
        WeekContextSelector::ContainingDay(day) => day,
    };
    let weekday_offset = i64::from(anchor.weekday().num_days_from_monday());
    let start = anchor - Duration::days(weekday_offset);
    let end = start + Duration::days(6);
    (start, end)
}

fn month_bounds(selector: MonthContextSelector) -> Result<(NaiveDate, NaiveDate)> {
    let anchor = match selector {
        MonthContextSelector::ThisMonth => Utc::now().date_naive(),
        MonthContextSelector::LastMonth => Utc::now()
            .date_naive()
            .checked_sub_months(Months::new(1))
            .context("failed to resolve previous month")?,
        MonthContextSelector::ContainingDay(day) => day,
    };
    let start = anchor
        .with_day(1)
        .context("failed to resolve start of month")?;
    let end = start
        .checked_add_months(Months::new(1))
        .context("failed to resolve end of month")?
        - Duration::days(1);
    Ok((start, end))
}

fn range_contains_timestamp(range: &DateRange, timestamp: DateTime<Utc>) -> bool {
    let micros = timestamp.timestamp_micros();
    let lower_ok = range
        .lower_bound_micros()
        .is_none_or(|lower| micros >= lower);
    let upper_ok = range
        .upper_bound_micros()
        .is_none_or(|upper| micros <= upper);
    lower_ok && upper_ok
}

fn merged_note_items(groups: Vec<Vec<ContextNoteItem>>, limit: usize) -> Vec<ContextNoteItem> {
    let mut merged = Vec::new();
    let mut seen = HashSet::new();
    for group in groups {
        merge_note_items(&mut merged, group, limit, &mut seen);
        if merged.len() >= limit {
            break;
        }
    }
    merged
}

fn unique_note_count<const N: usize>(groups: [&[ContextNoteItem]; N]) -> usize {
    groups
        .into_iter()
        .flat_map(|items| items.iter().map(|item| item.note_id.clone()))
        .collect::<HashSet<_>>()
        .len()
}

fn build_window_links(
    target: &str,
    note_reason: &str,
    notes: &[ContextNoteItem],
    metric_reason: &str,
    metrics: &[MetricRecordEntry],
) -> Vec<ContextLink> {
    let mut links = notes
        .iter()
        .map(|note| ContextLink {
            kind: ContextLinkKind::Structural,
            from: target.to_string(),
            to: format!("note:{}", note.note_id),
            reason: note_reason.to_string(),
            confidence: None,
        })
        .collect::<Vec<_>>();
    links.extend(metrics.iter().map(|record| ContextLink {
        kind: ContextLinkKind::Structural,
        from: target.to_string(),
        to: format!("metric:{}", record.record.id),
        reason: metric_reason.to_string(),
        confidence: None,
    }));
    links
}

fn merge_metric_files(
    left: Vec<MetricFileSummary>,
    right: Vec<MetricFileSummary>,
) -> Vec<MetricFileSummary> {
    let mut merged = Vec::new();
    let mut seen = HashSet::new();
    for file in left.into_iter().chain(right) {
        if seen.insert(file.relative_path.clone()) {
            merged.push(file);
        }
    }
    merged.sort_by(|lhs, rhs| {
        rhs.file_modified_at
            .cmp(&lhs.file_modified_at)
            .then_with(|| lhs.relative_path.cmp(&rhs.relative_path))
    });
    merged
}

fn active_days_from_notes_and_metrics(
    notes: &[ContextNoteItem],
    metrics: &[MetricRecordEntry],
) -> BTreeSet<String> {
    active_dates_from_notes_and_metrics(notes, metrics)
        .into_iter()
        .map(|date| date.to_string())
        .collect()
}

fn active_dates_from_notes_and_metrics(
    notes: &[ContextNoteItem],
    metrics: &[MetricRecordEntry],
) -> BTreeSet<NaiveDate> {
    let mut days = BTreeSet::new();
    for note in notes {
        if let Some(captures) = DATE_PREFIX_RE.captures(&note.note_id) {
            if let Some(matched) = captures.get(1) {
                if let Ok(date) = NaiveDate::parse_from_str(matched.as_str(), "%Y-%m-%d") {
                    days.insert(date);
                }
            }
        }
    }
    for metric in metrics {
        if let Some(date) = metric.record.date {
            days.insert(date);
        }
    }
    days
}

fn range_bounds_micros(range: &DateRange) -> Result<(i64, i64)> {
    let start = range
        .lower_bound_micros()
        .context("range is missing a lower bound")?;
    let end = range
        .upper_bound_micros()
        .context("range is missing an upper bound")?;
    Ok((start, end))
}

fn indexed_notes_to_context_items(
    database: &IndexDatabase,
    entries: Vec<IndexedNoteRecord>,
    reason: &str,
) -> Result<Vec<ContextNoteItem>> {
    let mut items = Vec::new();
    for entry in entries {
        if is_context_noise_path(Path::new(&entry.relative_path))
            || is_context_noise_note_id(&entry.id)
        {
            continue;
        }
        items.push(ContextNoteItem {
            note_id: entry.id.clone(),
            title: entry.title.clone(),
            relative_path: Some(PathBuf::from(&entry.relative_path)),
            file_modified_at: Some(entry.file_modified_at),
            created_at: entry.created_at,
            preview: database.note_excerpt(&entry.id, 240)?,
            reason: Some(reason.to_string()),
        });
    }
    Ok(items)
}

fn context_metric_item_from_record(
    record: &MetricRecordEntry,
    reason: Option<String>,
) -> ContextMetricItem {
    ContextMetricItem {
        metric_id: record.record.id.clone(),
        key: record.record.key.clone(),
        value: record.record.value,
        unit: record.record.unit.clone(),
        source: record.record.source.clone(),
        date: record.record.date,
        ts: record.record.ts.into(),
        reason,
    }
}

fn metric_items_from_records_dedup_by_key_with_reasons(
    records: &[MetricRecordEntry],
    reasons: Option<&std::collections::HashMap<String, String>>,
    limit: usize,
) -> Vec<ContextMetricItem> {
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for record in records {
        if items.len() >= limit {
            break;
        }
        if seen.insert(record.record.key.clone()) {
            items.push(context_metric_item_from_record(
                record,
                reasons.and_then(|map| map.get(&record.record.id).cloned()),
            ));
        }
    }
    items
}

fn metric_items_from_records_dedup_by_key(
    records: &[MetricRecordEntry],
    reason: Option<String>,
    limit: usize,
) -> Vec<ContextMetricItem> {
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for record in records {
        if items.len() >= limit {
            break;
        }
        if seen.insert(record.record.key.clone()) {
            items.push(context_metric_item_from_record(record, reason.clone()));
        }
    }
    items
}

fn metric_rollups_for_metric_target(
    records: &[MetricRecordEntry],
    bucket_limit: usize,
) -> Vec<ContextMetricRollup> {
    if records.is_empty() || bucket_limit == 0 {
        return Vec::new();
    }

    let sources = records
        .iter()
        .map(|record| record.record.source.clone())
        .collect::<BTreeSet<_>>();
    if sources.len() <= 1 {
        return build_metric_rollup(
            records,
            None,
            Some("Daily trend for the requested metric".to_string()),
            bucket_limit,
        )
        .into_iter()
        .collect();
    }

    let mut grouped = Vec::new();
    for source in sources {
        let source_records = records
            .iter()
            .filter(|record| record.record.source == source)
            .cloned()
            .collect::<Vec<_>>();
        if let Some(rollup) = build_metric_rollup(
            &source_records,
            Some(source.clone()),
            Some(format!("Daily trend for source `{source}`")),
            bucket_limit,
        ) {
            grouped.push(rollup);
        }
    }
    sort_context_metric_rollups(&mut grouped);
    grouped
}

fn metric_rollups_for_window(
    records: &[MetricRecordEntry],
    rollup_limit: usize,
    bucket_limit: usize,
    reason: Option<String>,
) -> Vec<ContextMetricRollup> {
    if records.is_empty() || rollup_limit == 0 || bucket_limit == 0 {
        return Vec::new();
    }

    let mut grouped = HashMap::<(String, String), Vec<MetricRecordEntry>>::new();
    for record in records.iter().cloned() {
        grouped
            .entry((record.record.key.clone(), record.record.source.clone()))
            .or_default()
            .push(record);
    }

    let mut rollups = grouped
        .into_iter()
        .filter_map(|((_key, source), group)| {
            build_metric_rollup(&group, Some(source), reason.clone(), bucket_limit)
        })
        .collect::<Vec<_>>();
    sort_context_metric_rollups(&mut rollups);
    if rollups.len() > rollup_limit {
        rollups.truncate(rollup_limit);
    }
    rollups
}

fn build_metric_rollup(
    records: &[MetricRecordEntry],
    source: Option<String>,
    reason: Option<String>,
    bucket_limit: usize,
) -> Option<ContextMetricRollup> {
    if records.is_empty() || bucket_limit == 0 {
        return None;
    }

    let key = records.first()?.record.key.clone();
    if records.iter().any(|record| record.record.key != key) {
        return None;
    }

    let units = records
        .iter()
        .map(|record| record.record.unit.clone())
        .collect::<BTreeSet<_>>();
    if units.len() > 1 {
        return None;
    }

    let unit = records
        .first()
        .and_then(|record| record.record.unit.clone());
    let mut totals = BTreeMap::new();
    for record in records {
        let bucket_date = record
            .record
            .date
            .unwrap_or_else(|| record.record.ts.date_naive());
        let entry = totals.entry(bucket_date).or_insert((0.0_f64, 0_usize));
        entry.0 += record.record.value;
        entry.1 += 1;
    }

    let active_day_count = totals.len();
    let mut buckets = totals
        .into_iter()
        .map(|(date, (value, record_count))| ContextMetricRollupBucket {
            date,
            value,
            record_count,
        })
        .collect::<Vec<_>>();
    buckets.sort_by(|left, right| right.date.cmp(&left.date));
    if buckets.len() > bucket_limit {
        buckets.truncate(bucket_limit);
    }

    Some(ContextMetricRollup {
        key,
        source,
        unit,
        active_day_count,
        matching_record_count: records.len(),
        reason,
        buckets,
    })
}

fn sort_context_metric_rollups(rollups: &mut [ContextMetricRollup]) {
    rollups.sort_by(|left, right| {
        right
            .active_day_count
            .cmp(&left.active_day_count)
            .then_with(|| right.matching_record_count.cmp(&left.matching_record_count))
            .then_with(|| {
                right
                    .buckets
                    .first()
                    .map(|bucket| bucket.date)
                    .cmp(&left.buckets.first().map(|bucket| bucket.date))
            })
            .then_with(|| left.key.cmp(&right.key))
            .then_with(|| left.source.cmp(&right.source))
    });
}

fn metric_reasons_from_evidence(
    evidence: &[MetricRecordEvidence],
) -> std::collections::HashMap<String, String> {
    evidence
        .iter()
        .map(|evidence| {
            (
                evidence.record.record.id.clone(),
                evidence.link_reason.clone(),
            )
        })
        .collect()
}

fn note_day_links(note_id: &str, dates: &BTreeSet<NaiveDate>) -> Vec<ContextLink> {
    dates
        .iter()
        .map(|date| ContextLink {
            kind: ContextLinkKind::Structural,
            from: format!("note:{note_id}"),
            to: format!("day:{date}"),
            reason: format!("Note references day {date}"),
            confidence: None,
        })
        .collect()
}

fn build_day_relationship_links(
    day: &str,
    history_notes: &[ContextNoteItem],
    backlink_notes: &[ContextNoteItem],
    metrics: &[MetricRecordEntry],
) -> Vec<ContextLink> {
    let mut links = history_notes
        .iter()
        .map(|note| ContextLink {
            kind: ContextLinkKind::Structural,
            from: format!("day:{day}"),
            to: format!("note:{}", note.note_id),
            reason: note
                .reason
                .clone()
                .unwrap_or_else(|| "Daily note for requested day".to_string()),
            confidence: None,
        })
        .collect::<Vec<_>>();
    links.extend(backlink_notes.iter().map(|note| {
        ContextLink {
            kind: ContextLinkKind::Structural,
            from: format!("note:{}", note.note_id),
            to: format!("day:{day}"),
            reason: note
                .reason
                .clone()
                .unwrap_or_else(|| "Links to this day".to_string()),
            confidence: None,
        }
    }));
    links.extend(metrics.iter().map(|metric| ContextLink {
        kind: ContextLinkKind::Structural,
        from: format!("day:{day}"),
        to: format!("metric:{}", metric.record.id),
        reason: format!("Metric recorded on {day}"),
        confidence: None,
    }));
    links
}

fn push_pivot(
    pivots: &mut Vec<ContextPivot>,
    seen_commands: &mut HashSet<String>,
    kind: &str,
    target: impl Into<String>,
    command: String,
    reason: impl Into<String>,
) {
    if seen_commands.insert(command.clone()) {
        pivots.push(ContextPivot {
            kind: kind.to_string(),
            target: target.into(),
            command,
            reason: reason.into(),
        });
    }
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '-'))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('"', "\\\""))
    }
}

fn build_note_pivots(
    note_id: &str,
    days: &[String],
    notes: &[ContextNoteItem],
    metrics: &[ContextMetricItem],
) -> Vec<ContextPivot> {
    let mut pivots = Vec::new();
    let mut seen = HashSet::new();
    push_pivot(
        &mut pivots,
        &mut seen,
        "read_note",
        note_id.to_string(),
        format!("arrowhead notes read {}", shell_quote(note_id)),
        "Read the anchor note directly.",
    );
    if let Some(day) = days.first() {
        push_pivot(
            &mut pivots,
            &mut seen,
            "context_day",
            day.clone(),
            format!("arrowhead context day {day}"),
            "Explore the surrounding day context.",
        );
    }
    if let Some(metric) = metrics.first() {
        push_pivot(
            &mut pivots,
            &mut seen,
            "context_metric",
            metric.key.clone(),
            format!("arrowhead context metric {}", shell_quote(&metric.key)),
            "Follow the strongest metric lead.",
        );
    }
    if let Some(note) = notes.first() {
        push_pivot(
            &mut pivots,
            &mut seen,
            "context_note",
            note.note_id.clone(),
            format!("arrowhead context note {}", shell_quote(&note.note_id)),
            "Inspect the strongest related note.",
        );
    }
    pivots
}

fn build_day_pivots(
    day: &str,
    notes: &[ContextNoteItem],
    metrics: &[MetricRecordEntry],
    compare_days: &[String],
) -> Vec<ContextPivot> {
    let mut pivots = Vec::new();
    let mut seen = HashSet::new();
    if let Some(note) = notes
        .iter()
        .find(|note| note.note_id == day)
        .or_else(|| notes.first())
    {
        push_pivot(
            &mut pivots,
            &mut seen,
            "read_note",
            note.note_id.clone(),
            format!("arrowhead notes read {}", shell_quote(&note.note_id)),
            "Read the daily note or strongest note lead for this day.",
        );
    }
    if let Some(metric) = metrics.first() {
        push_pivot(
            &mut pivots,
            &mut seen,
            "context_metric",
            metric.record.key.clone(),
            format!(
                "arrowhead context metric {}",
                shell_quote(&metric.record.key)
            ),
            "Inspect a metric recorded on this day.",
        );
    }
    if let Some(compare_day) = compare_days.first() {
        push_pivot(
            &mut pivots,
            &mut seen,
            "context_day",
            compare_day.clone(),
            format!("arrowhead context day {compare_day}"),
            "Compare this day against the nearest adjacent active day.",
        );
    }
    if let Some(note) = notes.iter().find(|note| note.note_id != day) {
        push_pivot(
            &mut pivots,
            &mut seen,
            "context_note",
            note.note_id.clone(),
            format!("arrowhead context note {}", shell_quote(&note.note_id)),
            "Inspect a note that changed or linked into this day.",
        );
    }
    pivots
}

fn build_metric_pivots(
    target: &str,
    metrics: &[MetricRecordEntry],
    related_notes: &[ContextNoteItem],
    related_days: &[String],
    related_metrics: &[ContextMetricItem],
    metric_rollups: &[ContextMetricRollup],
    range: Option<&str>,
) -> Vec<ContextPivot> {
    let mut pivots = Vec::new();
    let mut seen = HashSet::new();
    push_pivot(
        &mut pivots,
        &mut seen,
        "metrics_read",
        target.to_string(),
        format!("arrowhead metrics read {}", shell_quote(target)),
        "Inspect the exact metric target directly.",
    );
    if let Some(day) = related_days.first() {
        push_pivot(
            &mut pivots,
            &mut seen,
            "context_day",
            day.clone(),
            format!("arrowhead context day {day}"),
            "Explore a day where this metric was active.",
        );
    }
    if let Some(note) = related_notes.first() {
        push_pivot(
            &mut pivots,
            &mut seen,
            "context_note",
            note.note_id.clone(),
            format!("arrowhead context note {}", shell_quote(&note.note_id)),
            "Inspect the strongest related note.",
        );
    }
    if let Some(metric) = related_metrics.first() {
        push_pivot(
            &mut pivots,
            &mut seen,
            "context_metric",
            metric.key.clone(),
            format!("arrowhead context metric {}", shell_quote(&metric.key)),
            "Compare against a nearby co-occurring metric.",
        );
    } else if let Some(metric) = metrics.first() {
        let day = metric
            .record
            .date
            .unwrap_or_else(|| metric.record.ts.date_naive())
            .to_string();
        push_pivot(
            &mut pivots,
            &mut seen,
            "context_day",
            day.clone(),
            format!("arrowhead context day {day}"),
            "Inspect the day containing the latest record.",
        );
    }
    if let Some(rollup) = metric_rollups.first() {
        push_pivot(
            &mut pivots,
            &mut seen,
            "metrics_aggregate",
            rollup.key.clone(),
            metrics_aggregate_command(&rollup.key, rollup.source.as_deref(), range),
            "Inspect the daily trend for this metric.",
        );
    }
    pivots
}

fn build_month_pivots(
    month_range: &str,
    notes: &[ContextNoteItem],
    metrics: &[MetricRecordEntry],
    active_days: &[String],
    metric_rollups: &[ContextMetricRollup],
) -> Vec<ContextPivot> {
    let mut pivots = build_day_pivots(month_range, notes, metrics, active_days);
    let mut seen = pivots
        .iter()
        .map(|pivot| pivot.command.clone())
        .collect::<HashSet<_>>();
    if let Some(rollup) = metric_rollups.first() {
        push_pivot(
            &mut pivots,
            &mut seen,
            "metrics_aggregate",
            rollup.key.clone(),
            metrics_aggregate_command(&rollup.key, rollup.source.as_deref(), Some(month_range)),
            "Inspect the strongest metric trend in this month.",
        );
    }
    pivots
}

fn build_source_pivots(
    source: &str,
    metrics: &[MetricRecordEntry],
    related_notes: &[ContextNoteItem],
    related_days: &[String],
    related_metrics: &[ContextMetricItem],
) -> Vec<ContextPivot> {
    let mut pivots = Vec::new();
    let mut seen = HashSet::new();
    if let Some(metric_key) = related_metrics
        .first()
        .map(|metric| metric.key.clone())
        .or_else(|| metrics.first().map(|record| record.record.key.clone()))
    {
        push_pivot(
            &mut pivots,
            &mut seen,
            "context_metric",
            metric_key.clone(),
            format!("arrowhead context metric {}", shell_quote(&metric_key)),
            format!("Inspect a metric emitted by source {source}."),
        );
    }
    if let Some(day) = related_days.first() {
        push_pivot(
            &mut pivots,
            &mut seen,
            "context_day",
            day.clone(),
            format!("arrowhead context day {day}"),
            "Inspect an active day for this source.",
        );
    }
    if let Some(note) = related_notes.first() {
        push_pivot(
            &mut pivots,
            &mut seen,
            "context_note",
            note.note_id.clone(),
            format!("arrowhead context note {}", shell_quote(&note.note_id)),
            "Inspect the strongest related note for this source.",
        );
    }
    pivots
}

fn metrics_aggregate_command(key: &str, source: Option<&str>, range: Option<&str>) -> String {
    let mut query = format!("key:{key}");
    if let Some(source) = source.map(str::trim).filter(|value| !value.is_empty()) {
        query.push(' ');
        query.push_str("source:");
        query.push_str(source);
    }
    if let Some(range) = range.map(str::trim).filter(|value| !value.is_empty()) {
        query.push(' ');
        query.push_str("date:");
        query.push_str(range);
    }
    format!(
        "arrowhead metrics search {} --aggregate sum",
        shell_quote(&query)
    )
}

fn note_item_from_note_record(note: &NoteRecord, reason: Option<String>) -> ContextNoteItem {
    ContextNoteItem {
        note_id: note.id.clone(),
        title: note.title.clone(),
        relative_path: Some(note.relative_path.clone()),
        file_modified_at: Some(note.file_modified_at),
        created_at: note.created_at,
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
        created_at: None,
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

fn upsert_note_metric_evidence(
    target: &mut HashMap<String, NoteMetricEvidence>,
    incoming: NoteMetricEvidence,
) {
    match target.get_mut(&incoming.note.note_id) {
        Some(existing) => {
            let incoming_is_stronger = incoming.kind < existing.kind
                || (incoming.kind == existing.kind
                    && incoming.note.file_modified_at > existing.note.file_modified_at);
            if incoming_is_stronger {
                *existing = incoming;
            }
        }
        None => {
            target.insert(incoming.note.note_id.clone(), incoming);
        }
    }
}

fn upsert_metric_record_evidence(
    target: &mut HashMap<String, MetricRecordEvidence>,
    incoming: MetricRecordEvidence,
) {
    match target.get_mut(&incoming.record.record.id) {
        Some(existing) => {
            let incoming_is_stronger = incoming.kind < existing.kind
                || (incoming.kind == existing.kind
                    && incoming.record.record.ts > existing.record.record.ts);
            if incoming_is_stronger {
                *existing = incoming;
            }
        }
        None => {
            target.insert(incoming.record.record.id.clone(), incoming);
        }
    }
}

fn sort_note_metric_evidence(evidence: &mut [NoteMetricEvidence]) {
    evidence.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| right.note.file_modified_at.cmp(&left.note.file_modified_at))
            .then_with(|| left.note.note_id.cmp(&right.note.note_id))
    });
}

fn sort_metric_record_evidence(evidence: &mut [MetricRecordEvidence]) {
    evidence.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| right.record.record.ts.cmp(&left.record.record.ts))
            .then_with(|| left.record.record.key.cmp(&right.record.record.key))
            .then_with(|| left.record.record.id.cmp(&right.record.record.id))
    });
}

fn context_evidence_link_kind(kind: ContextEvidenceKind) -> ContextLinkKind {
    match kind {
        ContextEvidenceKind::Explicit => ContextLinkKind::Explicit,
        ContextEvidenceKind::Structural => ContextLinkKind::Structural,
        ContextEvidenceKind::Inferred => ContextLinkKind::Related,
    }
}

fn dedup_context_links(links: &mut Vec<ContextLink>) {
    let mut deduped = Vec::with_capacity(links.len());
    let mut seen = HashSet::new();
    for link in links.drain(..) {
        let key = (
            link.kind,
            link.from.clone(),
            link.to.clone(),
            link.reason.clone(),
        );
        if seen.insert(key) {
            deduped.push(link);
        }
    }
    deduped.sort_by(|left, right| {
        left.from
            .cmp(&right.from)
            .then_with(|| left.to.cmp(&right.to))
            .then_with(|| left.reason.cmp(&right.reason))
    });
    *links = deduped;
}

fn trim_context_links(links: &mut Vec<ContextLink>, limit: usize) {
    if links.len() > limit {
        links.truncate(limit);
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

    let metadata_json = serde_json::to_string(&note.metadata).unwrap_or_default();
    let haystack = format!(
        "{}\n{}\n{}\n{}",
        note.id,
        note.title.clone().unwrap_or_default(),
        note.content,
        metadata_json
    );
    for matched in ABSOLUTE_DATE_RE.find_iter(&haystack) {
        if let Ok(date) = NaiveDate::parse_from_str(matched.as_str(), "%Y-%m-%d") {
            dates.insert(date);
        }
    }

    dates
}

fn extract_metric_key_mentions(note: &NoteRecord) -> BTreeSet<String> {
    let metadata_json = serde_json::to_string(&note.metadata).unwrap_or_default();
    let haystack = format!(
        "{}\n{}\n{}\n{}",
        note.id,
        note.title.clone().unwrap_or_default(),
        note.content,
        metadata_json
    );
    METRIC_KEY_TOKEN_RE
        .find_iter(&haystack)
        .map(|matched| matched.as_str().to_ascii_lowercase())
        .collect()
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

fn is_context_noise_search_result(result: &SearchResult) -> bool {
    result
        .relative_path
        .as_deref()
        .map(Path::new)
        .is_some_and(is_context_noise_path)
        || is_context_noise_note_id(&result.note_id)
}

fn is_context_noise_note_item(note: &ContextNoteItem) -> bool {
    note.relative_path
        .as_deref()
        .is_some_and(is_context_noise_path)
        || is_context_noise_note_id(&note.note_id)
}

fn is_context_noise_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_context_noise_filename)
}

fn is_context_noise_filename(file_name: &str) -> bool {
    ["AGENTS.md", "CLAUDE.md", "ARROWHEAD.md"]
        .iter()
        .any(|candidate| file_name.eq_ignore_ascii_case(candidate))
}

fn is_context_noise_note_id(note_id: &str) -> bool {
    ["AGENTS", "CLAUDE", "ARROWHEAD"]
        .iter()
        .any(|candidate| note_id.eq_ignore_ascii_case(candidate.trim()))
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
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn ts(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("valid timestamp")
    }

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
                Some(ts(2026, 4, 13, 8, 0)),
                ts(2026, 4, 14, 9, 30),
            ),
            build_note(
                vault,
                "2026-04-14",
                Some("2026-04-14"),
                "Daily note for body.weight updates.",
                Some(ts(2026, 4, 14, 7, 0)),
                ts(2026, 4, 14, 21, 0),
            ),
            build_note(
                vault,
                "Related Note",
                Some("Related Note"),
                "See [[Project Hub]] for the latest withings import.",
                Some(ts(2026, 4, 14, 12, 0)),
                ts(2026, 4, 14, 12, 30),
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

    fn build_note(
        vault: &Vault,
        note_id: &str,
        title: Option<&str>,
        body: &str,
        created_at: Option<DateTime<Utc>>,
        file_modified_at: DateTime<Utc>,
    ) -> NoteRecord {
        let mut metadata = MetadataMap::default();
        if let Some(title) = title {
            metadata.insert("title".to_string(), Value::String(title.to_string()));
        }
        vault
            .write_note(note_id, &metadata, body)
            .expect("write note");
        let mut note = vault.load_note(note_id).expect("load note");
        note.created_at = created_at;
        note.file_modified_at = file_modified_at;
        note
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

    fn insert_note_fixture(
        service: &ContextService,
        note_id: &str,
        title: Option<&str>,
        body: &str,
        created_at: Option<DateTime<Utc>>,
        file_modified_at: DateTime<Utc>,
    ) {
        let note = build_note(
            service.vault.as_ref(),
            note_id,
            title,
            body,
            created_at,
            file_modified_at,
        );
        let extraction = MetadataExtractor::new()
            .extract(&note)
            .expect("extract metadata");
        let mut note_ids = service
            .vault
            .inventory_snapshot()
            .expect("snapshot")
            .entries()
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<HashSet<_>>();
        note_ids.insert(note.id.clone());
        let resolved_links = make_resolved_links(&extraction, &note_ids);
        service
            .database
            .upsert_note(&note, &extraction, &resolved_links, Utc::now())
            .expect("upsert fixture note");
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
    async fn note_context_uses_structural_and_inferred_metric_evidence() {
        let (_dir, service) = build_service();
        insert_note_fixture(
            &service,
            "Weight Review",
            Some("Weight Review"),
            "Compare [[2026-04-15]] against the latest body.weight trend.",
            Some(ts(2026, 4, 16, 7, 0)),
            ts(2026, 4, 16, 7, 30),
        );

        let payload = service
            .note("Weight Review", Some(5), Some(5))
            .await
            .expect("note context");

        assert!(
            payload.related.days.iter().any(|day| day == "2026-04-15"),
            "expected literal date mention to surface as a related day"
        );
        assert!(
            payload
                .activity
                .metrics
                .iter()
                .any(|metric| metric.record.id == "01AAB"),
            "expected same-day metric evidence from the referenced day"
        );
        assert!(
            payload.links.items.iter().any(|link| {
                link.from == "note:Weight Review"
                    && link.to == "day:2026-04-15"
                    && link.kind == ContextLinkKind::Structural
            }),
            "expected note-to-day structural link"
        );
        assert!(
            payload.links.items.iter().any(|link| {
                link.from == "note:Weight Review"
                    && link.to == "metric:01AAB"
                    && link.kind == ContextLinkKind::Structural
            }),
            "expected same-day metric evidence to classify structurally"
        );
    }

    #[tokio::test]
    async fn note_context_surfaces_inferred_metric_key_evidence() {
        let (_dir, service) = build_service();
        insert_note_fixture(
            &service,
            "Weight Lexicon",
            Some("Weight Lexicon"),
            "body.weight is the main trend I want to keep an eye on.",
            Some(ts(2026, 4, 16, 8, 0)),
            ts(2026, 4, 16, 8, 30),
        );

        let payload = service
            .note("Weight Lexicon", Some(5), Some(5))
            .await
            .expect("note context");

        assert!(
            payload
                .activity
                .metrics
                .iter()
                .any(|metric| metric.record.key == "body.weight"),
            "expected inferred metric-key evidence to surface matching records"
        );
        assert!(
            payload.links.items.iter().any(|link| {
                link.from == "note:Weight Lexicon"
                    && link.to.starts_with("metric:")
                    && link.kind == ContextLinkKind::Related
                    && link.confidence.is_some()
            }),
            "expected inferred note-to-metric links to stay marked as related"
        );
    }

    #[tokio::test]
    async fn note_context_deduplicates_metric_leads_by_key() {
        let (_dir, service) = build_service();
        insert_note_fixture(
            &service,
            "Weight Summary",
            Some("Weight Summary"),
            "Compare body.weight between 2026-04-14 and 2026-04-15.",
            Some(ts(2026, 4, 16, 9, 0)),
            ts(2026, 4, 16, 9, 15),
        );

        let payload = service
            .note("Weight Summary", Some(5), Some(5))
            .await
            .expect("note context");

        let body_weight_count = payload
            .related
            .metrics
            .iter()
            .filter(|metric| metric.key == "body.weight")
            .count();
        assert_eq!(
            body_weight_count, 1,
            "expected note metric leads to collapse duplicate keys"
        );
        assert!(
            payload.activity.metrics.len() >= 2,
            "expected raw metric activity to keep the underlying records"
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
        assert!(
            payload
                .related
                .metric_rollups
                .iter()
                .any(|rollup| rollup.key == "body.weight" && rollup.active_day_count == 2),
            "expected daily metric rollup for the target key"
        );
    }

    #[tokio::test]
    async fn metric_context_prefers_explicit_and_structural_note_evidence() {
        let (_dir, service) = build_service();
        insert_note_fixture(
            &service,
            "Weight Lexicon",
            Some("Weight Lexicon"),
            "body.weight is one of the health metrics tracked in this vault.",
            Some(ts(2026, 4, 16, 8, 0)),
            ts(2026, 4, 16, 8, 30),
        );

        let payload = service
            .metric("body.weight", None, Some(6), Some(5))
            .await
            .expect("metric context");

        let note_ids = payload
            .related
            .notes
            .iter()
            .map(|note| note.note_id.as_str())
            .collect::<Vec<_>>();
        let project_position = note_ids
            .iter()
            .position(|note_id| *note_id == "Project Hub")
            .expect("explicit note present");
        let daily_note_position = note_ids
            .iter()
            .position(|note_id| *note_id == "2026-04-14")
            .expect("same-day note present");
        let glossary_position = note_ids
            .iter()
            .position(|note_id| *note_id == "Weight Lexicon")
            .expect("inferred note present");

        assert!(
            project_position < glossary_position,
            "expected explicit note evidence to outrank inferred text matches"
        );
        assert!(
            daily_note_position < glossary_position,
            "expected structural same-day note evidence to outrank inferred text matches"
        );
        assert_eq!(
            payload
                .links
                .items
                .iter()
                .find(|link| link.from == "note:Project Hub" && link.to == "body.weight")
                .map(|link| link.kind),
            Some(ContextLinkKind::Explicit),
            "expected explicit note links to be classified explicitly"
        );
    }

    #[tokio::test]
    async fn metric_context_applies_date_range_filter() {
        let (_dir, service) = build_service();
        let payload = service
            .metric("body.weight", Some("2026-04-15"), Some(5), Some(5))
            .await
            .expect("metric context");

        assert_eq!(payload.summary.kind, ContextTargetKind::Metric);
        assert_eq!(payload.summary.target, "body.weight");
        assert_eq!(
            payload.summary.label.as_deref(),
            Some("body.weight (1 record)")
        );
        assert_eq!(payload.activity.metrics.len(), 1);
        assert_eq!(
            payload.activity.metrics[0].record.date,
            Some(NaiveDate::from_ymd_opt(2026, 4, 15).expect("date"))
        );
    }

    #[tokio::test]
    async fn metric_context_filters_convention_file_text_matches() {
        let (_dir, service) = build_service();
        insert_note_fixture(
            &service,
            "AGENTS",
            Some("AGENTS.md - Toto's Vault Operating Manual"),
            "Known metric keys include body.weight and nutrition.energy_intake.",
            Some(Utc::now() - chrono::Duration::hours(2)),
            Utc::now() - chrono::Duration::hours(1),
        );
        insert_note_fixture(
            &service,
            "CLAUDE",
            Some("CLAUDE.md - Toto's Vault Operating Manual"),
            "Schema reference table: body.weight, nutrition.energy_intake, body.fat_pct.",
            Some(Utc::now() - chrono::Duration::hours(2)),
            Utc::now() - chrono::Duration::hours(1),
        );

        let payload = service
            .metric("body.weight", None, Some(5), Some(5))
            .await
            .expect("metric context");

        assert!(
            payload
                .related
                .notes
                .iter()
                .all(|note| note.note_id != "AGENTS" && note.note_id != "CLAUDE"),
            "expected convention files to be filtered from related notes"
        );
        assert!(
            payload
                .related
                .notes
                .iter()
                .any(|note| note.note_id == "Project Hub"),
            "expected contextual note matches to remain"
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

    #[tokio::test]
    async fn source_context_prefers_explicit_and_structural_note_evidence() {
        let (_dir, service) = build_service();
        insert_note_fixture(
            &service,
            "Weight Lexicon",
            Some("Weight Lexicon"),
            "body.weight is one of the health metrics tracked in this vault.",
            Some(ts(2026, 4, 16, 8, 0)),
            ts(2026, 4, 16, 8, 30),
        );

        let payload = service
            .source("withings", None, Some(6), Some(5))
            .await
            .expect("source context");

        let note_ids = payload
            .related
            .notes
            .iter()
            .map(|note| note.note_id.as_str())
            .collect::<Vec<_>>();
        let explicit_position = note_ids
            .iter()
            .position(|note_id| *note_id == "Related Note")
            .expect("explicit source note present");
        let daily_note_position = note_ids
            .iter()
            .position(|note_id| *note_id == "2026-04-14")
            .expect("same-day note present");
        let inferred_position = note_ids
            .iter()
            .position(|note_id| *note_id == "Weight Lexicon")
            .expect("inferred note present");

        assert!(
            explicit_position < inferred_position,
            "expected explicit source mentions to outrank inferred metric-key notes"
        );
        assert!(
            daily_note_position < inferred_position,
            "expected same-day source notes to outrank inferred metric-key notes"
        );
        assert_eq!(
            payload
                .links
                .items
                .iter()
                .find(|link| link.from == "note:Related Note" && link.to == "source:withings")
                .map(|link| link.kind),
            Some(ContextLinkKind::Explicit),
            "expected explicit source note links to be classified explicitly"
        );
    }

    #[tokio::test]
    async fn day_context_surfaces_day_metrics_and_adjacent_days() {
        let (_dir, service) = build_service();
        let payload = service
            .day("2026-04-14", Some(5), Some(5))
            .await
            .expect("day context");

        assert_eq!(payload.summary.kind, ContextTargetKind::Day);
        assert_eq!(payload.summary.target, "2026-04-14");
        assert_eq!(payload.activity.metrics.len(), 1);
        assert!(
            !payload.activity.notes_created.is_empty(),
            "expected created notes for requested day"
        );
        assert!(
            !payload.activity.notes_updated.is_empty(),
            "expected updated notes for requested day"
        );
        assert!(
            !payload.pivots.is_empty(),
            "expected concrete follow-up pivots"
        );
        assert!(
            payload.related.days.iter().any(|day| day == "2026-04-15"),
            "expected adjacent active day"
        );
        assert!(
            payload.links.items.iter().any(|link| {
                link.from == "day:2026-04-14"
                    && link.to == "metric:01AAA"
                    && link.kind == ContextLinkKind::Structural
            }),
            "expected day-to-metric structural relationships in day context links"
        );
        assert!(
            payload.links.items.iter().any(|link| {
                link.from == "note:Project Hub"
                    && link.to == "day:2026-04-14"
                    && link.kind == ContextLinkKind::Structural
            }),
            "expected backlinks into the day to be represented as relationships"
        );
    }

    #[tokio::test]
    async fn week_context_accepts_anchor_day() {
        let (_dir, service) = build_service();
        let payload = service
            .week(
                WeekContextSelector::ContainingDay(
                    NaiveDate::from_ymd_opt(2026, 4, 14).expect("date"),
                ),
                Some(5),
                Some(5),
            )
            .await
            .expect("week context");

        assert_eq!(payload.summary.kind, ContextTargetKind::Week);
        assert_eq!(payload.summary.target, "2026-04-13..2026-04-19");
        assert!(
            payload
                .related
                .days
                .iter()
                .any(|day| day == "2026-04-14" || day == "2026-04-15"),
            "expected active days inside requested week"
        );
    }

    #[tokio::test]
    async fn month_context_accepts_anchor_day() {
        let (_dir, service) = build_service();
        let payload = service
            .month(
                MonthContextSelector::ContainingDay(
                    NaiveDate::from_ymd_opt(2026, 4, 14).expect("date"),
                ),
                Some(5),
                Some(5),
            )
            .await
            .expect("month context");

        assert_eq!(payload.summary.kind, ContextTargetKind::Month);
        assert_eq!(payload.summary.target, "2026-04-01..2026-04-30");
        assert_eq!(payload.summary.label.as_deref(), Some("April 2026"));
        assert!(
            payload
                .related
                .days
                .iter()
                .any(|day| day == "2026-04-14" || day == "2026-04-15"),
            "expected active days inside requested month"
        );
        assert!(
            payload
                .related
                .metric_rollups
                .iter()
                .any(|rollup| rollup.key == "body.weight"
                    && rollup.source.as_deref() == Some("withings")),
            "expected month context to surface source-specific metric trends"
        );
        assert!(
            payload
                .pivots
                .iter()
                .any(|pivot| pivot.kind == "metrics_aggregate"),
            "expected month context to suggest a trend-following aggregate pivot"
        );
    }

    #[tokio::test]
    async fn month_context_filters_convention_file_activity_noise() {
        let (_dir, service) = build_service();
        insert_note_fixture(
            &service,
            "AGENTS",
            Some("AGENTS.md - Toto's Vault Operating Manual"),
            "Operating instructions for the vault.",
            Some(ts(2026, 4, 14, 18, 19)),
            ts(2026, 4, 14, 18, 20),
        );

        let payload = service
            .month(
                MonthContextSelector::ContainingDay(
                    NaiveDate::from_ymd_opt(2026, 4, 14).expect("date"),
                ),
                Some(10),
                Some(5),
            )
            .await
            .expect("month context");

        assert!(
            payload
                .activity
                .notes_created
                .iter()
                .all(|note| note.note_id != "AGENTS"),
            "expected convention-file noise to be filtered from month note activity"
        );
    }

    #[tokio::test]
    async fn changed_context_surfaces_recent_note_activity() {
        let (_dir, service) = build_service();
        let payload = service
            .changed(3, Some(5), Some(5))
            .await
            .expect("changed context");

        assert_eq!(payload.summary.kind, ContextTargetKind::Changed);
        assert!(
            !payload.activity.notes.is_empty(),
            "expected recent note activity in changed context"
        );
    }

    #[tokio::test]
    async fn changed_context_skips_convention_file_link_noise() {
        let (_dir, service) = build_service();
        insert_note_fixture(
            &service,
            "AGENTS",
            Some("AGENTS.md - Toto's Vault Operating Manual"),
            "Examples: [[YYYY-MM-DD]] and [[wikilinks]] for agents.",
            Some(Utc::now() - chrono::Duration::hours(3)),
            Utc::now() - chrono::Duration::hours(1),
        );
        insert_note_fixture(
            &service,
            "CLAUDE",
            Some("CLAUDE.md - Toto's Vault Operating Manual"),
            "Examples: [[YYYY-MM-DD]] and [[wikilinks]] for Claude templates.",
            Some(Utc::now() - chrono::Duration::hours(3)),
            Utc::now() - chrono::Duration::hours(1),
        );

        let payload = service.changed(3, Some(5), Some(5)).await.expect("changed");

        assert!(
            payload.activity.links.iter().all(|link| {
                !link.from.starts_with("note:AGENTS") && !link.from.starts_with("note:CLAUDE")
            }),
            "expected convention-file wikilink examples to be filtered from changed activity"
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

    #[test]
    fn context_noise_filters_match_agent_instruction_files() {
        assert!(is_context_noise_filename("AGENTS.md"));
        assert!(is_context_noise_filename("claude.md"));
        assert!(is_context_noise_note_id("ARROWHEAD"));
        assert!(!is_context_noise_filename("Project Hub.md"));
        assert!(!is_context_noise_note_id("Project Hub"));
    }
}
