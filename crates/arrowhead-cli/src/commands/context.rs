//! `arrowhead context` command family.

use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{Duration, NaiveDate, Utc};
use clap::{Args, Subcommand};
use serde_json::to_string_pretty;
use tracing::warn;

use super::CommandContext;
use crate::logging;
use arrowhead_core::{
    ContextAttentionItem, ContextEvidenceKind, ContextLink, ContextMetricItem, ContextMetricRollup,
    ContextPayload, ContextPivot, ContextService, ContextTargetKind, DEFAULT_CONTEXT_METRIC_LIMIT,
    DEFAULT_CONTEXT_NOTE_LIMIT, MonthContextSelector, SearchConfig, SearchService, Vault,
    VaultConfig, WeekContextSelector, embeddings::EmbeddingPipeline, sqlite::IndexDatabase,
};

/// Controls whether note-context flows should try to load embeddings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemanticContextMode {
    /// Do not initialise embeddings.
    Disabled,
    /// Try to initialise embeddings, but keep going if they are unavailable.
    Preferred,
}

/// Context retrieval commands spanning notes, metrics, and sources.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct ContextCommand {
    /// Select which context target to inspect.
    #[command(subcommand)]
    pub action: ContextAction,
}

/// Available `arrowhead context` subcommands.
#[derive(Debug, Subcommand, Clone, PartialEq)]
pub enum ContextAction {
    /// Show context for a specific day.
    Day(DayArgs),
    /// Show context for a calendar week.
    Week(WeekArgs),
    /// Show context for a calendar month.
    Month(MonthArgs),
    /// Show recently changed notes and metrics.
    Changed(ChangedArgs),
    /// Show context around a note.
    Note(NoteArgs),
    /// Show context around a metric id or key.
    Metric(MetricArgs),
    /// Show context around a metrics source.
    Source(SourceArgs),
}

