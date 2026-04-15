//! Read-oriented metrics query helpers and service layer.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::task;

use crate::{
    MetricRecord, MetricValidationIssue, MetricValidationStatus,
    query::{
        DateRange, parse_absolute_date, parse_relative_range, range_from_lower,
        range_from_parsed_date, range_from_upper,
    },
    sqlite::IndexDatabase,
};

/// Default number of metrics search results to return when no limit is provided.
pub const DEFAULT_METRICS_SEARCH_LIMIT: usize = 10;

/// Summary information for an indexed metrics file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricFileSummary {
    /// Vault-relative metrics file path.
    pub relative_path: PathBuf,
    /// Filesystem modification timestamp stored in the index.
    pub file_modified_at: DateTime<Utc>,
    /// Timestamp when Arrowhead last indexed the file.
    pub indexed_at: DateTime<Utc>,
    /// Number of non-empty NDJSON rows encountered in the file.
    pub row_count: u64,
    /// Number of rows promoted into indexed metric records.
    pub record_count: u64,
    /// Total warning-level validation issues attached to the file.
    pub warning_count: u64,
    /// Total error-level validation issues attached to the file.
    pub error_count: u64,
}

/// Indexed metrics record together with source-location and validation data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricRecordEntry {
    /// Vault-relative metrics file containing the record.
    pub source_file: PathBuf,
    /// 1-based line number inside the source file.
    pub source_line: usize,
    /// Parsed record content.
    pub record: MetricRecord,
    /// Raw NDJSON row text.
    pub raw_line: String,
    /// Aggregate validation status for the indexed row.
    pub validation_status: MetricValidationStatus,
    /// Warning-level validation issues associated with the row.
    pub issues: Vec<MetricValidationIssue>,
}

/// Parsed metrics search query supporting a small fielded syntax.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MetricsQuery {
    /// Exact `key:` filters that must match.
    pub key_filters: Vec<String>,
    /// Exact `source:` filters that must match.
    pub source_filters: Vec<String>,
    /// Substring `file:` filters that must match the source path.
    pub file_filters: Vec<String>,
    /// Substring `note:` filters that must match the note field.
    pub note_filters: Vec<String>,
    /// Free-text terms searched across textual metrics fields.
    pub text_terms: Vec<String>,
    /// Optional date or timestamp range.
    pub date_range: Option<DateRange>,
}

/// Parse a metrics search query.
pub fn parse_metrics_query(input: &str) -> Result<MetricsQuery> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("empty metrics query");
    }

    let mut query = MetricsQuery::default();
    for token in tokenize_metrics_query(trimmed)? {
        if let Some((field, value)) = token.split_once(':') {
            let value = value.trim();
            if value.is_empty() {
                bail!("metrics query field `{field}` requires a value");
            }

            match field.to_ascii_lowercase().as_str() {
                "key" => query.key_filters.push(value.to_string()),
                "source" => query.source_filters.push(value.to_string()),
                "file" => query.file_filters.push(value.to_string()),
                "note" => query.note_filters.push(value.to_string()),
                "date" => merge_metrics_date_range(
                    &mut query.date_range,
                    parse_metrics_date_filter(value)?,
                )?,
                _ => query.text_terms.push(token),
            }
        } else {
            query.text_terms.push(token);
        }
    }

    if query.key_filters.is_empty()
        && query.source_filters.is_empty()
        && query.file_filters.is_empty()
        && query.note_filters.is_empty()
        && query.text_terms.is_empty()
        && query.date_range.is_none()
    {
        bail!("empty metrics query");
    }

    Ok(query)
}

fn tokenize_metrics_query(input: &str) -> Result<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;

    for ch in input.chars() {
        match quote {
            Some(active) if ch == active => {
                quote = None;
            }
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
            }
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }

    if let Some(active) = quote {
        bail!("unterminated {active} quote in metrics query");
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    Ok(tokens)
}

fn parse_metrics_date_filter(input: &str) -> Result<DateRange> {
    let trimmed = input.trim();
    if let Some(range) = parse_relative_range(trimmed, Utc::now())? {
        return Ok(range);
    }

    if let Some((lower, upper)) = trimmed.split_once("..") {
        let lower = lower.trim();
        let upper = upper.trim();

        if lower.is_empty() && upper.is_empty() {
            bail!("date filter `{trimmed}` must include at least one bound");
        }

        if lower.is_empty() {
            let parsed = parse_absolute_date(upper)
                .with_context(|| format!("invalid metrics date filter `{trimmed}`"))?;
            return Ok(range_from_upper(crate::query::DateRangeBound {
                value: parsed.instant,
                inclusive: true,
            }));
        }

        if upper.is_empty() {
            let parsed = parse_absolute_date(lower)
                .with_context(|| format!("invalid metrics date filter `{trimmed}`"))?;
            return Ok(range_from_lower(crate::query::DateRangeBound {
                value: parsed.instant,
                inclusive: true,
            }));
        }

        let lower_parsed = parse_absolute_date(lower)
            .with_context(|| format!("invalid metrics date filter `{trimmed}`"))?;
        let upper_parsed = parse_absolute_date(upper)
            .with_context(|| format!("invalid metrics date filter `{trimmed}`"))?;
        let lower_range = range_from_lower(crate::query::DateRangeBound {
            value: lower_parsed.instant,
            inclusive: true,
        });
        let upper_range = range_from_upper(crate::query::DateRangeBound {
            value: upper_parsed.instant,
            inclusive: true,
        });
        return lower_range.intersect(&upper_range).with_context(|| {
            format!("metrics date filter `{trimmed}` resolves to an empty range")
        });
    }

    let parsed = parse_absolute_date(trimmed)
        .with_context(|| format!("invalid metrics date `{trimmed}`"))?;
    Ok(range_from_parsed_date(parsed))
}

