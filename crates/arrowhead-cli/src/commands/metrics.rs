//! `arrowhead metrics` command family.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, FixedOffset, NaiveDate};
use clap::{Args, Subcommand};
use serde_json::{Map, Value, json};
use tracing::info;

use super::CommandContext;
use crate::logging;
use arrowhead_core::{
    DeletedMetricRecord, MetricCreateRequest, MetricRecordEntry, MetricUpdateRequest,
    MetricsMutationService, MetricsService, PatchValue, Vault, VaultConfig, sqlite::IndexDatabase,
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
    /// Create a new metric record.
    Create(CreateArgs),
    /// Update an existing metric record.
    Update(UpdateArgs),
    /// Delete a metric record by id.
    Delete(DeleteArgs),
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

/// Arguments for `arrowhead metrics create`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct CreateArgs {
    /// Explicit target file. Defaults to the vault metrics write file.
    #[arg(long)]
    pub file: Option<PathBuf>,
    /// Optional stable id. Defaults to a generated ULID-style identifier.
    #[arg(long)]
    pub id: Option<String>,
    /// RFC 3339 timestamp recorded for the metric event.
    #[arg(long)]
    pub ts: String,
    /// Metric key.
    #[arg(long)]
    pub key: String,
    /// Numeric metric value.
    #[arg(long)]
    pub value: f64,
    /// Source that produced the metric.
    #[arg(long)]
    pub source: String,
    /// Optional YYYY-MM-DD date bucket.
    #[arg(long)]
    pub date: Option<String>,
    /// Optional unit string.
    #[arg(long)]
    pub unit: Option<String>,
    /// Optional provenance id.
    #[arg(long = "origin-id")]
    pub origin_id: Option<String>,
    /// Optional note text.
    #[arg(long)]
    pub note: Option<String>,
    /// Optional JSON object assigned to the `context` field.
    #[arg(long = "context-json")]
    pub context_json: Option<String>,
    /// Optional tags attached to the metric.
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `arrowhead metrics update`.
#[derive(Debug, Args, Clone, PartialEq, Default)]
pub struct UpdateArgs {
    /// Stable metric id or `metric:<id>` reference.
    pub metric_id: String,
    /// Optional replacement RFC 3339 timestamp.
    #[arg(long)]
    pub ts: Option<String>,
    /// Optional replacement metric key.
    #[arg(long)]
    pub key: Option<String>,
    /// Optional replacement numeric value.
    #[arg(long)]
    pub value: Option<f64>,
    /// Optional replacement source.
    #[arg(long)]
    pub source: Option<String>,
    /// Optional replacement YYYY-MM-DD date.
    #[arg(long)]
    pub date: Option<String>,
    /// Clear the `date` field.
    #[arg(long)]
    pub clear_date: bool,
    /// Optional replacement unit.
    #[arg(long)]
    pub unit: Option<String>,
    /// Clear the `unit` field.
    #[arg(long)]
    pub clear_unit: bool,
    /// Optional replacement provenance id.
    #[arg(long = "origin-id")]
    pub origin_id: Option<String>,
    /// Clear the `origin_id` field.
    #[arg(long = "clear-origin-id")]
    pub clear_origin_id: bool,
    /// Optional replacement note text.
    #[arg(long)]
    pub note: Option<String>,
    /// Clear the `note` field.
    #[arg(long = "clear-note")]
    pub clear_note: bool,
    /// Optional replacement context JSON object.
    #[arg(long = "context-json")]
    pub context_json: Option<String>,
    /// Clear the `context` field.
    #[arg(long = "clear-context")]
    pub clear_context: bool,
    /// Replace the entire tags list with these values.
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    /// Clear all tags.
    #[arg(long = "clear-tags")]
    pub clear_tags: bool,
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `arrowhead metrics delete`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct DeleteArgs {
    /// Stable metric id or `metric:<id>` reference.
    pub metric_id: String,
    /// Required acknowledgement for destructive deletes.
    #[arg(long)]
    pub yes: bool,
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

    let vault = Arc::new(Vault::new(VaultConfig::new(vault_path))?);
    vault.ensure_arrowhead_dirs()?;

    let logs_dir = vault.paths().logs_dir();
    let _logging_guard = logging::scoped_file_logging(&logs_dir, ctx.verbosity())?;

    let db_path = vault.paths().arrowhead_dir.join("index.db");
    let database = Arc::new(IndexDatabase::open(&db_path)?);
    let read_service = MetricsService::new(Arc::clone(&database));
    let mutation_service = MetricsMutationService::new(Arc::clone(&vault), database);

    match &command.action {
        MetricsAction::Files(args) => {
            info!(json = args.json, "listing indexed metrics files");
            let files = read_service.list_files().await?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "files": files }))?
                );
                return Ok(());
            }

            if files.is_empty() {
                render_empty_metrics_state(vault.as_ref())?;
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
            ensure_metrics_index_available(vault.as_ref(), &read_service).await?;
            let record = read_service.read_record(&args.metric_id).await?;
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
            ensure_metrics_index_available(vault.as_ref(), &read_service).await?;
            let results = read_service.search(&args.query, Some(args.limit)).await?;

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
        MetricsAction::Create(args) => {
            let record = mutation_service.create(build_create_request(args)?).await?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "action": "created",
                        "record": record
                    }))?
                );
            } else {
                println!("Created metric record:\n");
                print_metric_record(&record);
            }
            Ok(())
        }
        MetricsAction::Update(args) => {
            let record = mutation_service.update(build_update_request(args)?).await?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "action": "updated",
                        "record": record
                    }))?
                );
            } else {
                println!("Updated metric record:\n");
                print_metric_record(&record);
            }
            Ok(())
        }
        MetricsAction::Delete(args) => {
            if !args.yes {
                bail!("metrics delete requires `--yes`");
            }
            let deleted = mutation_service.delete(&args.metric_id).await?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "action": "deleted",
                        "deleted": deleted
                    }))?
                );
            } else {
                print_deleted_metric_record(&deleted);
            }
            Ok(())
        }
    }
}