/// Arguments for `arrowhead context day`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct DayArgs {
    /// Optional day to inspect in YYYY-MM-DD format.
    #[arg(conflicts_with_all = ["this", "last"])]
    pub day: Option<String>,
    /// Inspect today.
    #[arg(long, conflicts_with = "last")]
    pub this: bool,
    /// Inspect yesterday.
    #[arg(long, conflicts_with = "this")]
    pub last: bool,
    /// Maximum number of related notes to surface.
    #[arg(long, default_value_t = DEFAULT_CONTEXT_NOTE_LIMIT)]
    pub note_limit: usize,
    /// Maximum number of metric records to surface.
    #[arg(long, default_value_t = DEFAULT_CONTEXT_METRIC_LIMIT)]
    pub metric_limit: usize,
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `arrowhead context week`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct WeekArgs {
    /// Optional day inside the week to inspect in YYYY-MM-DD format.
    #[arg(conflicts_with_all = ["this", "last"])]
    pub day: Option<String>,
    /// Inspect the current week.
    #[arg(long, conflicts_with = "last")]
    pub this: bool,
    /// Inspect the previous week.
    #[arg(long, conflicts_with = "this")]
    pub last: bool,
    /// Maximum number of related notes to surface.
    #[arg(long, default_value_t = DEFAULT_CONTEXT_NOTE_LIMIT)]
    pub note_limit: usize,
    /// Maximum number of metric records to surface.
    #[arg(long, default_value_t = DEFAULT_CONTEXT_METRIC_LIMIT)]
    pub metric_limit: usize,
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `arrowhead context month`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct MonthArgs {
    /// Optional month (`YYYY-MM`) or day inside the month (`YYYY-MM-DD`) to inspect.
    #[arg(conflicts_with_all = ["this", "last"])]
    pub month: Option<String>,
    /// Inspect the current month.
    #[arg(long, conflicts_with = "last")]
    pub this: bool,
    /// Inspect the previous month.
    #[arg(long, conflicts_with = "this")]
    pub last: bool,
    /// Maximum number of related notes to surface.
    #[arg(long, default_value_t = DEFAULT_CONTEXT_NOTE_LIMIT)]
    pub note_limit: usize,
    /// Maximum number of metric records to surface.
    #[arg(long, default_value_t = DEFAULT_CONTEXT_METRIC_LIMIT)]
    pub metric_limit: usize,
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `arrowhead context changed`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct ChangedArgs {
    /// Number of trailing days to inspect.
    #[arg(long, default_value_t = 7)]
    pub days: usize,
    /// Maximum number of related notes to surface.
    #[arg(long, default_value_t = DEFAULT_CONTEXT_NOTE_LIMIT)]
    pub note_limit: usize,
    /// Maximum number of metric records to surface.
    #[arg(long, default_value_t = DEFAULT_CONTEXT_METRIC_LIMIT)]
    pub metric_limit: usize,
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `arrowhead context note`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct NoteArgs {
    /// Note identifier.
    pub note_id: String,
    /// Maximum number of related notes to surface.
    #[arg(long, default_value_t = DEFAULT_CONTEXT_NOTE_LIMIT)]
    pub note_limit: usize,
    /// Maximum number of metric records to surface.
    #[arg(long, default_value_t = DEFAULT_CONTEXT_METRIC_LIMIT)]
    pub metric_limit: usize,
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `arrowhead context metric`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct MetricArgs {
    /// Metric id (`metric:<id>` or raw id) or metric key.
    pub metric: String,
    /// Optional metrics date range such as `past30d`, `2026-04`, or `2026-04-01..2026-04-15`.
    #[arg(long)]
    pub range: Option<String>,
    /// Maximum number of related notes to surface.
    #[arg(long, default_value_t = DEFAULT_CONTEXT_NOTE_LIMIT)]
    pub note_limit: usize,
    /// Maximum number of metric records to surface.
    #[arg(long, default_value_t = DEFAULT_CONTEXT_METRIC_LIMIT)]
    pub metric_limit: usize,
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `arrowhead context source`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct SourceArgs {
    /// Metrics source identifier.
    pub source: String,
    /// Optional metrics date range such as `past30d`, `2026-04`, or `2026-04-01..2026-04-15`.
    #[arg(long)]
    pub range: Option<String>,
    /// Maximum number of related notes to surface.
    #[arg(long, default_value_t = DEFAULT_CONTEXT_NOTE_LIMIT)]
    pub note_limit: usize,
    /// Maximum number of metric records to surface.
    #[arg(long, default_value_t = DEFAULT_CONTEXT_METRIC_LIMIT)]
    pub metric_limit: usize,
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Execute the requested context command.
pub async fn run(ctx: &CommandContext, command: &ContextCommand) -> Result<()> {
    let vault_path = ctx
        .config
        .vault
        .clone()
        .context("no vault configured. Provide --vault or run `arrowhead init`.")?;

    let vault = Arc::new(Vault::new(VaultConfig::new(vault_path))?);
    vault.ensure_arrowhead_dirs()?;

    let logs_dir = vault.paths().logs_dir();
    let _logging_guard = logging::scoped_file_logging(&logs_dir, ctx.verbosity())?;

    let database = Arc::new(IndexDatabase::open(
        vault.paths().arrowhead_dir.join("index.db"),
    )?);
    let semantic_mode = match &command.action {
        ContextAction::Note(_) => SemanticContextMode::Preferred,
        ContextAction::Day(_)
        | ContextAction::Week(_)
        | ContextAction::Month(_)
        | ContextAction::Changed(_)
        | ContextAction::Metric(_)
        | ContextAction::Source(_) => SemanticContextMode::Disabled,
    };
    let service = build_context_service(ctx, Arc::clone(&vault), database, semantic_mode).await?;

    let payload = match &command.action {
        ContextAction::Day(args) => {
            let day = resolve_day(args)?.format("%Y-%m-%d").to_string();
            service
                .day(&day, Some(args.note_limit), Some(args.metric_limit))
                .await?
        }
        ContextAction::Week(args) => {
            service
                .week(
                    resolve_week_selector(args)?,
                    Some(args.note_limit),
                    Some(args.metric_limit),
                )
                .await?
        }
        ContextAction::Month(args) => {
            service
                .month(
                    resolve_month_selector(args)?,
                    Some(args.note_limit),
                    Some(args.metric_limit),
                )
                .await?
        }
        ContextAction::Changed(args) => {
            service
                .changed(args.days, Some(args.note_limit), Some(args.metric_limit))
                .await?
        }
        ContextAction::Note(args) => {
            service
                .note(
                    &args.note_id,
                    Some(args.note_limit),
                    Some(args.metric_limit),
                )
                .await?
        }
        ContextAction::Metric(args) => {
            service
                .metric(
                    &args.metric,
                    args.range.as_deref(),
                    Some(args.note_limit),
                    Some(args.metric_limit),
                )
                .await?
        }
        ContextAction::Source(args) => {
            service
                .source(
                    &args.source,
                    args.range.as_deref(),
                    Some(args.note_limit),
                    Some(args.metric_limit),
                )
                .await?
        }
    };

    let json = match &command.action {
        ContextAction::Day(args) => args.json,
        ContextAction::Week(args) => args.json,
        ContextAction::Month(args) => args.json,
        ContextAction::Changed(args) => args.json,
        ContextAction::Note(args) => args.json,
        ContextAction::Metric(args) => args.json,
        ContextAction::Source(args) => args.json,
    };

    let metric_range = match &command.action {
        ContextAction::Metric(args) => normalized_metric_range(args.range.as_deref()),
        _ => None,
    };

    if json {
        println!("{}", to_string_pretty(&payload)?);
    } else if let ContextAction::Metric(_) = &command.action {
        render_metric_context_with_range(&payload, metric_range);
    } else {
        render_context_payload(&payload);
    }

    Ok(())
}

pub(crate) async fn build_context_service(
    ctx: &CommandContext,
    vault: Arc<Vault>,
    database: Arc<IndexDatabase>,
    semantic_mode: SemanticContextMode,
) -> Result<ContextService> {
    let embeddings = match semantic_mode {
        SemanticContextMode::Disabled => None,
        SemanticContextMode::Preferred => {
            let selected_model = ctx
                .config
                .embedding_model
                .clone()
                .unwrap_or_else(|| "fast".to_string());
            match EmbeddingPipeline::initialise(
                vault.as_ref(),
                Arc::clone(&database),
                &selected_model,
            )
            .await
            {
                Ok(pipeline) => Some(Arc::new(pipeline)),
                Err(err) => {
                    warn!(
                        error = %err,
                        model = %selected_model,
                        "semantic context unavailable; continuing without embeddings"
                    );
                    None
                }
            }
        }
    };
    let search = SearchService::new(Arc::clone(&database), SearchConfig::default(), embeddings);
    Ok(ContextService::new(vault, database, search))
}

pub(crate) fn render_context_payload(payload: &ContextPayload) {
    match payload.summary.kind {
        ContextTargetKind::Metric => render_metric_context(payload),
        _ => {
            println!(
                "Context: {} {}",
                render_target_kind(payload.summary.kind),
                payload.summary.target
            );
            if let Some(label) = payload
                .summary
                .label
                .as_deref()
                .filter(|label| *label != payload.summary.target)
            {
                println!("{label}");
            }

            match payload.summary.kind {
                ContextTargetKind::Day => render_day_context(payload),
                ContextTargetKind::Week | ContextTargetKind::Month | ContextTargetKind::Changed => {
                    render_window_context(payload)
                }
                ContextTargetKind::Note => render_note_context(payload),
                ContextTargetKind::Source => render_source_context(payload),
                ContextTargetKind::Metric => unreachable!("metric contexts are handled above"),
            }
        }
    }

    render_attention(payload);
    render_next_pivots(&payload.pivots);
}

fn render_target_kind(kind: ContextTargetKind) -> &'static str {
    match kind {
        ContextTargetKind::Day => "day",
        ContextTargetKind::Week => "week",
        ContextTargetKind::Month => "month",
        ContextTargetKind::Changed => "changed",
        ContextTargetKind::Note => "note",
        ContextTargetKind::Metric => "metric",
        ContextTargetKind::Source => "source",
    }
}

fn resolve_day(args: &DayArgs) -> Result<NaiveDate> {
    resolve_day_with_today(args, Utc::now().date_naive())
}

fn resolve_day_with_today(args: &DayArgs, today: NaiveDate) -> Result<NaiveDate> {
    if args.last {
        return Ok(today - Duration::days(1));
    }
    if let Some(day) = args.day.as_deref() {
        let parsed = NaiveDate::parse_from_str(day.trim(), "%Y-%m-%d")
            .with_context(|| format!("invalid day `{}`", day.trim()))?;
        return Ok(parsed);
    }
    Ok(today)
}

fn resolve_week_selector(args: &WeekArgs) -> Result<WeekContextSelector> {
    if args.last {
        return Ok(WeekContextSelector::LastWeek);
    }
    if let Some(day) = args.day.as_deref() {
        let parsed = NaiveDate::parse_from_str(day.trim(), "%Y-%m-%d")
            .with_context(|| format!("invalid week day `{}`", day.trim()))?;
        return Ok(WeekContextSelector::ContainingDay(parsed));
    }
    Ok(WeekContextSelector::ThisWeek)
}

fn resolve_month_selector(args: &MonthArgs) -> Result<MonthContextSelector> {
    if args.last {
        return Ok(MonthContextSelector::LastMonth);
    }
    if let Some(month) = args.month.as_deref() {
        let parsed = parse_month_selector_day(month)?;
        return Ok(MonthContextSelector::ContainingDay(parsed));
    }
    Ok(MonthContextSelector::ThisMonth)
}

fn parse_month_selector_day(input: &str) -> Result<NaiveDate> {
    let trimmed = input.trim();
    if let Ok(parsed) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        return Ok(parsed);
    }

    NaiveDate::parse_from_str(&format!("{trimmed}-01"), "%Y-%m-%d")
        .with_context(|| format!("invalid month `{trimmed}`"))
}

fn render_day_context(payload: &ContextPayload) {
    if !payload.history.notes.is_empty() {
        println!("\nDaily Note");
        for note in &payload.history.notes {
            print_note_line(note);
        }
    }

    if !payload.activity.notes_created.is_empty() {
        println!("\nNotes Added");
        for note in &payload.activity.notes_created {
            print_note_line(note);
            print_optional_timestamp("file created", note.created_at);
        }
    }

    if !payload.activity.notes_updated.is_empty() {
        println!("\nNotes Updated");
        for note in &payload.activity.notes_updated {
            print_note_line(note);
            print_optional_timestamp("updated", note.file_modified_at);
        }
    }

    if !payload.related.notes.is_empty() {
        println!("\nBacklinks Into This Day");
        for note in &payload.related.notes {
            print_note_line(note);
        }
    }

    if !payload.activity.metrics.is_empty() {
        println!("\nMetrics Recorded");
        for metric in &payload.activity.metrics {
            print_metric_line(metric);
        }
    }

    if !payload.activity.links.is_empty() {
        println!("\nLinks In Notes Changed That Day");
        for link in &payload.activity.links {
            print_link_line(link);
        }
    }

    if !payload.related.days.is_empty() {
        println!("\nAdjacent Days Worth Comparing");
        for day in &payload.related.days {
            println!("- Day: {day}");
        }
    }
}

fn render_window_context(payload: &ContextPayload) {
    if !payload.activity.notes_created.is_empty() {
        println!("\nNotes Added");
        for note in &payload.activity.notes_created {
            print_note_line(note);
            print_optional_timestamp("file created", note.created_at);
        }
    }

    if !payload.activity.notes_updated.is_empty() {
        println!("\nNotes Updated");
        for note in &payload.activity.notes_updated {
            print_note_line(note);
            print_optional_timestamp("updated", note.file_modified_at);
        }
    }

    if !payload.activity.metrics.is_empty() {
        println!("\nMetrics Recorded");
        for metric in &payload.activity.metrics {
            print_metric_line(metric);
        }
    }

    if !payload.related.metric_rollups.is_empty() {
        println!("\nMetric Trends");
        for rollup in &payload.related.metric_rollups {
            print_metric_rollup(rollup);
        }
    }

    if !payload.activity.links.is_empty() {
        println!("\nLinks In Notes Changed In This Window");
        for link in &payload.activity.links {
            print_link_line(link);
        }
    }

    if !payload.related.days.is_empty() {
        println!("\nActive Days");
        for day in &payload.related.days {
            println!("- Day: {day}");
        }
    }
}

fn render_note_context(payload: &ContextPayload) {
    println!("\nLeads");
    if !payload.related.days.is_empty() {
        println!("Days");
        for day in &payload.related.days {
            println!("- Day: {day}");
        }
    }
    if !payload.related.notes.is_empty() {
        println!("Related Notes");
        for note in &payload.related.notes {
            print_note_line(note);
        }
    }
    if !payload.related.metrics.is_empty() {
        println!("Metrics Tied To This Note");
        for metric in &payload.related.metrics {
            print_metric_lead_line(metric);
        }
    }
    if payload.related.days.is_empty()
        && payload.related.notes.is_empty()
        && payload.related.metrics.is_empty()
    {
        println!("- none");
    }

    if !payload.activity.notes_updated.is_empty() {
        println!("\nFreshness");
        for note in &payload.activity.notes_updated {
            print_note_line(note);
            print_optional_timestamp("updated", note.file_modified_at);
        }
    }

    if !payload.links.items.is_empty() {
        println!("\nRelationships");
        for link in &payload.links.items {
            print_link_line(link);
        }
    }
}

fn normalized_metric_range(range: Option<&str>) -> Option<&str> {
    range.map(str::trim).filter(|range| !range.is_empty())
}

fn metric_summary_line(label: Option<&str>, range: Option<&str>) -> Option<String> {
    label.map(|label| match normalized_metric_range(range) {
        Some(range) => append_metric_range(label, range),
        None => label.to_string(),
    })
}

fn metric_context_header(target: &str, range: Option<&str>) -> String {
    match normalized_metric_range(range) {
        Some(range) => format!("Context: metric {target} (range: {range})"),
        None => format!("Context: metric {target}"),
    }
}

fn append_metric_range(label: &str, range: &str) -> String {
    if label.ends_with(')') {
        if let Some(open_index) = label.rfind(" (") {
            let prefix = &label[..open_index];
            let details = &label[open_index + 2..label.len() - 1];
            return format!("{prefix} ({details}, range: {range})");
        }
    }

    format!("{label} (range: {range})")
}

fn render_metric_context(payload: &ContextPayload) {
    render_metric_context_with_range(payload, None);
}

fn render_metric_context_with_range(payload: &ContextPayload, metric_range: Option<&str>) {
    println!(
        "{}",
        metric_context_header(&payload.summary.target, metric_range)
    );
    if let Some(label) = metric_summary_line(payload.summary.label.as_deref(), metric_range) {
        println!("{label}");
    }

    if !payload.activity.metrics.is_empty() {
        println!("\nLatest Records");
        for metric in &payload.activity.metrics {
            print_metric_line(metric);
        }
    }

    if !payload.related.metric_rollups.is_empty() {
        println!("\nMetric Trends");
        for rollup in &payload.related.metric_rollups {
            print_metric_rollup(rollup);
        }
    }

    if !payload.related.days.is_empty() {
        println!("\nRelated Days");
        for day in &payload.related.days {
            println!("- Day: {day}");
        }
    }

    if !payload.related.notes.is_empty() {
        println!("\nRelated Notes");
        for note in &payload.related.notes {
            print_note_line(note);
        }
    }

    if !payload.related.metrics.is_empty() {
        println!("\nNearby Metrics");
        for metric in &payload.related.metrics {
            print_metric_lead_line(metric);
        }
    }
}

fn render_source_context(payload: &ContextPayload) {
    if !payload.activity.metrics.is_empty() {
        println!("\nMetrics");
        for metric in &payload.activity.metrics {
            print_metric_line(metric);
        }
    }

    if !payload.related.metric_rollups.is_empty() {
        println!("\nMetric Trends");
        for rollup in &payload.related.metric_rollups {
            print_metric_rollup(rollup);
        }
    }

    if !payload.related.notes.is_empty() {
        println!("\nRelated Notes");
        for note in &payload.related.notes {
            print_note_line(note);
        }
    }

    if !payload.related.days.is_empty() {
        println!("\nActive Days");
        for day in &payload.related.days {
            println!("- Day: {day}");
        }
    }

    if !payload.related.metrics.is_empty() {
        println!("\nMetric Themes");
        for metric in &payload.related.metrics {
            print_metric_lead_line(metric);
        }
    } else if !payload.related.metric_keys.is_empty() {
        println!("\nMetric Themes");
        for key in &payload.related.metric_keys {
            println!("- Metric: {key}");
        }
    }
}

fn render_attention(payload: &ContextPayload) {
    println!("\nAttention");
    if payload.attention.items.is_empty() {
        println!("- none");
        return;
    }
    for item in &payload.attention.items {
        print_attention_line(item);
    }
}

fn render_next_pivots(pivots: &[ContextPivot]) {
    if pivots.is_empty() {
        return;
    }
    println!("\nNext Pivots");
    for pivot in pivots {
        let suffix = render_reason_and_evidence(
            Some(pivot.reason.as_str()),
            pivot.evidence_kind,
            pivot.confidence,
        );
        println!("- {}{}", pivot.command, suffix);
    }
}

fn print_note_line(note: &arrowhead_core::ContextNoteItem) {
    let label = note.title.as_deref().unwrap_or(&note.note_id);
    let path = note
        .relative_path
        .as_ref()
        .map(|path| format!(" [{}]", path.display()))
        .unwrap_or_default();
    let reason =
        render_reason_and_evidence(note.reason.as_deref(), note.evidence_kind, note.confidence);
    println!("- Note: {label}{path}{reason}");
}

fn print_optional_timestamp(label: &str, value: Option<chrono::DateTime<chrono::Utc>>) {
    if let Some(timestamp) = value {
        println!("  {}: {}", label, timestamp.to_rfc3339());
    }
}

fn print_metric_line(record: &arrowhead_core::MetricRecordEntry) {
    let unit_suffix = record
        .record
        .unit
        .as_deref()
        .map(|unit| format!(" {unit}"))
        .unwrap_or_default();
    println!(
        "- Metric: {} {}{} from {} ({})",
        record.record.id, record.record.value, unit_suffix, record.record.source, record.record.key
    );
}

fn print_metric_lead_line(metric: &ContextMetricItem) {
    let unit_suffix = metric
        .unit
        .as_deref()
        .map(|unit| format!(" {unit}"))
        .unwrap_or_default();
    let date_suffix = metric
        .date
        .map(|date| format!(" on {date}"))
        .unwrap_or_default();
    let reason_suffix = render_reason_and_evidence(
        metric.reason.as_deref(),
        metric.evidence_kind,
        metric.confidence,
    );
    println!(
        "- Metric: {} = {}{} from {}{}{}",
        metric.key, metric.value, unit_suffix, metric.source, date_suffix, reason_suffix
    );
}

fn print_metric_rollup(rollup: &ContextMetricRollup) {
    let source_suffix = rollup
        .source
        .as_deref()
        .map(|source| format!(" from {source}"))
        .unwrap_or_default();
    let reason_suffix = rollup
        .reason
        .as_deref()
        .map(|reason| format!(" ({reason})"))
        .unwrap_or_default();
    println!(
        "- Trend: {}{} ({} active day{}, {} record{}){}",
        rollup.key,
        source_suffix,
        rollup.active_day_count,
        if rollup.active_day_count == 1 {
            ""
        } else {
            "s"
        },
        rollup.matching_record_count,
        if rollup.matching_record_count == 1 {
            ""
        } else {
            "s"
        },
        reason_suffix
    );
    let unit_suffix = rollup
        .unit
        .as_deref()
        .map(|unit| format!(" {unit}"))
        .unwrap_or_default();
    for bucket in &rollup.buckets {
        println!(
            "  {}  {}{}  ({} record{})",
            bucket.date,
            bucket.value,
            unit_suffix,
            bucket.record_count,
            if bucket.record_count == 1 { "" } else { "s" }
        );
    }
}

fn print_link_line(link: &ContextLink) {
    let evidence = match link.confidence {
        Some(confidence) => format!("{:?} {:.2}", link.kind, confidence).to_lowercase(),
        None => format!("{:?}", link.kind).to_lowercase(),
    };
    println!(
        "- {}: {} -> {} ({})",
        evidence, link.from, link.to, link.reason
    );
}

fn render_reason_and_evidence(
    reason: Option<&str>,
    evidence_kind: Option<ContextEvidenceKind>,
    confidence: Option<f32>,
) -> String {
    let evidence_label = render_evidence_label(evidence_kind, confidence);
    match (reason, evidence_label) {
        (Some(reason), Some(evidence)) => format!(" ({reason}; {evidence})"),
        (Some(reason), None) => format!(" ({reason})"),
        (None, Some(evidence)) => format!(" ({evidence})"),
        (None, None) => String::new(),
    }
}

fn render_evidence_label(
    evidence_kind: Option<ContextEvidenceKind>,
    confidence: Option<f32>,
) -> Option<String> {
    let label = match evidence_kind? {
        ContextEvidenceKind::Explicit => "explicit".to_string(),
        ContextEvidenceKind::Structural => "structural".to_string(),
        ContextEvidenceKind::Inferred => match confidence {
            Some(confidence) => format!("inferred {confidence:.2}"),
            None => "inferred".to_string(),
        },
    };
    Some(label)
}

fn print_attention_line(item: &ContextAttentionItem) {
    let mut suffix = String::new();
    if let Some(note_id) = item.note_id.as_deref() {
        suffix.push_str(&format!(" note:{note_id}"));
    }
    if let Some(metric_id) = item.metric_id.as_deref() {
        suffix.push_str(&format!(" metric:{metric_id}"));
    }
    if let Some(source_file) = item.source_file.as_ref() {
        suffix.push_str(&format!(
            " {}:{}",
            source_file.display(),
            item.source_line.unwrap_or(0)
        ));
    }
    println!("- {}:{} {}", item.kind, suffix, item.message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_day_supports_this_last_and_explicit_values() {
        let today = NaiveDate::from_ymd_opt(2026, 4, 17).expect("valid date");

        let default_day = resolve_day_with_today(
            &DayArgs {
                day: None,
                this: false,
                last: false,
                note_limit: DEFAULT_CONTEXT_NOTE_LIMIT,
                metric_limit: DEFAULT_CONTEXT_METRIC_LIMIT,
                json: false,
            },
            today,
        )
        .expect("default day");
        assert_eq!(default_day, today);

        let last_day = resolve_day_with_today(
            &DayArgs {
                day: None,
                this: false,
                last: true,
                note_limit: DEFAULT_CONTEXT_NOTE_LIMIT,
                metric_limit: DEFAULT_CONTEXT_METRIC_LIMIT,
                json: false,
            },
            today,
        )
        .expect("last day");
        assert_eq!(
            last_day,
            NaiveDate::from_ymd_opt(2026, 4, 16).expect("valid date")
        );

        let explicit_day = resolve_day_with_today(
            &DayArgs {
                day: Some("2026-04-12".to_string()),
                this: false,
                last: false,
                note_limit: DEFAULT_CONTEXT_NOTE_LIMIT,
                metric_limit: DEFAULT_CONTEXT_METRIC_LIMIT,
                json: false,
            },
            today,
        )
        .expect("explicit day");
        assert_eq!(
            explicit_day,
            NaiveDate::from_ymd_opt(2026, 4, 12).expect("valid date")
        );
    }

    #[test]
    fn resolve_month_selector_accepts_month_shorthand() {
        let selector = resolve_month_selector(&MonthArgs {
            month: Some("2026-04".to_string()),
            this: false,
            last: false,
            note_limit: DEFAULT_CONTEXT_NOTE_LIMIT,
            metric_limit: DEFAULT_CONTEXT_METRIC_LIMIT,
            json: false,
        })
        .expect("month selector");

        assert_eq!(
            selector,
            MonthContextSelector::ContainingDay(
                NaiveDate::from_ymd_opt(2026, 4, 1).expect("valid date")
            )
        );
    }

    #[test]
    fn metric_context_header_includes_active_range() {
        assert_eq!(
            metric_context_header("nutrition.energy_intake", Some("past7d")),
            "Context: metric nutrition.energy_intake (range: past7d)"
        );
        assert_eq!(
            metric_context_header("nutrition.energy_intake", None),
            "Context: metric nutrition.energy_intake"
        );
    }

    #[test]
    fn metric_summary_line_includes_active_range() {
        assert_eq!(
            metric_summary_line(Some("nutrition.energy_intake (10 records)"), Some("past7d"))
                .as_deref(),
            Some("nutrition.energy_intake (10 records, range: past7d)")
        );
        assert_eq!(
            metric_summary_line(Some("nutrition.energy_intake (10 records)"), None).as_deref(),
            Some("nutrition.energy_intake (10 records)")
        );
    }

    #[test]
    fn metric_summary_line_appends_range_when_label_has_no_record_suffix() {
        assert_eq!(
            metric_summary_line(Some("body.weight from withings"), Some("past7d")).as_deref(),
            Some("body.weight from withings (range: past7d)")
        );
    }
}
