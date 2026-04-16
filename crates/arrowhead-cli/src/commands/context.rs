//! `arrowhead context` command family.

use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use clap::{Args, Subcommand};
use serde_json::to_string_pretty;
use tracing::warn;

use super::CommandContext;
use crate::logging;
use arrowhead_core::{
    ContextAttentionItem, ContextLink, ContextMetricItem, ContextPayload, ContextPivot,
    ContextService, ContextTargetKind, DEFAULT_CONTEXT_METRIC_LIMIT, DEFAULT_CONTEXT_NOTE_LIMIT,
    MonthContextSelector, SearchConfig, SearchService, Vault, VaultConfig, WeekContextSelector,
    embeddings::EmbeddingPipeline, sqlite::IndexDatabase,
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
    /// Day to inspect in YYYY-MM-DD format.
    pub day: String,
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
    /// Optional day inside the month to inspect in YYYY-MM-DD format.
    #[arg(conflicts_with_all = ["this", "last"])]
    pub day: Option<String>,
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
            service
                .day(&args.day, Some(args.note_limit), Some(args.metric_limit))
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
    if let Some(day) = args.day.as_deref() {
        let parsed = NaiveDate::parse_from_str(day.trim(), "%Y-%m-%d")
            .with_context(|| format!("invalid month day `{}`", day.trim()))?;
        return Ok(MonthContextSelector::ContainingDay(parsed));
    }
    Ok(MonthContextSelector::ThisMonth)
}

fn render_day_context(payload: &ContextPayload) {
    if !payload.history.notes.is_empty() {
        println!("\nDaily Note");
        for note in &payload.history.notes {
            print_note_line(note);
        }
    }

    if !payload.activity.notes_created.is_empty() {
        println!("\nNotes Created");
        for note in &payload.activity.notes_created {
            print_note_line(note);
            print_optional_timestamp("created", note.created_at);
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
        println!("\nNotes Created");
        for note in &payload.activity.notes_created {
            print_note_line(note);
            print_optional_timestamp("created", note.created_at);
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
        println!("- {} ({})", pivot.command, pivot.reason);
    }
}

fn print_note_line(note: &arrowhead_core::ContextNoteItem) {
    let label = note.title.as_deref().unwrap_or(&note.note_id);
    let path = note
        .relative_path
        .as_ref()
        .map(|path| format!(" [{}]", path.display()))
        .unwrap_or_default();
    let reason = note
        .reason
        .as_deref()
        .map(|reason| format!(" ({reason})"))
        .unwrap_or_default();
    println!("- Note: {}{}{}", label, path, reason);
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
    let reason_suffix = metric
        .reason
        .as_deref()
        .map(|reason| format!(" ({reason})"))
        .unwrap_or_default();
    println!(
        "- Metric: {} = {}{} from {}{}{}",
        metric.key, metric.value, unit_suffix, metric.source, date_suffix, reason_suffix
    );
}

fn print_link_line(link: &ContextLink) {
    println!(
        "- {:?}: {} -> {} ({})",
        link.kind, link.from, link.to, link.reason
    );
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
