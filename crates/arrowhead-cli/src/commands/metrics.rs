//! `arrowhead metrics` read-only command family.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::json;
use tracing::info;

use super::CommandContext;
use crate::logging;
use arrowhead_core::{
    MetricRecordEntry, MetricsService, Vault, VaultConfig, sqlite::IndexDatabase,
};

/// Read-only metrics operations.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct MetricsCommand {
    /// Select which metrics operation to perform.
    #[command(subcommand)]
    pub action: MetricsAction,
}

/// Available metrics subcommands.
#[derive(Debug, Subcommand, Clone, PartialEq)]
pub enum MetricsAction {
    /// List indexed metrics files.
    Files(FilesArgs),
    /// Read a metric record by id or `metric:<id>` reference.
    Read(ReadArgs),
    /// Search indexed metrics records.
    Search(SearchArgs),
}

/// Arguments for `arrowhead metrics files`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct FilesArgs {
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `arrowhead metrics read`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct ReadArgs {
    /// Stable metric id or `metric:<id>` reference.
    pub metric_id: String,
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `arrowhead metrics search`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct SearchArgs {
    /// Search query supporting `key:`, `source:`, `file:`, `date:`, and `note:` filters.
    pub query: String,
    /// Maximum number of records to return.
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Execute the requested metrics command.
pub async fn run(ctx: &CommandContext, command: &MetricsCommand) -> Result<()> {
    let vault_path = ctx
        .config
        .vault
        .clone()
        .context("no vault configured. Provide --vault or run `arrowhead init`.")?;

    let vault = Vault::new(VaultConfig::new(vault_path))?;
    vault.ensure_arrowhead_dirs()?;

    let logs_dir = vault.paths().logs_dir();
    let _logging_guard = logging::scoped_file_logging(&logs_dir, ctx.verbosity())?;

    let db_path = vault.paths().arrowhead_dir.join("index.db");
    let service = MetricsService::new(Arc::new(IndexDatabase::open(&db_path)?));

    match &command.action {
        MetricsAction::Files(args) => {
            info!(json = args.json, "listing indexed metrics files");
            let files = service.list_files().await?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "files": files }))?
                );
                return Ok(());
            }

            if files.is_empty() {
                render_empty_metrics_state(&vault)?;
                return Ok(());
            }

            for file in files {
                println!(
                    "{}\n  rows: {}  records: {}  warnings: {}  errors: {}\n  modified: {}\n  indexed: {}",
                    file.relative_path.display(),
                    file.row_count,
                    file.record_count,
                    file.warning_count,
                    file.error_count,
                    file.file_modified_at.to_rfc3339(),
                    file.indexed_at.to_rfc3339(),
                );
            }
            Ok(())
        }
        MetricsAction::Read(args) => {
            info!(metric_id = %args.metric_id, json = args.json, "reading metrics record");
            ensure_metrics_index_available(&vault, &service).await?;
            let record = service.read_record(&args.metric_id).await?;
            let Some(record) = record else {
                bail!(
                    "metric {} was not found in the index. Run `arrowhead index start` if the record was added recently.",
                    args.metric_id
                );
            };

            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "record": record }))?
                );
            } else {
                print_metric_record(&record);
            }
            Ok(())
        }
        MetricsAction::Search(args) => {
            info!(
                query = args.query.as_str(),
                limit = args.limit,
                json = args.json,
                "searching indexed metrics records"
            );
            ensure_metrics_index_available(&vault, &service).await?;
            let results = service.search(&args.query, Some(args.limit)).await?;

            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "query": args.query,
                        "total": results.len(),
                        "results": results
                    }))?
                );
                return Ok(());
            }

            if results.is_empty() {
                println!("No metric records matched `{}`.", args.query);
                return Ok(());
            }

            for (index, result) in results.iter().enumerate() {
                if index > 0 {
                    println!();
                }
                print_metric_record(result);
            }
            Ok(())
        }
    }
}

async fn ensure_metrics_index_available(vault: &Vault, service: &MetricsService) -> Result<()> {
    if !service.list_files().await?.is_empty() {
        return Ok(());
    }

    let discovered = vault.metrics_files()?;
    if discovered.is_empty() {
        bail!("no metrics files were discovered in this vault");
    }

    bail!(
        "metrics files are present but not indexed yet. Run `arrowhead index start` to index {} discovered file(s).",
        discovered.len()
    )
}

