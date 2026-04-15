//! Safe metrics file mutation helpers.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use tokio::task;

use crate::{
    MetricIssueSeverity, MetricRecord, MetricRecordEntry, MetricValidationIssue, ParsedMetricRow,
    Vault, parse_metrics_file, parse_metrics_line, sqlite::IndexDatabase,
};

/// Patch semantics for update operations.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PatchValue<T> {
    /// Leave the existing value unchanged.
    #[default]
    Unchanged,
    /// Replace the value with the supplied content.
    Set(T),
    /// Clear the value entirely.
    Clear,
}

/// Input payload for metrics record creation.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricCreateRequest {
    /// Explicit target file. Falls back to the vault default write file.
    pub file: Option<PathBuf>,
    /// Optional stable id. When omitted Arrowhead generates one.
    pub id: Option<String>,
    /// Timestamp recorded for the metric event.
    pub ts: DateTime<FixedOffset>,
    /// Metric key.
    pub key: String,
    /// Numeric metric value.
    pub value: f64,
    /// Source that produced the metric.
    pub source: String,
    /// Optional day bucket.
    pub date: Option<NaiveDate>,
    /// Optional unit string.
    pub unit: Option<String>,
    /// Optional provenance id.
    pub origin_id: Option<String>,
    /// Optional note text.
    pub note: Option<String>,
    /// Optional structured context object.
    pub context: Option<Map<String, Value>>,
    /// Optional tags attached to the metric.
    pub tags: Vec<String>,
    /// Additional top-level fields to preserve in the NDJSON row.
    pub extra_fields: BTreeMap<String, Value>,
}

/// Input payload for metrics record updates.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MetricUpdateRequest {
    /// Stable id or `metric:<id>` reference of the record to mutate.
    pub metric_id: String,
    /// Optional replacement timestamp.
    pub ts: Option<DateTime<FixedOffset>>,
    /// Optional replacement metric key.
    pub key: Option<String>,
    /// Optional replacement numeric value.
    pub value: Option<f64>,
    /// Optional replacement source.
    pub source: Option<String>,
    /// Optional date patch.
    pub date: PatchValue<NaiveDate>,
    /// Optional unit patch.
    pub unit: PatchValue<String>,
    /// Optional origin id patch.
    pub origin_id: PatchValue<String>,
    /// Optional note patch.
    pub note: PatchValue<String>,
    /// Optional context patch.
    pub context: PatchValue<Map<String, Value>>,
    /// Optional full-tag replacement.
    pub tags: PatchValue<Vec<String>>,
}

impl MetricUpdateRequest {
    /// Returns `true` when at least one field would be modified.
    pub fn has_changes(&self) -> bool {
        self.ts.is_some()
            || self.key.is_some()
            || self.value.is_some()
            || self.source.is_some()
            || !matches!(self.date, PatchValue::Unchanged)
            || !matches!(self.unit, PatchValue::Unchanged)
            || !matches!(self.origin_id, PatchValue::Unchanged)
            || !matches!(self.note, PatchValue::Unchanged)
            || !matches!(self.context, PatchValue::Unchanged)
            || !matches!(self.tags, PatchValue::Unchanged)
    }
}

/// Structured result for a deleted metrics record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletedMetricRecord {
    /// Stable id of the removed record.
    pub metric_id: String,
    /// Source file that contained the removed row.
    pub source_file: PathBuf,
    /// 1-based source line number before deletion.
    pub source_line: usize,
}

/// Structured result for a created metrics file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedMetricFile {
    /// Relative path of the created file.
    pub relative_path: PathBuf,
}

/// Structured result for a renamed metrics file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenamedMetricFile {
    /// Previous relative file path.
    pub source_path: PathBuf,
    /// New relative file path.
    pub destination_path: PathBuf,
}

/// Structured result for a deleted metrics file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletedMetricFile {
    /// Relative path of the deleted file.
    pub relative_path: PathBuf,
    /// Number of non-empty rows that were present before deletion.
    pub row_count: usize,
}

/// Per-file summary for `assign-missing-ids`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignedMetricIdsFile {
    /// Relative path of the rewritten file.
    pub relative_path: PathBuf,
    /// Number of ids assigned in this file.
    pub assigned_count: usize,
}

/// Aggregate result for `assign-missing-ids`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssignedMetricIdsSummary {
    /// File-level assignment results.
    pub files: Vec<AssignedMetricIdsFile>,
    /// Total ids assigned across every processed file.
    pub total_assigned: usize,
}

/// Metrics mutation service backed by a vault and index database.
#[derive(Debug, Clone)]
pub struct MetricsMutationService {
    vault: Arc<Vault>,
    database: Arc<IndexDatabase>,
}

impl MetricsMutationService {
    /// Construct a mutation service using the supplied vault and database.
    pub fn new(vault: Arc<Vault>, database: Arc<IndexDatabase>) -> Self {
        Self { vault, database }
    }