fn build_create_request(args: &CreateArgs) -> Result<MetricCreateRequest> {
    Ok(MetricCreateRequest {
        file: args.file.clone(),
        id: args.id.clone(),
        ts: parse_metric_timestamp(&args.ts)?,
        key: args.key.clone(),
        value: args.value,
        source: args.source.clone(),
        date: parse_optional_metric_date(args.date.as_deref())?,
        unit: args.unit.clone(),
        origin_id: args.origin_id.clone(),
        note: args.note.clone(),
        context: parse_optional_context_object(args.context_json.as_deref())?,
        tags: args.tags.clone(),
        extra_fields: BTreeMap::new(),
    })
}

fn build_update_request(args: &UpdateArgs) -> Result<MetricUpdateRequest> {
    Ok(MetricUpdateRequest {
        metric_id: args.metric_id.clone(),
        ts: args.ts.as_deref().map(parse_metric_timestamp).transpose()?,
        key: args.key.clone(),
        value: args.value,
        source: args.source.clone(),
        date: parse_patch_metric_date(args.date.as_deref(), args.clear_date)?,
        unit: parse_patch_string(args.unit.clone(), args.clear_unit, "unit")?,
        origin_id: parse_patch_string(args.origin_id.clone(), args.clear_origin_id, "origin_id")?,
        note: parse_patch_string(args.note.clone(), args.clear_note, "note")?,
        context: parse_patch_context_object(args.context_json.as_deref(), args.clear_context)?,
        tags: parse_patch_tags(&args.tags, args.clear_tags)?,
    })
}

fn parse_metric_timestamp(input: &str) -> Result<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(input.trim())
        .with_context(|| format!("invalid metrics timestamp `{}`", input.trim()))
}

fn parse_optional_metric_date(input: Option<&str>) -> Result<Option<NaiveDate>> {
    input
        .map(|value| parse_metric_date(value.trim()))
        .transpose()
}

fn parse_metric_date(input: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(input, "%Y-%m-%d")
        .with_context(|| format!("invalid metrics date `{input}`"))
}

fn parse_optional_context_object(input: Option<&str>) -> Result<Option<Map<String, Value>>> {
    input.map(parse_context_object).transpose()
}

fn parse_context_object(input: &str) -> Result<Map<String, Value>> {
    match serde_json::from_str::<Value>(input.trim())
        .with_context(|| format!("invalid context JSON `{}`", input.trim()))?
    {
        Value::Object(object) => Ok(object),
        _ => bail!("context JSON must be an object"),
    }
}

fn parse_patch_metric_date(input: Option<&str>, clear: bool) -> Result<PatchValue<NaiveDate>> {
    if clear && input.is_some() {
        bail!("cannot use `--date` together with `--clear-date`");
    }
    if clear {
        return Ok(PatchValue::Clear);
    }
    input
        .map(|value| parse_metric_date(value.trim()).map(PatchValue::Set))
        .transpose()
        .map(|value| value.unwrap_or_default())
}

fn parse_patch_context_object(
    input: Option<&str>,
    clear: bool,
) -> Result<PatchValue<Map<String, Value>>> {
    if clear && input.is_some() {
        bail!("cannot use `--context-json` together with `--clear-context`");
    }
    if clear {
        return Ok(PatchValue::Clear);
    }
    input
        .map(|value| parse_context_object(value.trim()).map(PatchValue::Set))
        .transpose()
        .map(|value| value.unwrap_or_default())
}

fn parse_patch_string(
    input: Option<String>,
    clear: bool,
    field_name: &str,
) -> Result<PatchValue<String>> {
    if clear && input.is_some() {
        bail!("cannot set `{field_name}` and clear it in the same command");
    }
    if clear {
        return Ok(PatchValue::Clear);
    }
    Ok(input.map(PatchValue::Set).unwrap_or_default())
}

fn parse_patch_tags(input: &[String], clear: bool) -> Result<PatchValue<Vec<String>>> {
    if clear && !input.is_empty() {
        bail!("cannot use `--tag` together with `--clear-tags`");
    }
    if clear {
        return Ok(PatchValue::Clear);
    }
    if input.is_empty() {
        return Ok(PatchValue::Unchanged);
    }
    Ok(PatchValue::Set(input.to_vec()))
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

fn print_deleted_metric_record(record: &DeletedMetricRecord) {
    println!(
        "Deleted metric {} from {}:{}",
        record.metric_id,
        record.source_file.display(),
        record.source_line
    );
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