fn render_empty_metrics_state(vault: &Vault) -> Result<()> {
    let discovered = vault.metrics_files()?;
    if discovered.is_empty() {
        println!("No metrics files discovered in this vault.");
    } else {
        println!(
            "No indexed metrics files yet. Run `arrowhead index start` to index {} discovered file(s).",
            discovered.len()
        );
    }
    Ok(())
}

fn print_metric_record(record: &MetricRecordEntry) {
    let metric = &record.record;
    let unit_suffix = metric
        .unit
        .as_deref()
        .map(|unit| format!(" {unit}"))
        .unwrap_or_default();

    println!(
        "{}\n  key: {}\n  value: {}{}\n  source: {}\n  ts: {}\n  file: {}:{}\n  validation: {}",
        metric.id,
        metric.key,
        metric.value,
        unit_suffix,
        metric.source,
        metric.ts.to_rfc3339(),
        record.source_file.display(),
        record.source_line,
        metric_validation_label(record)
    );

    if let Some(date) = metric.date {
        println!("  date: {date}");
    }
    if let Some(origin_id) = metric.origin_id.as_deref() {
        println!("  origin_id: {origin_id}");
    }
    if let Some(note) = metric.note.as_deref() {
        println!("  note: {note}");
    }
    if !metric.tags.is_empty() {
        println!("  tags: {}", metric.tags.join(", "));
    }
    if let Some(context) = metric.context.as_ref() {
        if let Ok(text) = serde_json::to_string(context) {
            println!("  context: {text}");
        }
    }
    if !metric.extra_fields.is_empty() {
        if let Ok(text) = serde_json::to_string(&metric.extra_fields) {
            println!("  extra: {text}");
        }
    }
    if !record.issues.is_empty() {
        println!("  issues:");
        for issue in &record.issues {
            let field = issue
                .field
                .as_deref()
                .map(|value| format!(" ({value})"))
                .unwrap_or_default();
            println!(
                "    - {}{}: {}",
                issue_code_label(issue),
                field,
                issue.message
            );
        }
    }
}

fn metric_validation_label(record: &MetricRecordEntry) -> &'static str {
    match record.validation_status {
        arrowhead_core::MetricValidationStatus::Valid => "valid",
        arrowhead_core::MetricValidationStatus::Warning => "warning",
        arrowhead_core::MetricValidationStatus::Invalid => "invalid",
    }
}

fn issue_code_label(issue: &arrowhead_core::MetricValidationIssue) -> &'static str {
    match issue.code {
        arrowhead_core::MetricIssueCode::InvalidJson => "invalid_json",
        arrowhead_core::MetricIssueCode::InvalidRowType => "invalid_row_type",
        arrowhead_core::MetricIssueCode::InvalidId => "invalid_id",
        arrowhead_core::MetricIssueCode::InvalidTimestamp => "invalid_timestamp",
        arrowhead_core::MetricIssueCode::InvalidKey => "invalid_key",
        arrowhead_core::MetricIssueCode::InvalidValue => "invalid_value",
        arrowhead_core::MetricIssueCode::InvalidSource => "invalid_source",
        arrowhead_core::MetricIssueCode::InvalidDate => "invalid_date",
        arrowhead_core::MetricIssueCode::InvalidUnit => "invalid_unit",
        arrowhead_core::MetricIssueCode::InvalidOriginId => "invalid_origin_id",
        arrowhead_core::MetricIssueCode::InvalidNote => "invalid_note",
        arrowhead_core::MetricIssueCode::InvalidContext => "invalid_context",
        arrowhead_core::MetricIssueCode::InvalidTags => "invalid_tags",
        arrowhead_core::MetricIssueCode::UnknownField => "unknown_field",
        arrowhead_core::MetricIssueCode::UnknownMetricKey => "unknown_metric_key",
        arrowhead_core::MetricIssueCode::UnknownUnit => "unknown_unit",
        arrowhead_core::MetricIssueCode::UnitMismatch => "unit_mismatch",
        arrowhead_core::MetricIssueCode::DuplicateId => "duplicate_id",
        arrowhead_core::MetricIssueCode::DuplicateOriginId => "duplicate_origin_id",
    }
}