    /// Create a new metrics record in the target NDJSON file and refresh the index.
    pub async fn create(&self, input: MetricCreateRequest) -> Result<MetricRecordEntry> {
        let vault = Arc::clone(&self.vault);
        let database = Arc::clone(&self.database);
        task::spawn_blocking(move || create_record(vault.as_ref(), database.as_ref(), input))
            .await
            .context("metrics create task aborted")?
    }

    /// Update an existing metrics record by stable id and refresh the index.
    pub async fn update(&self, input: MetricUpdateRequest) -> Result<MetricRecordEntry> {
        let vault = Arc::clone(&self.vault);
        let database = Arc::clone(&self.database);
        task::spawn_blocking(move || update_record(vault.as_ref(), database.as_ref(), input))
            .await
            .context("metrics update task aborted")?
    }

    /// Delete a metrics record by stable id and refresh the index.
    pub async fn delete(&self, metric_ref: &str) -> Result<DeletedMetricRecord> {
        let vault = Arc::clone(&self.vault);
        let database = Arc::clone(&self.database);
        let metric_ref = metric_ref.to_string();
        task::spawn_blocking(move || delete_record(vault.as_ref(), database.as_ref(), &metric_ref))
            .await
            .context("metrics delete task aborted")?
    }

    /// Create an empty metrics file and refresh the index entry.
    pub async fn create_file(&self, path: &Path) -> Result<CreatedMetricFile> {
        let vault = Arc::clone(&self.vault);
        let database = Arc::clone(&self.database);
        let path = path.to_path_buf();
        task::spawn_blocking(move || create_metrics_file(vault.as_ref(), database.as_ref(), &path))
            .await
            .context("metrics file create task aborted")?
    }

    /// Rename a metrics file and refresh its indexed location.
    pub async fn rename_file(
        &self,
        source: &Path,
        destination: &Path,
    ) -> Result<RenamedMetricFile> {
        let vault = Arc::clone(&self.vault);
        let database = Arc::clone(&self.database);
        let source = source.to_path_buf();
        let destination = destination.to_path_buf();
        task::spawn_blocking(move || {
            rename_metrics_file(vault.as_ref(), database.as_ref(), &source, &destination)
        })
        .await
        .context("metrics file rename task aborted")?
    }

    /// Delete a metrics file and remove its indexed rows.
    pub async fn delete_file(&self, path: &Path) -> Result<DeletedMetricFile> {
        let vault = Arc::clone(&self.vault);
        let database = Arc::clone(&self.database);
        let path = path.to_path_buf();
        task::spawn_blocking(move || delete_metrics_file(vault.as_ref(), database.as_ref(), &path))
            .await
            .context("metrics file delete task aborted")?
    }

    /// Assign generated ids to legacy rows that are missing them.
    pub async fn assign_missing_ids(
        &self,
        path: Option<&Path>,
    ) -> Result<AssignedMetricIdsSummary> {
        let vault = Arc::clone(&self.vault);
        let database = Arc::clone(&self.database);
        let path = path.map(Path::to_path_buf);
        task::spawn_blocking(move || {
            assign_missing_metric_ids(vault.as_ref(), database.as_ref(), path.as_deref())
        })
        .await
        .context("metrics assign-missing-ids task aborted")?
    }
}

#[derive(Debug, Clone)]
struct LocatedMetricRow {
    relative_path: PathBuf,
    absolute_path: PathBuf,
    rows: Vec<ParsedMetricRow>,
    row_index: usize,
}

fn create_record(
    vault: &Vault,
    database: &IndexDatabase,
    input: MetricCreateRequest,
) -> Result<MetricRecordEntry> {
    let (relative_path, absolute_path) = resolve_write_target(vault, input.file.as_deref())?;
    let metric_id = input.id.unwrap_or_else(generate_metric_id);
    ensure_metric_id_available(vault, &metric_id)?;

    let record = MetricRecord {
        id: metric_id.clone(),
        ts: input.ts,
        key: input.key,
        value: input.value,
        source: input.source,
        date: input.date,
        unit: trim_optional_string(input.unit),
        origin_id: trim_optional_string(input.origin_id),
        note: trim_optional_string(input.note),
        context: input.context,
        tags: normalise_tags(input.tags),
        extra_fields: input.extra_fields,
    };
    let raw_line = render_validated_metric_line(&record, &absolute_path)?;
    append_metric_line(&absolute_path, &raw_line)?;
    refresh_index_for_file(vault, database, &relative_path)?;

    database.metric_record_by_id(&metric_id)?.ok_or_else(|| {
        anyhow!("created metric `{metric_id}` did not appear in the refreshed index")
    })
}

