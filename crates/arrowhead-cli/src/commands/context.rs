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
    ContextAttentionItem, ContextLink, ContextPayload, ContextService, ContextTargetKind,
    DEFAULT_CONTEXT_METRIC_LIMIT, DEFAULT_CONTEXT_NOTE_LIMIT, SearchConfig, SearchService, Vault,
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
    /// Optional metrics date range such as `past30d` or `2026-04-01..2026-04-15`.
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
    /// Optional metrics date range such as `past30d` or `2026-04-01..2026-04-15`.
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
        ContextAction::Changed(args) => args.json,
        ContextAction::Note(args) => args.json,
        ContextAction::Metric(args) => args.json,
        ContextAction::Source(args) => args.json,
    };

    if json {
        println!("{}", to_string_pretty(&payload)?);
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
    println!(
        "Context: {} {}",
        render_target_kind(payload.summary.kind),
        payload.summary.target
    );
    if let Some(label) = payload.summary.label.as_deref() {
        println!("{label}");
    }

    println!(
        "\nSummary\n- notes: {}\n- metrics: {}\n- links: {}\n- attention: {}",
        payload.summary.note_count,
        payload.summary.metric_count,
        payload.summary.link_count,
        payload.summary.attention_count
    );

    if !payload.history.notes.is_empty() || !payload.history.metrics.is_empty() {
        println!("\nHistory");
        for note in &payload.history.notes {
            print_note_line(note);
        }
        for metric in &payload.history.metrics {
            print_metric_line(metric);
        }
    }

    if !payload.activity.notes.is_empty()
        || !payload.activity.metrics.is_empty()
        || !payload.activity.files.is_empty()
    {
        println!("\nActivity");
        for note in &payload.activity.notes {
            print_note_line(note);
        }
        for metric in &payload.activity.metrics {
            print_metric_line(metric);
        }
        for file in &payload.activity.files {
            println!(
                "- File: {} (records {}, warnings {}, errors {})",
                file.relative_path.display(),
                file.record_count,
                file.warning_count,
                file.error_count
            );
        }
    }

    if !payload.links.items.is_empty() {
        println!("\nLinks");
        for link in &payload.links.items {
            print_link_line(link);
        }
    }

    if !payload.attention.items.is_empty() {
        println!("\nAttention");
        for item in &payload.attention.items {
            print_attention_line(item);
        }
    }

    if !payload.related.days.is_empty()
        || !payload.related.notes.is_empty()
        || !payload.related.metric_keys.is_empty()
        || !payload.related.sources.is_empty()
    {
        println!("\nRelated");
        for day in &payload.related.days {
            println!("- Day: {day}");
        }
        for note in &payload.related.notes {
            print_note_line(note);
        }
        for key in &payload.related.metric_keys {
            println!("- Metric key: {key}");
        }
        for source in &payload.related.sources {
            println!("- Source: {source}");
        }
    }
}

fn render_target_kind(kind: ContextTargetKind) -> &'static str {
    match kind {
        ContextTargetKind::Day => "day",
        ContextTargetKind::Week => "week",
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
