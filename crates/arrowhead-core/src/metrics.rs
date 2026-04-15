//! Metrics conventions and discovery helpers.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Default relative directory used to store metrics files.
pub const DEFAULT_METRICS_ROOT: &str = "Metrics";
/// Default filename used for newly written metrics records.
pub const DEFAULT_METRICS_WRITE_FILE_NAME: &str = "All.metrics.ndjson";
/// Default metrics file suffix recognised by Arrowhead.
pub const DEFAULT_METRICS_EXTENSION: &str = ".metrics.ndjson";
/// Default prefix used when referencing metrics records from notes or tools.
pub const DEFAULT_METRIC_REFERENCE_PREFIX: &str = "metric:";
/// Default first day of the week when metrics tooling groups records.
pub const DEFAULT_WEEK_START_DAY: &str = "monday";
/// Default hour at which a new metrics day begins.
pub const DEFAULT_DAY_START_HOUR: u8 = 0;

/// Optional metrics-specific settings stored in `.arrowhead/workspace.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct MetricsConfigFile {
    /// Relative root directory that stores metrics files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// File suffixes that Arrowhead should treat as metrics files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
    /// Relative path used when a write target is not supplied explicitly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_write_file: Option<String>,
    /// Prefix used when generating record references such as `metric:<id>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_reference_prefix: Option<String>,
    /// Week start day for time-windowed metrics features.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub week_start_day: Option<String>,
    /// Hour offset that determines when a new metrics day starts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub day_start_hour: Option<u8>,
}

impl MetricsConfigFile {
    /// Returns `true` when the config does not override any metrics conventions.
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
            && self.extensions.is_empty()
            && self.default_write_file.is_none()
            && self.record_reference_prefix.is_none()
            && self.week_start_day.is_none()
            && self.day_start_hour.is_none()
    }
}

/// Where the resolved metrics conventions came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetricsConventionsSource {
    /// Loaded from `.obsidian/plugins/metrics-lens/data.json`.
    ObsidianPlugin(PathBuf),
    /// Loaded from `.arrowhead/workspace.toml`.
    ArrowheadWorkspace(PathBuf),
    /// Falling back to Arrowhead defaults.
    Default,
}

impl MetricsConventionsSource {
    /// Stable machine-readable identifier for the source.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ObsidianPlugin(_) => "obsidian-plugin",
            Self::ArrowheadWorkspace(_) => "arrowhead-workspace",
            Self::Default => "default",
        }
    }

    /// Filesystem path that backed the conventions, if any.
    pub fn path(&self) -> Option<&PathBuf> {
        match self {
            Self::ObsidianPlugin(path) | Self::ArrowheadWorkspace(path) => Some(path),
            Self::Default => None,
        }
    }
}

/// Fully resolved metrics conventions after applying precedence rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsConventions {
    /// Source that supplied the final metrics conventions.
    pub source: MetricsConventionsSource,
    /// Relative directory that stores metrics files.
    pub root: PathBuf,
    /// File suffixes recognised as metrics files.
    pub extensions: Vec<String>,
    /// Relative default write target for metrics mutations.
    pub default_write_file: PathBuf,
    /// Prefix used when referencing metrics records.
    pub record_reference_prefix: String,
    /// Week start day used by time-based metrics features.
    pub week_start_day: String,
    /// Hour offset that determines when a new metrics day starts.
    pub day_start_hour: u8,
}

/// Metrics file discovered inside the vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsFileEntry {
    /// Vault-relative file path.
    pub relative_path: PathBuf,
    /// Absolute filesystem path.
    pub absolute_path: PathBuf,
    /// Last modification timestamp reported by the filesystem.
    pub file_modified_at: DateTime<Utc>,
}

/// Severity associated with a metrics validation issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricIssueSeverity {
    /// Informational warning that does not make the row structurally invalid.
    Warning,
    /// Structural error that makes the row unsafe for mutation or indexing.
    Error,
}