fn merge_metrics_date_range(target: &mut Option<DateRange>, next: DateRange) -> Result<()> {
    if let Some(current) = target.take() {
        *target = Some(
            current
                .intersect(&next)
                .context("metrics date filters exclude one another")?,
        );
    } else {
        *target = Some(next);
    }
    Ok(())
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

/// Read-oriented accessors for indexed metrics data.
#[derive(Debug, Clone)]
pub struct MetricsService {
    database: Arc<IndexDatabase>,
    default_limit: usize,
}

impl MetricsService {
    /// Construct a metrics service using the supplied database handle.
    pub fn new(database: Arc<IndexDatabase>) -> Self {
        Self {
            database,
            default_limit: DEFAULT_METRICS_SEARCH_LIMIT,
        }
    }

    /// List indexed metrics files.
    pub async fn list_files(&self) -> Result<Vec<MetricFileSummary>> {
        let database = Arc::clone(&self.database);
        task::spawn_blocking(move || database.list_metric_files())
            .await
            .context("metrics file list task aborted")?
    }

    /// Read a specific metric record by stable id or `metric:<id>` reference.
    pub async fn read_record(&self, metric_ref: &str) -> Result<Option<MetricRecordEntry>> {
        let database = Arc::clone(&self.database);
        let metric_id = normalise_metric_reference(metric_ref)?;
        task::spawn_blocking(move || database.metric_record_by_id(&metric_id))
            .await
            .context("metrics read task aborted")?
    }

    /// Search indexed metrics records.
    pub async fn search(
        &self,
        query: &str,
        limit: Option<usize>,
    ) -> Result<Vec<MetricRecordEntry>> {
        let parsed = parse_metrics_query(query)?;
        let database = Arc::clone(&self.database);
        let limit = limit.unwrap_or(self.default_limit).max(1);
        task::spawn_blocking(move || database.search_metric_records(&parsed, limit))
            .await
            .context("metrics search task aborted")?
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::{parse_metrics_reader, sqlite::IndexDatabase};
    use tempfile::TempDir;

    fn build_service(contents: &str, relative_path: &str) -> MetricsService {
        let dir = TempDir::new().expect("tempdir");
        let db = Arc::new(IndexDatabase::open(dir.path().join("index.db")).expect("open db"));
        let rows = parse_metrics_reader(
            Cursor::new(contents),
            PathBuf::from(relative_path).as_path(),
        )
        .expect("parse metrics rows");
        db.upsert_metrics_file(relative_path, Utc::now(), &rows, Utc::now())
            .expect("upsert metrics rows");
        Box::leak(Box::new(dir));
        MetricsService::new(db)
    }

    #[test]
    fn parse_metrics_query_supports_fields_and_quotes() {
        let query =
            parse_metrics_query(r#"key:body.weight source:withings date:past30d "steak dinner""#)
                .expect("parse query");
        assert_eq!(query.key_filters, vec!["body.weight".to_string()]);
        assert_eq!(query.source_filters, vec!["withings".to_string()]);
        assert_eq!(query.text_terms, vec!["steak dinner".to_string()]);
        assert!(query.date_range.is_some());
    }

    #[tokio::test]
    async fn read_record_normalises_metric_reference_prefix() {
        let service = build_service(
            r#"{"id":"01AAA","ts":"2026-04-14T08:30:00+00:00","key":"body.weight","value":105.6,"unit":"kg","source":"withings"}"#,
            "Metrics/health.metrics.ndjson",
        );

        let record = service
            .read_record("metric:01AAA")
            .await
            .expect("read metric record")
            .expect("metric record present");
        assert_eq!(record.record.id, "01AAA");
        assert_eq!(record.source_line, 1);
    }

    #[tokio::test]
    async fn search_matches_field_filters_and_free_text() {
        let service = build_service(
            concat!(
                r#"{"id":"01AAA","ts":"2026-04-14T08:30:00+00:00","key":"body.weight","value":105.6,"unit":"kg","source":"withings","note":"Morning weigh-in","tags":["health"]}"#,
                "\n",
                r#"{"id":"01AAB","ts":"2026-04-14T12:00:00+00:00","key":"nutrition.energy_intake","value":850,"unit":"kcal","source":"manual","note":"Steak dinner","tags":["food"]}"#
            ),
            "Metrics/health.metrics.ndjson",
        );

        let by_key = service
            .search("key:body.weight", Some(5))
            .await
            .expect("search by key");
        assert_eq!(by_key.len(), 1);
        assert_eq!(by_key[0].record.id, "01AAA");

        let by_text = service
            .search("\"steak dinner\"", Some(5))
            .await
            .expect("search by text");
        assert_eq!(by_text.len(), 1);
        assert_eq!(by_text[0].record.id, "01AAB");
    }

    #[tokio::test]
    async fn list_files_returns_indexed_file_summaries() {
        let service = build_service(
            r#"{"id":"01AAA","ts":"2026-04-14T08:30:00+00:00","key":"body.weight","value":105.6,"unit":"kg","source":"withings"}"#,
            "Metrics/health.metrics.ndjson",
        );

        let files = service.list_files().await.expect("list metrics files");
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].relative_path,
            PathBuf::from("Metrics/health.metrics.ndjson")
        );
        assert_eq!(files[0].record_count, 1);
    }
}