fn update_record(
    vault: &Vault,
    database: &IndexDatabase,
    input: MetricUpdateRequest,
) -> Result<MetricRecordEntry> {
    if !input.has_changes() {
        bail!("metrics update requires at least one field change");
    }

    let metric_id = normalise_metric_reference(&input.metric_id)?;
    let mut located = locate_unique_metric_row(vault, &metric_id)?;
    let row = located.rows[located.row_index]
        .record
        .as_ref()
        .ok_or_else(|| {
            anyhow!(
                "metric `{metric_id}` cannot be updated because its row is not structurally valid"
            )
        })?;

    let mut updated = row.clone();
    if let Some(ts) = input.ts {
        updated.ts = ts;
    }
    if let Some(key) = input.key {
        updated.key = key;
    }
    if let Some(value) = input.value {
        updated.value = value;
    }
    if let Some(source) = input.source {
        updated.source = source;
    }
    updated.date = match input.date {
        PatchValue::Unchanged => updated.date,
        PatchValue::Set(value) => Some(value),
        PatchValue::Clear => None,
    };
    updated.unit = apply_optional_patch(updated.unit, input.unit);
    updated.origin_id = apply_optional_patch(updated.origin_id, input.origin_id);
    updated.note = apply_optional_patch(updated.note, input.note);
    updated.context = match input.context {
        PatchValue::Unchanged => updated.context,
        PatchValue::Set(value) => Some(value),
        PatchValue::Clear => None,
    };
    updated.tags = match input.tags {
        PatchValue::Unchanged => updated.tags,
        PatchValue::Set(value) => normalise_tags(value),
        PatchValue::Clear => Vec::new(),
    };

    let raw_line = render_validated_metric_line(&updated, &located.absolute_path)?;
    located.rows[located.row_index].raw_line = raw_line;
    write_rows_preserving_raw_lines(&located.absolute_path, &located.rows)?;
    refresh_index_for_file(vault, database, &located.relative_path)?;

    database.metric_record_by_id(&metric_id)?.ok_or_else(|| {
        anyhow!("updated metric `{metric_id}` did not appear in the refreshed index")
    })
}

fn delete_record(
    vault: &Vault,
    database: &IndexDatabase,
    metric_ref: &str,
) -> Result<DeletedMetricRecord> {
    let metric_id = normalise_metric_reference(metric_ref)?;
    let mut located = locate_unique_metric_row(vault, &metric_id)?;
    let source_line = located.rows[located.row_index].line_number;
    let source_file = located.relative_path.clone();
    located.rows.remove(located.row_index);
    write_rows_preserving_raw_lines(&located.absolute_path, &located.rows)?;
    refresh_index_for_file(vault, database, &located.relative_path)?;

    Ok(DeletedMetricRecord {
        metric_id,
        source_file,
        source_line,
    })
}

fn create_metrics_file(
    vault: &Vault,
    database: &IndexDatabase,
    requested_path: &Path,
) -> Result<CreatedMetricFile> {
    let (relative_path, absolute_path) = resolve_existing_metrics_file_path(vault, requested_path)?;
    if absolute_path.exists() {
        bail!("metrics file `{}` already exists", relative_path.display());
    }

    write_string_atomic(&absolute_path, "")?;
    refresh_index_for_file(vault, database, &relative_path)?;
    Ok(CreatedMetricFile { relative_path })
}

fn rename_metrics_file(
    vault: &Vault,
    database: &IndexDatabase,
    source_path: &Path,
    destination_path: &Path,
) -> Result<RenamedMetricFile> {
    let (source_relative, source_absolute) =
        resolve_existing_metrics_file_path(vault, source_path)?;
    let (destination_relative, destination_absolute) =
        resolve_existing_metrics_file_path(vault, destination_path)?;

    if source_relative == destination_relative {
        bail!("source and destination metrics files must be different");
    }
    if !source_absolute.exists() {
        bail!(
            "metrics file `{}` does not exist",
            source_relative.display()
        );
    }
    if destination_absolute.exists() {
        bail!(
            "metrics file `{}` already exists",
            destination_relative.display()
        );
    }

    if let Some(parent) = destination_absolute.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create metrics directory {}", parent.display()))?;
    }

    fs::rename(&source_absolute, &destination_absolute).with_context(|| {
        format!(
            "failed to rename metrics file {} to {}",
            source_absolute.display(),
            destination_absolute.display()
        )
    })?;
    database.remove_metrics_file(&source_relative.to_string_lossy())?;
    refresh_index_for_file(vault, database, &destination_relative)?;
    clean_empty_metrics_parents(vault, source_absolute.parent())?;

    Ok(RenamedMetricFile {
        source_path: source_relative,
        destination_path: destination_relative,
    })
}

fn delete_metrics_file(
    vault: &Vault,
    database: &IndexDatabase,
    requested_path: &Path,
) -> Result<DeletedMetricFile> {
    let (relative_path, absolute_path) = resolve_existing_metrics_file_path(vault, requested_path)?;
    if !absolute_path.exists() {
        bail!("metrics file `{}` does not exist", relative_path.display());
    }

    let row_count = parse_metrics_file(&absolute_path)?.len();
    fs::remove_file(&absolute_path)
        .with_context(|| format!("failed to delete metrics file {}", absolute_path.display()))?;
    database.remove_metrics_file(&relative_path.to_string_lossy())?;
    clean_empty_metrics_parents(vault, absolute_path.parent())?;

    Ok(DeletedMetricFile {
        relative_path,
        row_count,
    })
}