/// Stable code identifying a validation problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricIssueCode {
    /// The NDJSON row could not be parsed as JSON.
    InvalidJson,
    /// The parsed JSON row was not an object.
    InvalidRowType,
    /// The `id` field is missing or invalid.
    InvalidId,
    /// The `ts` field is missing or invalid.
    InvalidTimestamp,
    /// The `key` field is missing or invalid.
    InvalidKey,
    /// The `value` field is missing or invalid.
    InvalidValue,
    /// The `source` field is missing or invalid.
    InvalidSource,
    /// The `date` field is present but invalid.
    InvalidDate,
    /// The `unit` field is present but invalid.
    InvalidUnit,
    /// The `origin_id` field is present but invalid.
    InvalidOriginId,
    /// The `note` field is present but invalid.
    InvalidNote,
    /// The `context` field is present but invalid.
    InvalidContext,
    /// The `tags` field is present but invalid.
    InvalidTags,
    /// The row includes a top-level key Arrowhead does not recognise.
    UnknownField,
    /// The metric key is not recognised by Arrowhead's initial registry.
    UnknownMetricKey,
    /// The unit is not recognised for the metric key.
    UnknownUnit,
    /// The supplied unit does not match known units for the metric key.
    UnitMismatch,
    /// The row duplicates a previously seen stable record id.
    DuplicateId,
    /// The row duplicates a previously seen origin id.
    DuplicateOriginId,
}

/// Validation issue attached to a parsed metrics row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricValidationIssue {
    /// Severity of the problem.
    pub severity: MetricIssueSeverity,
    /// Stable issue code.
    pub code: MetricIssueCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Field that triggered the issue, when applicable.
    pub field: Option<String>,
    /// Human-readable explanation.
    pub message: String,
}

impl MetricValidationIssue {
    fn warning(code: MetricIssueCode, field: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            severity: MetricIssueSeverity::Warning,
            code,
            field: field.map(str::to_string),
            message: message.into(),
        }
    }

    fn error(code: MetricIssueCode, field: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            severity: MetricIssueSeverity::Error,
            code,
            field: field.map(str::to_string),
            message: message.into(),
        }
    }
}

/// Aggregate validation status for a parsed metrics row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricValidationStatus {
    /// No issues were detected.
    Valid,
    /// Only warnings were detected.
    Warning,
    /// At least one structural error was detected.
    Invalid,
}

/// Typed metrics row parsed from an NDJSON record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricRecord {
    /// Stable record identifier.
    pub id: String,
    /// Timestamp recorded for the metric event.
    pub ts: DateTime<FixedOffset>,
    /// Metric key.
    pub key: String,
    /// Numeric value associated with the metric.
    pub value: f64,
    /// Source that produced the metric.
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional day bucket for the metric.
    pub date: Option<NaiveDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional unit.
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional provenance id.
    pub origin_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional human-authored note.
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional structured context object.
    pub context: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    /// Optional tags attached to the row.
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    /// Unrecognised top-level keys preserved for diagnostics.
    pub extra_fields: BTreeMap<String, Value>,
}

/// Parsed metrics row with validation issues and source location metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedMetricRow {
    /// Parsed row content when the line could be interpreted as a JSON object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<MetricRecord>,
    /// Raw NDJSON text for this row.
    pub raw_line: String,
    /// Source file that contained the row.
    pub source_file: PathBuf,
    /// 1-based line number in the source file.
    pub line_number: usize,
    /// Validation issues discovered while parsing or validating the row.
    pub issues: Vec<MetricValidationIssue>,
}

impl ParsedMetricRow {
    /// Returns the aggregate validation status for this row.
    pub fn validation_status(&self) -> MetricValidationStatus {
        if self
            .issues
            .iter()
            .any(|issue| issue.severity == MetricIssueSeverity::Error)
        {
            MetricValidationStatus::Invalid
        } else if self.issues.is_empty() {
            MetricValidationStatus::Valid
        } else {
            MetricValidationStatus::Warning
        }
    }

    /// Returns `true` when the row has at least one error-level issue.
    pub fn has_errors(&self) -> bool {
        self.validation_status() == MetricValidationStatus::Invalid
    }
}

/// Parse and validate every NDJSON row in the supplied metrics file.
pub fn parse_metrics_file(path: &Path) -> Result<Vec<ParsedMetricRow>> {
    let file = File::open(path)
        .with_context(|| format!("failed to open metrics file {}", path.display()))?;
    let reader = BufReader::new(file);
    parse_metrics_reader(reader, path)
}

