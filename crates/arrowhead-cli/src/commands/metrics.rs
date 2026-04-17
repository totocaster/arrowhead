//! `arrowhead metrics` command family.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, FixedOffset, NaiveDate};
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::{Map, Value, json};
use tracing::info;

use super::CommandContext;
use crate::logging;
use arrowhead_core::{
    AssignedMetricIdsSummary, CreatedMetricFile, DeletedMetricFile, DeletedMetricRecord,
    MetricCreateRequest, MetricRecordEntry, MetricUpdateRequest, MetricsMutationService,
    MetricsService, PatchValue, RenamedMetricFile, Vault, VaultConfig, sqlite::IndexDatabase,
};

/// Metrics read and write operations.
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
    /// Assign generated ids to legacy rows that are missing them.
    AssignMissingIds(AssignMissingIdsArgs),
}

/// Arguments for `arrowhead metrics files`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct FilesArgs {
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
    /// Optional file mutation subcommand.
    #[command(subcommand)]
    pub action: Option<FilesAction>,
}

/// Available `arrowhead metrics files` subcommands.
#[derive(Debug, Subcommand, Clone, PartialEq)]
pub enum FilesAction {
    /// Create an empty metrics file.
    Create(FileCreateArgs),
    /// Rename a metrics file.
    Rename(FileRenameArgs),
    /// Delete a metrics file.
    Delete(FileDeleteArgs),
}