fn assign_missing_metric_ids(
    vault: &Vault,
    database: &IndexDatabase,
    requested_path: Option<&Path>,
) -> Result<AssignedMetricIdsSummary> {
    let targets = resolve_assign_id_targets(vault, requested_path)?;
    let mut used_ids = collect_present_metric_ids(vault)?;
    let mut files = Vec::new();
    let mut total_assigned = 0;

    for target in targets {
        let mut rows = parse_metrics_file(&target.absolute_path)?;
        let mut assigned_count = 0;

        for row in &mut rows {
            if let Some(updated_line) = assign_missing_id_to_raw_line(&row.raw_line, &mut used_ids)?
            {
                row.raw_line = updated_line;
                assigned_count += 1;
            }
        }

        if assigned_count > 0 {
            write_rows_preserving_raw_lines(&target.absolute_path, &rows)?;
            refresh_index_for_file(vault, database, &target.relative_path)?;
        }

        total_assigned += assigned_count;
        files.push(AssignedMetricIdsFile {
            relative_path: target.relative_path,
            assigned_count,
        });
    }

    Ok(AssignedMetricIdsSummary {
        files,
        total_assigned,
    })
}

fn resolve_write_target(vault: &Vault, requested: Option<&Path>) -> Result<(PathBuf, PathBuf)> {
    let metrics = vault.metrics_conventions();
    let relative_path = if let Some(path) = requested {
        vault.resolve_relative_metrics_path(path).ok_or_else(|| {
            anyhow!(
                "metrics file `{}` must live under `{}` and use a supported metrics extension",
                path.display(),
                metrics.root.display()
            )
        })?
    } else {
        metrics.default_write_file.clone()
    };

    Ok((relative_path.clone(), vault.note_path(&relative_path)))
}

fn resolve_existing_metrics_file_path(
    vault: &Vault,
    requested: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let metrics = vault.metrics_conventions();
    let relative_path = vault
        .resolve_relative_metrics_path(requested)
        .ok_or_else(|| {
            anyhow!(
                "metrics file `{}` must live under `{}` and use a supported metrics extension",
                requested.display(),
                metrics.root.display()
            )
        })?;
    let absolute_path = vault.note_path(&relative_path);
    Ok((relative_path, absolute_path))
}

fn locate_unique_metric_row(vault: &Vault, metric_id: &str) -> Result<LocatedMetricRow> {
    let matches = locate_metric_rows(vault, metric_id)?;
    match matches.len() {
        0 => bail!("metric `{metric_id}` was not found in any discovered metrics file"),
        1 => Ok(matches.into_iter().next().expect("single match")),
        _ => bail!(
            "metric `{metric_id}` is ambiguous because it appears multiple times: {}",
            describe_metric_locations(&matches)
        ),
    }
}

fn ensure_metric_id_available(vault: &Vault, metric_id: &str) -> Result<()> {
    let matches = locate_metric_rows(vault, metric_id)?;
    if matches.is_empty() {
        return Ok(());
    }

    bail!(
        "metric `{metric_id}` already exists at {}; choose a different id",
        describe_metric_locations(&matches)
    )
}

fn locate_metric_rows(vault: &Vault, metric_id: &str) -> Result<Vec<LocatedMetricRow>> {
    let mut matches = Vec::new();
    for file in vault.metrics_files()? {
        let rows = parse_metrics_file(&file.absolute_path)?;
        for (row_index, row) in rows.iter().enumerate() {
            if row.record.as_ref().map(|record| record.id.as_str()) == Some(metric_id) {
                matches.push(LocatedMetricRow {
                    relative_path: file.relative_path.clone(),
                    absolute_path: file.absolute_path.clone(),
                    rows: rows.clone(),
                    row_index,
                });
            }
        }
    }
    Ok(matches)
}