/// Parse and validate every NDJSON row from a buffered reader.
pub fn parse_metrics_reader<R: BufRead>(
    reader: R,
    source_file: &Path,
) -> Result<Vec<ParsedMetricRow>> {
    let mut rows = Vec::new();

    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| {
            format!(
                "failed to read metrics file {} at line {}",
                source_file.display(),
                index + 1
            )
        })?;
        if line.trim().is_empty() {
            continue;
        }
        rows.push(parse_metrics_line(&line, source_file, index + 1));
    }

    apply_duplicate_issues(&mut rows);
    Ok(rows)
}

/// Parse and validate a single NDJSON row.
pub fn parse_metrics_line(
    raw_line: &str,
    source_file: &Path,
    line_number: usize,
) -> ParsedMetricRow {
    let parsed = match serde_json::from_str::<Value>(raw_line) {
        Ok(value) => value,
        Err(err) => {
            return ParsedMetricRow {
                record: None,
                raw_line: raw_line.to_string(),
                source_file: source_file.to_path_buf(),
                line_number,
                issues: vec![MetricValidationIssue::error(
                    MetricIssueCode::InvalidJson,
                    None,
                    format!("line is not valid JSON: {err}"),
                )],
            };
        }
    };

    let object = match parsed {
        Value::Object(map) => map,
        _ => {
            return ParsedMetricRow {
                record: None,
                raw_line: raw_line.to_string(),
                source_file: source_file.to_path_buf(),
                line_number,
                issues: vec![MetricValidationIssue::error(
                    MetricIssueCode::InvalidRowType,
                    None,
                    "metrics rows must be JSON objects",
                )],
            };
        }
    };

    let (record, issues) = parse_metric_object(object);
    ParsedMetricRow {
        record,
        raw_line: raw_line.to_string(),
        source_file: source_file.to_path_buf(),
        line_number,
        issues,
    }
}

fn parse_metric_object(
    mut object: Map<String, Value>,
) -> (Option<MetricRecord>, Vec<MetricValidationIssue>) {
    let mut issues = Vec::new();

    let id = parse_required_string(&mut object, "id", MetricIssueCode::InvalidId, &mut issues);
    let ts = parse_required_timestamp(
        &mut object,
        "ts",
        MetricIssueCode::InvalidTimestamp,
        &mut issues,
    );
    let key = parse_required_string(&mut object, "key", MetricIssueCode::InvalidKey, &mut issues);
    let value = parse_required_f64(
        &mut object,
        "value",
        MetricIssueCode::InvalidValue,
        &mut issues,
    );
    let source = parse_required_string(
        &mut object,
        "source",
        MetricIssueCode::InvalidSource,
        &mut issues,
    );
    let date = parse_optional_date(
        &mut object,
        "date",
        MetricIssueCode::InvalidDate,
        &mut issues,
    );
    let unit = parse_optional_string(
        &mut object,
        "unit",
        MetricIssueCode::InvalidUnit,
        &mut issues,
    );
    let origin_id = parse_optional_string(
        &mut object,
        "origin_id",
        MetricIssueCode::InvalidOriginId,
        &mut issues,
    );
    let note = parse_optional_string(
        &mut object,
        "note",
        MetricIssueCode::InvalidNote,
        &mut issues,
    );
    let context = parse_optional_object(
        &mut object,
        "context",
        MetricIssueCode::InvalidContext,
        &mut issues,
    );
    let tags = parse_optional_tags(
        &mut object,
        "tags",
        MetricIssueCode::InvalidTags,
        &mut issues,
    );

    let known_key = key.as_deref().and_then(known_units_for_key);
    if let Some(metric_key) = key.as_deref() {
        if known_key.is_none() {
            issues.push(MetricValidationIssue::warning(
                MetricIssueCode::UnknownMetricKey,
                Some("key"),
                format!("metric key `{metric_key}` is not in Arrowhead's known registry yet"),
            ));
        }
    }
    if let Some(unit_value) = unit.as_deref() {
        match (key.as_deref(), known_key) {
            (Some(metric_key), Some(known_units)) => {
                if !known_units.iter().any(|known| known == &unit_value) {
                    issues.push(MetricValidationIssue::warning(
                        MetricIssueCode::UnitMismatch,
                        Some("unit"),
                        format!(
                            "unit `{unit_value}` does not match known units for `{metric_key}` ({})",
                            known_units.join(", ")
                        ),
                    ));
                }
            }
            (Some(_), None) => {
                issues.push(MetricValidationIssue::warning(
                    MetricIssueCode::UnknownUnit,
                    Some("unit"),
                    format!(
                        "unit `{unit_value}` cannot be verified because the metric key is unknown"
                    ),
                ));
            }
            (None, _) => {}
        }
    }

    let mut extra_fields = BTreeMap::new();
    for (field, value) in object {
        issues.push(MetricValidationIssue::warning(
            MetricIssueCode::UnknownField,
            Some(&field),
            format!("unknown top-level field `{field}` will be preserved but ignored"),
        ));
        extra_fields.insert(field, value);
    }

    let record = match (id, ts, key, value, source) {
        (Some(id), Some(ts), Some(key), Some(value), Some(source)) => Some(MetricRecord {
            id,
            ts,
            key,
            value,
            source,
            date,
            unit,
            origin_id,
            note,
            context,
            tags,
            extra_fields,
        }),
        _ => None,
    };

    (record, issues)
}