/// Arguments for `arrowhead metrics files create`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct FileCreateArgs {
    /// Target metrics file relative to the vault root.
    pub path: PathBuf,
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `arrowhead metrics files rename`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct FileRenameArgs {
    /// Existing metrics file relative to the vault root.
    pub source_path: PathBuf,
    /// New metrics file relative to the vault root.
    pub destination_path: PathBuf,
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `arrowhead metrics files delete`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct FileDeleteArgs {
    /// Metrics file relative to the vault root.
    pub path: PathBuf,
    /// Required acknowledgement for destructive deletes.
    #[arg(long)]
    pub yes: bool,
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
    /// Search query supporting `key:`, `source:`, `file:`, `note:`, and `date:` filters like `2026-04`, `2026-04-16`, `past7d`, or `2026-04-01..2026-04-30`.
    pub query: String,
    /// Maximum number of records or aggregate buckets to return.
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
    /// Aggregate matching records into daily roll-ups.
    #[arg(long, value_enum)]
    pub aggregate: Option<MetricSearchAggregate>,
    /// Emit structured JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Supported aggregate modes for `arrowhead metrics search`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricSearchAggregate {
    /// Sum the matching metric values per day.
    Sum,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct MetricAggregateBucket {
    date: NaiveDate,
    value: f64,
    record_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct MetricAggregateSummary {
    aggregate: MetricSearchAggregate,
    key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    unit: Option<String>,
    matching_record_count: usize,
    buckets: Vec<MetricAggregateBucket>,
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

/// Arguments for `arrowhead metrics assign-missing-ids`.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct AssignMissingIdsArgs {
    /// Optional metrics file relative to the vault root. Defaults to all discovered metrics files.
    #[arg(long)]
    pub file: Option<PathBuf>,
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
            match &args.action {
                None => {
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
                }
                Some(FilesAction::Create(file_args)) => {
                    let created = mutation_service.create_file(&file_args.path).await?;
                    if file_args.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "action": "created",
                                "file": created
                            }))?
                        );
                    } else {
                        print_created_metrics_file(&created);
                    }
                }
                Some(FilesAction::Rename(file_args)) => {
                    let renamed = mutation_service
                        .rename_file(&file_args.source_path, &file_args.destination_path)
                        .await?;
                    if file_args.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "action": "renamed",
                                "file": renamed
                            }))?
                        );
                    } else {
                        print_renamed_metrics_file(&renamed);
                    }
                }
                Some(FilesAction::Delete(file_args)) => {
                    if !file_args.yes {
                        bail!("metrics files delete requires `--yes`");
                    }
                    let deleted = mutation_service.delete_file(&file_args.path).await?;
                    if file_args.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&json!({
                                "action": "deleted",
                                "file": deleted
                            }))?
                        );
                    } else {
                        print_deleted_metrics_file(&deleted);
                    }
                }
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
                aggregate = ?args.aggregate,
                json = args.json,
                "searching indexed metrics records"
            );
            ensure_metrics_index_available(vault.as_ref(), &read_service).await?;
            if let Some(aggregate) = args.aggregate {
                let results = read_service.search_all(&args.query).await?;
                if results.is_empty() {
                    println!("No metric records matched `{}`.", args.query);
                    return Ok(());
                }

                let summary = aggregate_metric_records(&results, aggregate, args.limit)?;
                if args.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "query": args.query,
                            "aggregate": summary,
                        }))?
                    );
                } else {
                    print_metric_aggregate(&summary);
                }
                return Ok(());
            }

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
        MetricsAction::AssignMissingIds(args) => {
            let summary = mutation_service
                .assign_missing_ids(args.file.as_deref())
                .await?;
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "action": "assigned_missing_ids",
                        "summary": summary
                    }))?
                );
            } else {
                print_assigned_metric_ids_summary(&summary);
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

fn print_created_metrics_file(file: &CreatedMetricFile) {
    println!("Created metrics file {}", file.relative_path.display());
}

fn print_renamed_metrics_file(file: &RenamedMetricFile) {
    println!(
        "Renamed metrics file {} -> {}",
        file.source_path.display(),
        file.destination_path.display()
    );
}

fn print_deleted_metrics_file(file: &DeletedMetricFile) {
    println!(
        "Deleted metrics file {} ({} row{})",
        file.relative_path.display(),
        file.row_count,
        if file.row_count == 1 { "" } else { "s" }
    );
}

fn print_assigned_metric_ids_summary(summary: &AssignedMetricIdsSummary) {
    if summary.total_assigned == 0 {
        println!(
            "No missing metric ids were found in {} file{}.",
            summary.files.len(),
            if summary.files.len() == 1 { "" } else { "s" }
        );
        return;
    }

    println!(
        "Assigned {} missing metric id{} across {} file{}.",
        summary.total_assigned,
        if summary.total_assigned == 1 { "" } else { "s" },
        summary
            .files
            .iter()
            .filter(|file| file.assigned_count > 0)
            .count(),
        if summary
            .files
            .iter()
            .filter(|file| file.assigned_count > 0)
            .count()
            == 1
        {
            ""
        } else {
            "s"
        }
    );
    for file in &summary.files {
        if file.assigned_count == 0 {
            continue;
        }
        println!(
            "  {}: {} row{}",
            file.relative_path.display(),
            file.assigned_count,
            if file.assigned_count == 1 { "" } else { "s" }
        );
    }
}

fn aggregate_metric_records(
    records: &[MetricRecordEntry],
    aggregate: MetricSearchAggregate,
    limit: usize,
) -> Result<MetricAggregateSummary> {
    if records.is_empty() {
        bail!("cannot aggregate an empty metrics result set");
    }

    let keys = records
        .iter()
        .map(|record| record.record.key.clone())
        .collect::<BTreeSet<_>>();
    if keys.len() != 1 {
        bail!(
            "aggregate {} requires a single metric key, but matched: {}",
            aggregate_label(aggregate),
            keys.into_iter().collect::<Vec<_>>().join(", ")
        );
    }

    let units = records
        .iter()
        .map(|record| record.record.unit.clone())
        .collect::<BTreeSet<_>>();
    if units.len() > 1 {
        let rendered = units
            .into_iter()
            .map(|unit| unit.unwrap_or_else(|| "(no unit)".to_string()))
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "aggregate {} requires a single unit, but matched: {}",
            aggregate_label(aggregate),
            rendered
        );
    }

    let key = keys.into_iter().next().expect("single key present");
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
        match aggregate {
            MetricSearchAggregate::Sum => {
                entry.0 += record.record.value;
            }
        }
        entry.1 += 1;
    }

    let mut buckets = totals
        .into_iter()
        .map(|(date, (value, record_count))| MetricAggregateBucket {
            date,
            value,
            record_count,
        })
        .collect::<Vec<_>>();
    buckets.sort_by(|left, right| right.date.cmp(&left.date));
    let limit = limit.max(1);
    if buckets.len() > limit {
        buckets.truncate(limit);
    }

    Ok(MetricAggregateSummary {
        aggregate,
        key,
        unit,
        matching_record_count: records.len(),
        buckets,
    })
}

fn aggregate_label(aggregate: MetricSearchAggregate) -> &'static str {
    match aggregate {
        MetricSearchAggregate::Sum => "sum",
    }
}