fn describe_metric_locations(matches: &[LocatedMetricRow]) -> String {
    matches
        .iter()
        .map(|entry| {
            let line = entry.rows[entry.row_index].line_number;
            format!("{}:{}", entry.relative_path.display(), line)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn append_metric_line(path: &Path, raw_line: &str) -> Result<()> {
    let mut content = if path.exists() {
        fs::read_to_string(path)
            .with_context(|| format!("failed to read metrics file {}", path.display()))?
    } else {
        String::new()
    };
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(raw_line);
    content.push('\n');
    write_string_atomic(path, &content)
}

fn resolve_assign_id_targets(
    vault: &Vault,
    requested_path: Option<&Path>,
) -> Result<Vec<crate::metrics::MetricsFileEntry>> {
    if let Some(path) = requested_path {
        let (relative_path, absolute_path) = resolve_existing_metrics_file_path(vault, path)?;
        if !absolute_path.exists() {
            bail!("metrics file `{}` does not exist", relative_path.display());
        }
        let metadata = fs::metadata(&absolute_path)
            .with_context(|| format!("failed to stat metrics file {}", absolute_path.display()))?;
        let file_modified_at = metadata
            .modified()
            .with_context(|| format!("failed to read metrics mtime {}", absolute_path.display()))?
            .into();
        return Ok(vec![crate::metrics::MetricsFileEntry {
            relative_path,
            absolute_path,
            file_modified_at,
        }]);
    }

    let targets = vault.metrics_files()?;
    if targets.is_empty() {
        bail!("no metrics files were discovered in this vault");
    }
    Ok(targets)
}

fn collect_present_metric_ids(vault: &Vault) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    for file in vault.metrics_files()? {
        for row in parse_metrics_file(&file.absolute_path)? {
            if let Some(id) = extract_present_metric_id(&row.raw_line) {
                ids.insert(id);
            }
        }
    }
    Ok(ids)
}

fn extract_present_metric_id(raw_line: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(raw_line).ok()?;
    let object = value.as_object()?;
    let id = object.get("id")?.as_str()?.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

fn assign_missing_id_to_raw_line(
    raw_line: &str,
    used_ids: &mut BTreeSet<String>,
) -> Result<Option<String>> {
    let parsed = match serde_json::from_str::<Value>(raw_line) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let Value::Object(mut object) = parsed else {
        return Ok(None);
    };

    let needs_id = match object.get("id") {
        None | Some(Value::Null) => true,
        Some(Value::String(value)) => value.trim().is_empty(),
        Some(_) => false,
    };
    if !needs_id {
        return Ok(None);
    }

    let id = generate_unique_metric_id(used_ids);
    object.insert("id".to_string(), Value::String(id));
    serde_json::to_string(&Value::Object(object))
        .map(Some)
        .context("failed to serialise metrics row with assigned id")
}

fn write_rows_preserving_raw_lines(path: &Path, rows: &[ParsedMetricRow]) -> Result<()> {
    let mut content = rows
        .iter()
        .map(|row| row.raw_line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !content.is_empty() {
        content.push('\n');
    }
    write_string_atomic(path, &content)
}

fn write_string_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create metrics directory {}", parent.display()))?;
    }

    let temp_name = format!(
        ".{}.arrowhead.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("metrics"),
        std::process::id()
    );
    let temp_path = path.with_file_name(temp_name);
    fs::write(&temp_path, content).with_context(|| {
        format!(
            "failed to write temporary metrics file {}",
            temp_path.display()
        )
    })?;
    fs::rename(&temp_path, path)
        .with_context(|| format!("failed to replace metrics file {}", path.display()))
}

fn refresh_index_for_file(
    vault: &Vault,
    database: &IndexDatabase,
    relative_path: &Path,
) -> Result<()> {
    let absolute_path = vault.note_path(relative_path);
    let rows = parse_metrics_file(&absolute_path)?;
    let file_modified_at: DateTime<Utc> = fs::metadata(&absolute_path)
        .with_context(|| format!("failed to inspect metrics file {}", absolute_path.display()))?
        .modified()
        .with_context(|| format!("failed to read metrics mtime {}", absolute_path.display()))?
        .into();
    database.upsert_metrics_file(
        &relative_path.to_string_lossy(),
        file_modified_at,
        &rows,
        Utc::now(),
    )
}

fn render_validated_metric_line(record: &MetricRecord, source_file: &Path) -> Result<String> {
    let raw_line = render_metric_record_line(record)?;
    let parsed = parse_metrics_line(&raw_line, source_file, 1);
    let errors = parsed
        .issues
        .iter()
        .filter(|issue| issue.severity == MetricIssueSeverity::Error)
        .collect::<Vec<_>>();
    if errors.is_empty() {
        return Ok(raw_line);
    }

    bail!(
        "metric row is invalid: {}",
        describe_validation_issues(&errors)
    )
}

fn render_metric_record_line(record: &MetricRecord) -> Result<String> {
    let mut object = Map::new();
    object.insert("id".to_string(), Value::String(record.id.clone()));
    object.insert("ts".to_string(), Value::String(record.ts.to_rfc3339()));
    if let Some(date) = record.date {
        object.insert("date".to_string(), Value::String(date.to_string()));
    }
    object.insert("key".to_string(), Value::String(record.key.clone()));
    object.insert(
        "value".to_string(),
        Value::Number(
            Number::from_f64(record.value)
                .ok_or_else(|| anyhow!("metric value must be a finite number"))?,
        ),
    );
    if let Some(unit) = record.unit.as_ref() {
        object.insert("unit".to_string(), Value::String(unit.clone()));
    }
    object.insert("source".to_string(), Value::String(record.source.clone()));
    if let Some(origin_id) = record.origin_id.as_ref() {
        object.insert("origin_id".to_string(), Value::String(origin_id.clone()));
    }
    if let Some(note) = record.note.as_ref() {
        object.insert("note".to_string(), Value::String(note.clone()));
    }
    if let Some(context) = record.context.as_ref() {
        object.insert("context".to_string(), Value::Object(context.clone()));
    }
    if !record.tags.is_empty() {
        object.insert(
            "tags".to_string(),
            Value::Array(
                record
                    .tags
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect::<Vec<_>>(),
            ),
        );
    }
    for (field, value) in &record.extra_fields {
        object.insert(field.clone(), value.clone());
    }

    serde_json::to_string(&Value::Object(object)).context("failed to serialise metric row")
}

fn apply_optional_patch<T>(current: Option<T>, patch: PatchValue<T>) -> Option<T> {
    match patch {
        PatchValue::Unchanged => current,
        PatchValue::Set(value) => Some(value),
        PatchValue::Clear => None,
    }
}

fn normalise_tags(tags: Vec<String>) -> Vec<String> {
    let mut tags = tags
        .into_iter()
        .filter_map(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect::<Vec<_>>();
    tags.sort();
    tags.dedup();
    tags
}

fn trim_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn normalise_metric_reference(metric_ref: &str) -> Result<String> {
    let trimmed = metric_ref.trim();
    if trimmed.is_empty() {
        bail!("metric id must not be empty");
    }
    Ok(trimmed
        .strip_prefix("metric:")
        .unwrap_or(trimmed)
        .trim()
        .to_string())
}

fn describe_validation_issues(issues: &[&MetricValidationIssue]) -> String {
    issues
        .iter()
        .map(|issue| {
            if let Some(field) = issue.field.as_deref() {
                format!("{} ({field})", issue.message)
            } else {
                issue.message.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn generate_metric_id() -> String {
    const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

    let timestamp_ms = Utc::now().timestamp_millis() as u64;
    let mut bytes = [0_u8; 16];
    bytes[0] = ((timestamp_ms >> 40) & 0xff) as u8;
    bytes[1] = ((timestamp_ms >> 32) & 0xff) as u8;
    bytes[2] = ((timestamp_ms >> 24) & 0xff) as u8;
    bytes[3] = ((timestamp_ms >> 16) & 0xff) as u8;
    bytes[4] = ((timestamp_ms >> 8) & 0xff) as u8;
    bytes[5] = (timestamp_ms & 0xff) as u8;
    rand::thread_rng().fill_bytes(&mut bytes[6..]);

    let mut value = u128::from_be_bytes(bytes);
    let mut output = ['0'; 26];
    for index in (0..26).rev() {
        output[index] = CROCKFORD_BASE32[(value & 0x1f) as usize] as char;
        value >>= 5;
    }
    output.iter().collect()
}

fn generate_unique_metric_id(used_ids: &mut BTreeSet<String>) -> String {
    loop {
        let candidate = generate_metric_id();
        if used_ids.insert(candidate.clone()) {
            return candidate;
        }
    }
}

fn clean_empty_metrics_parents(vault: &Vault, start: Option<&Path>) -> Result<()> {
    let metrics_root = vault.note_path(&vault.metrics_conventions().root);
    let Some(mut current) = start.map(Path::to_path_buf) else {
        return Ok(());
    };

    while current.starts_with(&metrics_root) && current != metrics_root {
        let is_empty = fs::read_dir(&current)
            .with_context(|| format!("failed to inspect metrics directory {}", current.display()))?
            .next()
            .is_none();
        if !is_empty {
            break;
        }
        fs::remove_dir(&current)
            .with_context(|| format!("failed to remove empty directory {}", current.display()))?;
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent.to_path_buf();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MetricsService, VaultConfig, sqlite::IndexDatabase};
    use tempfile::TempDir;

    fn build_service() -> (TempDir, MetricsMutationService, MetricsService) {
        let dir = TempDir::new().expect("tempdir");
        let vault_root = dir.path().join("vault");
        fs::create_dir_all(&vault_root).expect("create vault");

        let vault = Arc::new(Vault::new(VaultConfig::new(vault_root)).expect("vault"));
        vault.ensure_arrowhead_dirs().expect("arrowhead dirs");
        let database = Arc::new(
            IndexDatabase::open(vault.paths().arrowhead_dir.join("index.db")).expect("database"),
        );
        let mutation = MetricsMutationService::new(Arc::clone(&vault), Arc::clone(&database));
        let read = MetricsService::new(database);
        (dir, mutation, read)
    }

    #[tokio::test]
    async fn create_writes_to_default_file_and_indexes_record() {
        let (_dir, mutation, read) = build_service();
        let created = mutation
            .create(MetricCreateRequest {
                file: None,
                id: Some("01TESTCREATE00000000000000".to_string()),
                ts: DateTime::parse_from_rfc3339("2026-04-14T08:30:00+04:00").expect("ts"),
                key: "body.weight".to_string(),
                value: 105.6,
                source: "withings".to_string(),
                date: Some(NaiveDate::from_ymd_opt(2026, 4, 14).expect("date")),
                unit: Some("kg".to_string()),
                origin_id: None,
                note: Some("Morning weigh-in".to_string()),
                context: None,
                tags: vec!["health".to_string(), "weight".to_string()],
                extra_fields: BTreeMap::new(),
            })
            .await
            .expect("create record");

        assert_eq!(
            created.source_file,
            PathBuf::from("Metrics/All.metrics.ndjson")
        );
        let fetched = read
            .read_record("metric:01TESTCREATE00000000000000")
            .await
            .expect("read record")
            .expect("record present");
        assert_eq!(fetched.record.value, 105.6);
        assert_eq!(
            fetched.record.tags,
            vec!["health".to_string(), "weight".to_string()]
        );
    }

    #[tokio::test]
    async fn update_rewrites_existing_metric_record() {
        let (_dir, mutation, read) = build_service();
        let created = mutation
            .create(MetricCreateRequest {
                file: Some(PathBuf::from("Metrics/Body.metrics.ndjson")),
                id: Some("01TESTUPDATE00000000000000".to_string()),
                ts: DateTime::parse_from_rfc3339("2026-04-14T08:30:00+04:00").expect("ts"),
                key: "body.weight".to_string(),
                value: 105.6,
                source: "withings".to_string(),
                date: None,
                unit: Some("kg".to_string()),
                origin_id: None,
                note: None,
                context: None,
                tags: Vec::new(),
                extra_fields: BTreeMap::new(),
            })
            .await
            .expect("create");

        let updated = mutation
            .update(MetricUpdateRequest {
                metric_id: created.record.id.clone(),
                value: Some(104.9),
                note: PatchValue::Set("Corrected measurement".to_string()),
                ..MetricUpdateRequest::default()
            })
            .await
            .expect("update");

        assert_eq!(updated.record.value, 104.9);
        assert_eq!(
            updated.record.note.as_deref(),
            Some("Corrected measurement")
        );
        let fetched = read
            .read_record(&created.record.id)
            .await
            .expect("read updated")
            .expect("updated record present");
        assert_eq!(fetched.record.value, 104.9);
    }

    #[tokio::test]
    async fn delete_removes_metric_record_and_refreshes_index() {
        let (_dir, mutation, read) = build_service();
        let created = mutation
            .create(MetricCreateRequest {
                file: Some(PathBuf::from("Metrics/Body.metrics.ndjson")),
                id: Some("01TESTDELETE00000000000000".to_string()),
                ts: DateTime::parse_from_rfc3339("2026-04-14T08:30:00+04:00").expect("ts"),
                key: "body.weight".to_string(),
                value: 105.6,
                source: "withings".to_string(),
                date: None,
                unit: Some("kg".to_string()),
                origin_id: None,
                note: None,
                context: None,
                tags: Vec::new(),
                extra_fields: BTreeMap::new(),
            })
            .await
            .expect("create");

        let deleted = mutation
            .delete(&created.record.id)
            .await
            .expect("delete record");
        assert_eq!(deleted.metric_id, created.record.id);
        assert!(
            read.read_record(&deleted.metric_id)
                .await
                .expect("read after delete")
                .is_none()
        );
    }

    #[tokio::test]
    async fn duplicate_ids_make_delete_ambiguous() {
        let (_dir, mutation, _read) = build_service();
        let vault = mutation.vault.as_ref();
        let file_a = vault.note_path("Metrics/A.metrics.ndjson");
        let file_b = vault.note_path("Metrics/B.metrics.ndjson");
        fs::create_dir_all(file_a.parent().expect("metrics dir")).expect("create metrics dir");
        fs::write(
            &file_a,
            "{\"id\":\"01DUPLICATE00000000000000\",\"ts\":\"2026-04-14T08:30:00+04:00\",\"key\":\"body.weight\",\"value\":105.6,\"unit\":\"kg\",\"source\":\"withings\"}\n",
        )
        .expect("write file a");
        fs::write(
            &file_b,
            "{\"id\":\"01DUPLICATE00000000000000\",\"ts\":\"2026-04-15T08:30:00+04:00\",\"key\":\"body.weight\",\"value\":104.9,\"unit\":\"kg\",\"source\":\"withings\"}\n",
        )
        .expect("write file b");

        let err = mutation
            .delete("01DUPLICATE00000000000000")
            .await
            .expect_err("duplicate ids should block delete");
        assert!(
            err.to_string().contains("ambiguous"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn create_file_creates_empty_metrics_file_and_indexes_it() {
        let (_dir, mutation, read) = build_service();

        let created = mutation
            .create_file(Path::new("Metrics/Health.metrics.ndjson"))
            .await
            .expect("create file");

        assert_eq!(
            created.relative_path,
            PathBuf::from("Metrics/Health.metrics.ndjson")
        );
        assert!(
            mutation.vault.note_path(&created.relative_path).exists(),
            "created file should exist on disk"
        );

        let files = read.list_files().await.expect("list files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative_path, created.relative_path);
        assert_eq!(files[0].row_count, 0);
    }

    #[tokio::test]
    async fn rename_file_moves_metrics_file_and_updates_index() {
        let (_dir, mutation, read) = build_service();

        mutation
            .create(MetricCreateRequest {
                file: Some(PathBuf::from("Metrics/Legacy.metrics.ndjson")),
                id: Some("01TESTRENAME00000000000000".to_string()),
                ts: DateTime::parse_from_rfc3339("2026-04-14T08:30:00+04:00").expect("ts"),
                key: "body.weight".to_string(),
                value: 105.6,
                source: "withings".to_string(),
                date: None,
                unit: Some("kg".to_string()),
                origin_id: None,
                note: None,
                context: None,
                tags: Vec::new(),
                extra_fields: BTreeMap::new(),
            })
            .await
            .expect("seed file");

        let renamed = mutation
            .rename_file(
                Path::new("Metrics/Legacy.metrics.ndjson"),
                Path::new("Metrics/Body.metrics.ndjson"),
            )
            .await
            .expect("rename file");

        assert_eq!(
            renamed.source_path,
            PathBuf::from("Metrics/Legacy.metrics.ndjson")
        );
        assert_eq!(
            renamed.destination_path,
            PathBuf::from("Metrics/Body.metrics.ndjson")
        );
        assert!(
            !mutation
                .vault
                .note_path("Metrics/Legacy.metrics.ndjson")
                .exists(),
            "source file should be removed"
        );
        assert!(
            mutation
                .vault
                .note_path("Metrics/Body.metrics.ndjson")
                .exists(),
            "destination file should exist"
        );

        let files = read.list_files().await.expect("list files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative_path, renamed.destination_path);
        assert_eq!(
            read.read_record("01TESTRENAME00000000000000")
                .await
                .expect("read renamed record")
                .expect("record present")
                .source_file,
            PathBuf::from("Metrics/Body.metrics.ndjson")
        );
    }

    #[tokio::test]
    async fn delete_file_removes_metrics_file_and_index_rows() {
        let (_dir, mutation, read) = build_service();

        mutation
            .create(MetricCreateRequest {
                file: Some(PathBuf::from("Metrics/Legacy.metrics.ndjson")),
                id: Some("01TESTFILEDELETE0000000000".to_string()),
                ts: DateTime::parse_from_rfc3339("2026-04-14T08:30:00+04:00").expect("ts"),
                key: "body.weight".to_string(),
                value: 105.6,
                source: "withings".to_string(),
                date: None,
                unit: Some("kg".to_string()),
                origin_id: None,
                note: None,
                context: None,
                tags: Vec::new(),
                extra_fields: BTreeMap::new(),
            })
            .await
            .expect("seed file");

        let deleted = mutation
            .delete_file(Path::new("Metrics/Legacy.metrics.ndjson"))
            .await
            .expect("delete file");

        assert_eq!(
            deleted.relative_path,
            PathBuf::from("Metrics/Legacy.metrics.ndjson")
        );
        assert_eq!(deleted.row_count, 1);
        assert!(
            !mutation
                .vault
                .note_path("Metrics/Legacy.metrics.ndjson")
                .exists(),
            "deleted file should be removed from disk"
        );
        assert!(
            read.list_files().await.expect("list files").is_empty(),
            "deleted file should be removed from index"
        );
    }

    #[tokio::test]
    async fn assign_missing_ids_rewrites_missing_id_rows_and_refreshes_index() {
        let (_dir, mutation, read) = build_service();
        let path = mutation.vault.note_path("Metrics/Legacy.metrics.ndjson");
        fs::create_dir_all(path.parent().expect("metrics dir")).expect("create metrics dir");
        fs::write(
            &path,
            concat!(
                r#"{"ts":"2026-04-14T08:30:00+04:00","key":"body.weight","value":105.6,"unit":"kg","source":"withings"}"#,
                "\n",
                r#"{"id":"01HASID000000000000000000","ts":"2026-04-15T08:30:00+04:00","key":"body.weight","value":104.9,"unit":"kg","source":"withings"}"#,
                "\n"
            ),
        )
        .expect("write legacy file");

        let summary = mutation
            .assign_missing_ids(Some(Path::new("Metrics/Legacy.metrics.ndjson")))
            .await
            .expect("assign missing ids");

        assert_eq!(summary.total_assigned, 1);
        assert_eq!(summary.files.len(), 1);
        assert_eq!(summary.files[0].assigned_count, 1);

        let content = fs::read_to_string(&path).expect("read rewritten file");
        assert!(content.contains("\"id\":\"01HASID000000000000000000\""));
        assert!(
            content
                .lines()
                .next()
                .is_some_and(|line| line.contains("\"id\":\"")),
            "first legacy row should receive a generated id"
        );

        let files = read.list_files().await.expect("list files");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].record_count, 2);
    }
}