fn parse_required_string(
    object: &mut Map<String, Value>,
    field: &'static str,
    code: MetricIssueCode,
    issues: &mut Vec<MetricValidationIssue>,
) -> Option<String> {
    match object.remove(field) {
        Some(Value::String(value)) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                issues.push(MetricValidationIssue::error(
                    code,
                    Some(field),
                    format!("field `{field}` must not be empty"),
                ));
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Some(Value::Null) | None => {
            issues.push(MetricValidationIssue::error(
                code,
                Some(field),
                format!("missing required field `{field}`"),
            ));
            None
        }
        Some(_) => {
            issues.push(MetricValidationIssue::error(
                code,
                Some(field),
                format!("field `{field}` must be a string"),
            ));
            None
        }
    }
}

fn parse_optional_string(
    object: &mut Map<String, Value>,
    field: &'static str,
    code: MetricIssueCode,
    issues: &mut Vec<MetricValidationIssue>,
) -> Option<String> {
    match object.remove(field) {
        Some(Value::String(value)) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Some(Value::Null) | None => None,
        Some(_) => {
            issues.push(MetricValidationIssue::error(
                code,
                Some(field),
                format!("field `{field}` must be a string when present"),
            ));
            None
        }
    }
}

fn parse_required_timestamp(
    object: &mut Map<String, Value>,
    field: &'static str,
    code: MetricIssueCode,
    issues: &mut Vec<MetricValidationIssue>,
) -> Option<DateTime<FixedOffset>> {
    let raw = parse_required_string(object, field, code, issues)?;
    match DateTime::parse_from_rfc3339(&raw) {
        Ok(timestamp) => Some(timestamp),
        Err(err) => {
            issues.push(MetricValidationIssue::error(
                code,
                Some(field),
                format!("field `{field}` must be an RFC 3339 timestamp: {err}"),
            ));
            None
        }
    }
}

fn parse_required_f64(
    object: &mut Map<String, Value>,
    field: &'static str,
    code: MetricIssueCode,
    issues: &mut Vec<MetricValidationIssue>,
) -> Option<f64> {
    match object.remove(field) {
        Some(Value::Number(number)) => number.as_f64().or_else(|| {
            issues.push(MetricValidationIssue::error(
                code,
                Some(field),
                format!("field `{field}` must be a finite JSON number"),
            ));
            None
        }),
        Some(Value::Null) | None => {
            issues.push(MetricValidationIssue::error(
                code,
                Some(field),
                format!("missing required field `{field}`"),
            ));
            None
        }
        Some(_) => {
            issues.push(MetricValidationIssue::error(
                code,
                Some(field),
                format!("field `{field}` must be numeric"),
            ));
            None
        }
    }
}

fn parse_optional_date(
    object: &mut Map<String, Value>,
    field: &'static str,
    code: MetricIssueCode,
    issues: &mut Vec<MetricValidationIssue>,
) -> Option<NaiveDate> {
    let raw = parse_optional_string(object, field, code, issues)?;
    match NaiveDate::parse_from_str(&raw, "%Y-%m-%d") {
        Ok(date) => Some(date),
        Err(err) => {
            issues.push(MetricValidationIssue::error(
                code,
                Some(field),
                format!("field `{field}` must use YYYY-MM-DD format: {err}"),
            ));
            None
        }
    }
}