fn print_metric_aggregate(summary: &MetricAggregateSummary) {
    let unit_suffix = summary
        .unit
        .as_deref()
        .map(|unit| format!(" {unit}"))
        .unwrap_or_default();

    println!(
        "{} by day for {}{}\n  matched: {} record{}",
        aggregate_label(summary.aggregate),
        summary.key,
        unit_suffix,
        summary.matching_record_count,
        if summary.matching_record_count == 1 {
            ""
        } else {
            "s"
        }
    );
    for bucket in &summary.buckets {
        println!(
            "  {}  {}{}  ({} record{})",
            bucket.date,
            format_metric_number(bucket.value),
            unit_suffix,
            bucket.record_count,
            if bucket.record_count == 1 { "" } else { "s" }
        );
    }
}

fn format_metric_number(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }

    let mut formatted = format!("{value:.12}");
    while formatted.contains('.') && formatted.ends_with('0') {
        formatted.pop();
    }
    if formatted.ends_with('.') {
        formatted.pop();
    }
    if formatted == "-0" {
        "0".to_string()
    } else {
        formatted
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

#[cfg(test)]
mod tests {
    use super::*;

    fn metric_record(
        id: &str,
        key: &str,
        value: f64,
        unit: Option<&str>,
        date: Option<NaiveDate>,
        ts: &str,
    ) -> MetricRecordEntry {
        MetricRecordEntry {
            source_file: PathBuf::from("Metrics/health.metrics.ndjson"),
            source_line: 1,
            record: arrowhead_core::MetricRecord {
                id: id.to_string(),
                ts: DateTime::parse_from_rfc3339(ts).expect("valid ts"),
                key: key.to_string(),
                value,
                source: "manual".to_string(),
                date,
                unit: unit.map(str::to_string),
                origin_id: None,
                note: None,
                context: None,
                tags: Vec::new(),
                extra_fields: BTreeMap::new(),
            },
            raw_line: String::new(),
            validation_status: arrowhead_core::MetricValidationStatus::Valid,
            issues: Vec::new(),
        }
    }

    #[test]
    fn aggregate_metric_records_rolls_up_by_day() {
        let records = vec![
            metric_record(
                "01AAA",
                "nutrition.energy_intake",
                800.0,
                Some("kcal"),
                NaiveDate::from_ymd_opt(2026, 4, 14),
                "2026-04-14T08:30:00+00:00",
            ),
            metric_record(
                "01AAB",
                "nutrition.energy_intake",
                1200.0,
                Some("kcal"),
                NaiveDate::from_ymd_opt(2026, 4, 14),
                "2026-04-14T18:00:00+00:00",
            ),
            metric_record(
                "01AAC",
                "nutrition.energy_intake",
                900.0,
                Some("kcal"),
                None,
                "2026-04-15T09:00:00+00:00",
            ),
        ];

        let summary =
            aggregate_metric_records(&records, MetricSearchAggregate::Sum, 10).expect("aggregate");

        assert_eq!(summary.key, "nutrition.energy_intake");
        assert_eq!(summary.unit.as_deref(), Some("kcal"));
        assert_eq!(summary.matching_record_count, 3);
        assert_eq!(summary.buckets.len(), 2);
        assert_eq!(
            summary.buckets[0].date,
            NaiveDate::from_ymd_opt(2026, 4, 15).unwrap()
        );
        assert_eq!(summary.buckets[0].value, 900.0);
        assert_eq!(
            summary.buckets[1].date,
            NaiveDate::from_ymd_opt(2026, 4, 14).unwrap()
        );
        assert_eq!(summary.buckets[1].value, 2000.0);
        assert_eq!(summary.buckets[1].record_count, 2);
    }

    #[test]
    fn aggregate_metric_records_rejects_mixed_units() {
        let records = vec![
            metric_record(
                "01AAA",
                "nutrition.energy_intake",
                800.0,
                Some("kcal"),
                NaiveDate::from_ymd_opt(2026, 4, 14),
                "2026-04-14T08:30:00+00:00",
            ),
            metric_record(
                "01AAB",
                "nutrition.energy_intake",
                0.8,
                Some("MJ"),
                NaiveDate::from_ymd_opt(2026, 4, 14),
                "2026-04-14T12:00:00+00:00",
            ),
        ];

        let err = aggregate_metric_records(&records, MetricSearchAggregate::Sum, 10)
            .expect_err("mixed units should fail");
        assert!(err.to_string().contains("single unit"));
    }

    #[test]
    fn format_metric_number_trims_floating_point_noise() {
        assert_eq!(format_metric_number(211.39999999999998), "211.4");
        assert_eq!(format_metric_number(211.0), "211");
        assert_eq!(format_metric_number(0.123456789), "0.123456789");
    }
}