fn parse_optional_object(
    object: &mut Map<String, Value>,
    field: &'static str,
    code: MetricIssueCode,
    issues: &mut Vec<MetricValidationIssue>,
) -> Option<Map<String, Value>> {
    match object.remove(field) {
        Some(Value::Object(value)) => Some(value),
        Some(Value::Null) | None => None,
        Some(_) => {
            issues.push(MetricValidationIssue::error(
                code,
                Some(field),
                format!("field `{field}` must be a JSON object when present"),
            ));
            None
        }
    }
}

fn parse_optional_tags(
    object: &mut Map<String, Value>,
    field: &'static str,
    code: MetricIssueCode,
    issues: &mut Vec<MetricValidationIssue>,
) -> Vec<String> {
    match object.remove(field) {
        Some(Value::Array(items)) => {
            let mut tags = Vec::new();
            let mut invalid = false;
            for item in items {
                match item {
                    Value::String(tag) => {
                        let trimmed = tag.trim();
                        if !trimmed.is_empty() {
                            tags.push(trimmed.to_string());
                        }
                    }
                    _ => invalid = true,
                }
            }
            if invalid {
                issues.push(MetricValidationIssue::error(
                    code,
                    Some(field),
                    format!("field `{field}` must be an array of strings"),
                ));
            }
            tags.sort();
            tags.dedup();
            tags
        }
        Some(Value::Null) | None => Vec::new(),
        Some(_) => {
            issues.push(MetricValidationIssue::error(
                code,
                Some(field),
                format!("field `{field}` must be an array of strings"),
            ));
            Vec::new()
        }
    }
}

fn apply_duplicate_issues(rows: &mut [ParsedMetricRow]) {
    let mut ids: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut origin_ids: BTreeMap<String, Vec<usize>> = BTreeMap::new();

    for (index, row) in rows.iter().enumerate() {
        let Some(record) = row.record.as_ref() else {
            continue;
        };
        ids.entry(record.id.clone()).or_default().push(index);
        if let Some(origin_id) = record.origin_id.as_ref() {
            origin_ids.entry(origin_id.clone()).or_default().push(index);
        }
    }

    for (id, indices) in ids {
        if indices.len() < 2 {
            continue;
        }
        for index in indices {
            rows[index].issues.push(MetricValidationIssue::error(
                MetricIssueCode::DuplicateId,
                Some("id"),
                format!("duplicate metric id `{id}` also appears elsewhere in the same file"),
            ));
        }
    }

    for (origin_id, indices) in origin_ids {
        if indices.len() < 2 {
            continue;
        }
        for index in indices {
            rows[index].issues.push(MetricValidationIssue::warning(
                MetricIssueCode::DuplicateOriginId,
                Some("origin_id"),
                format!(
                    "duplicate origin_id `{origin_id}` also appears elsewhere in the same file"
                ),
            ));
        }
    }
}

fn known_units_for_key(key: &str) -> Option<&'static [&'static str]> {
    match key {
        "body.weight" => Some(&["kg"]),
        "body.fat_percentage" => Some(&["%"]),
        "body.fat_mass" => Some(&["kg"]),
        "body.fat_free_mass" => Some(&["kg"]),
        "medication.semaglutide_dose" => Some(&["mg"]),
        "nutrition.energy_intake" => Some(&["kcal"]),
        "recovery.heart_rate_variability" => Some(&["ms"]),
        "recovery.oxygen_saturation" => Some(&["%"]),
        "recovery.respiratory_rate" => Some(&["br/min"]),
        "recovery.resting_heart_rate" => Some(&["bpm"]),
        "recovery.skin_temperature" => Some(&["Cel"]),
        "sleep.duration" => Some(&["min"]),
        "sleep.efficiency" => Some(&["%"]),
        "whoop.day_strain" => Some(&["score"]),
        "whoop.recovery_score" => Some(&["%"]),
        "whoop.sleep_performance" => Some(&["%"]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_metric_row() {
        let row = parse_metrics_line(
            r#"{"id":"01ABC","ts":"2026-04-14T08:30:00+04:00","date":"2026-04-14","key":"body.weight","value":105.6,"unit":"kg","source":"withings","origin_id":"withings:1","note":"Morning weigh-in","context":{"group_id":1},"tags":["health","weight"]}"#,
            Path::new("Metrics/withings.metrics.ndjson"),
            1,
        );

        assert_eq!(row.validation_status(), MetricValidationStatus::Valid);
        let record = row.record.expect("record parsed");
        assert_eq!(record.id, "01ABC");
        assert_eq!(record.key, "body.weight");
        assert_eq!(record.unit.as_deref(), Some("kg"));
        assert_eq!(
            record.tags,
            vec!["health".to_string(), "weight".to_string()]
        );
    }

    #[test]
    fn invalid_json_is_reported() {
        let row = parse_metrics_line("{not-json", Path::new("Metrics/test.metrics.ndjson"), 3);
        assert!(row.record.is_none());
        assert!(row.has_errors());
        assert_eq!(row.issues[0].code, MetricIssueCode::InvalidJson);
    }

    #[test]
    fn missing_required_fields_are_errors() {
        let row = parse_metrics_line(
            r#"{"ts":"2026-04-14T08:30:00+04:00","value":105.6}"#,
            Path::new("Metrics/test.metrics.ndjson"),
            1,
        );

        assert!(row.record.is_none());
        assert!(row.has_errors());
        assert!(
            row.issues
                .iter()
                .any(|issue| issue.code == MetricIssueCode::InvalidId)
        );
        assert!(
            row.issues
                .iter()
                .any(|issue| issue.code == MetricIssueCode::InvalidKey)
        );
        assert!(
            row.issues
                .iter()
                .any(|issue| issue.code == MetricIssueCode::InvalidSource)
        );
    }

    #[test]
    fn invalid_context_and_tags_are_reported() {
        let row = parse_metrics_line(
            r#"{"id":"01ABC","ts":"2026-04-14T08:30:00+04:00","key":"body.weight","value":105.6,"source":"withings","context":["bad"],"tags":["ok",4]}"#,
            Path::new("Metrics/test.metrics.ndjson"),
            1,
        );

        assert!(row.has_errors());
        assert!(
            row.issues
                .iter()
                .any(|issue| issue.code == MetricIssueCode::InvalidContext)
        );
        assert!(
            row.issues
                .iter()
                .any(|issue| issue.code == MetricIssueCode::InvalidTags)
        );
    }

    #[test]
    fn unknown_fields_and_keys_warn() {
        let row = parse_metrics_line(
            r#"{"id":"01ABC","ts":"2026-04-14T08:30:00+04:00","key":"nutrition.protein_intake","value":120,"source":"manual","unit":"g","mood":"good"}"#,
            Path::new("Metrics/test.metrics.ndjson"),
            1,
        );

        assert_eq!(row.validation_status(), MetricValidationStatus::Warning);
        assert!(
            row.issues
                .iter()
                .any(|issue| issue.code == MetricIssueCode::UnknownMetricKey)
        );
        assert!(
            row.issues
                .iter()
                .any(|issue| issue.code == MetricIssueCode::UnknownUnit)
        );
        let record = row.record.expect("record should still parse");
        assert!(record.extra_fields.contains_key("mood"));
    }

    #[test]
    fn unit_mismatch_warns_for_known_keys() {
        let row = parse_metrics_line(
            r#"{"id":"01ABC","ts":"2026-04-14T08:30:00+04:00","key":"body.weight","value":105.6,"source":"withings","unit":"lb"}"#,
            Path::new("Metrics/test.metrics.ndjson"),
            1,
        );

        assert_eq!(row.validation_status(), MetricValidationStatus::Warning);
        assert!(
            row.issues
                .iter()
                .any(|issue| issue.code == MetricIssueCode::UnitMismatch)
        );
    }

    #[test]
    fn duplicate_ids_and_origin_ids_are_detected() {
        let data = r#"{"id":"01ABC","ts":"2026-04-14T08:30:00+04:00","key":"body.weight","value":105.6,"source":"withings","origin_id":"same"}
{"id":"01ABC","ts":"2026-04-15T08:30:00+04:00","key":"body.weight","value":105.2,"source":"withings","origin_id":"same"}"#;
        let rows = parse_metrics_reader(
            BufReader::new(data.as_bytes()),
            Path::new("Metrics/test.metrics.ndjson"),
        )
        .expect("parse rows");

        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| {
            row.issues
                .iter()
                .any(|issue| issue.code == MetricIssueCode::DuplicateId)
        }));
        assert!(rows.iter().all(|row| {
            row.issues
                .iter()
                .any(|issue| issue.code == MetricIssueCode::DuplicateOriginId)
        }));
    }
}
