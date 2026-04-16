//! Read-oriented metrics query helpers and service layer.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Months, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use tokio::task;

use crate::{
    MetricRecord, MetricValidationIssue, MetricValidationStatus,
    query::{
        DateRange, DateRangeBound, parse_absolute_date, parse_relative_range, range_from_lower,
        range_from_parsed_date, range_from_upper,
    },
    sqlite::IndexDatabase,
};

/// Default number of metrics search results to return when no limit is provided.
pub const DEFAULT_METRICS_SEARCH_LIMIT: usize = 10;

const METRICS_DATE_FORMAT_HINT: &str =
    "expected YYYY-MM-DD, YYYY-MM, or a range like YYYY-MM-DD..YYYY-MM-DD";

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
            return Ok(range_from_upper(parse_metrics_date_bound(
                upper,
                DateBoundKind::Upper,
                trimmed,
            )?));
        }

        if upper.is_empty() {
            return Ok(range_from_lower(parse_metrics_date_bound(
                lower,
                DateBoundKind::Lower,
                trimmed,
            )?));
        }

        let lower_range = range_from_lower(parse_metrics_date_bound(
            lower,
            DateBoundKind::Lower,
            trimmed,
        )?);
        let upper_range = range_from_upper(parse_metrics_date_bound(
            upper,
            DateBoundKind::Upper,
            trimmed,
        )?);
        return lower_range.intersect(&upper_range).with_context(|| {
            format!("metrics date filter `{trimmed}` resolves to an empty range")
        });
    }

    parse_metrics_date_literal(trimmed)
}

fn parse_metrics_date_literal(value: &str) -> Result<DateRange> {
    if let Some(range) = parse_metrics_month_range(value)? {
        return Ok(range);
    }

    let parsed = parse_absolute_date(value).with_context(|| {
        format!("invalid metrics date literal `{value}`: {METRICS_DATE_FORMAT_HINT}")
    })?;
    Ok(range_from_parsed_date(parsed))
}

fn parse_metrics_date_bound(
    value: &str,
    kind: DateBoundKind,
    original_input: &str,
) -> Result<DateRangeBound> {
    if let Some(bound) = parse_metrics_month_bound(value, kind)? {
        return Ok(bound);
    }

    let parsed = parse_absolute_date(value).with_context(|| {
        format!("invalid metrics date filter `{original_input}`: {METRICS_DATE_FORMAT_HINT}")
    })?;
    Ok(DateRangeBound {
        value: parsed.instant,
        inclusive: true,
    })
}

fn parse_metrics_month_range(value: &str) -> Result<Option<DateRange>> {
    let Some(start) = parse_metrics_month_start(value)? else {
        return Ok(None);
    };

    let end = start
        .checked_add_months(Months::new(1))
        .context("metrics month range overflow")?
        .checked_sub_signed(Duration::microseconds(1))
        .context("metrics month range underflow")?;

    Ok(Some(DateRange::new(
        Some(DateRangeBound {
            value: start,
            inclusive: true,
        }),
        Some(DateRangeBound {
            value: end,
            inclusive: true,
        }),
    )))
}

fn parse_metrics_month_bound(value: &str, kind: DateBoundKind) -> Result<Option<DateRangeBound>> {
    let Some(start) = parse_metrics_month_start(value)? else {
        return Ok(None);
    };

    let end = start
        .checked_add_months(Months::new(1))
        .context("metrics month range overflow")?
        .checked_sub_signed(Duration::microseconds(1))
        .context("metrics month range underflow")?;

    let bound = match kind {
        DateBoundKind::Lower => DateRangeBound {
            value: start,
            inclusive: true,
        },
        DateBoundKind::Upper => DateRangeBound {
            value: end,
            inclusive: true,
        },
    };

    Ok(Some(bound))
}

fn parse_metrics_month_start(value: &str) -> Result<Option<DateTime<Utc>>> {
    let Ok(date) = NaiveDate::parse_from_str(&format!("{value}-01"), "%Y-%m-%d") else {
        return Ok(None);
    };

    let naive = date
        .and_hms_opt(0, 0, 0)
        .context("invalid metrics month start")?;
    Ok(Some(Utc.from_utc_datetime(&naive)))
}

#[derive(Debug, Clone, Copy)]
enum DateBoundKind {
    Lower,
    Upper,
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
        task::spawn_blocking(move || database.search_metric_records(&parsed, Some(limit)))
            .await
            .context("metrics search task aborted")?
    }

    /// Search indexed metrics records without applying a result limit.
    pub async fn search_all(&self, query: &str) -> Result<Vec<MetricRecordEntry>> {
        let parsed = parse_metrics_query(query)?;
        let database = Arc::clone(&self.database);
        task::spawn_blocking(move || database.search_metric_records(&parsed, None))
            .await
            .context("metrics search task aborted")?
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::{parse_metrics_reader, sqlite::IndexDatabase};
    use chrono::TimeZone;
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

    #[test]
    fn parse_metrics_query_expands_month_shorthand_date_filters() {
        let query = parse_metrics_query("date:2026-04").expect("parse query");
        let range = query.date_range.expect("date range");
        let expected_start = Utc
            .with_ymd_and_hms(2026, 4, 1, 0, 0, 0)
            .single()
            .expect("start");
        let expected_end = Utc
            .with_ymd_and_hms(2026, 5, 1, 0, 0, 0)
            .single()
            .expect("end")
            - Duration::microseconds(1);

        assert_eq!(range.start.expect("start bound").value, expected_start);
        assert_eq!(range.end.expect("end bound").value, expected_end);
    }

    #[test]
    fn parse_metrics_query_reports_actionable_date_hints() {
        let err = parse_metrics_query("date:2026-04-99").expect_err("invalid date");
        assert!(
            err.to_string().contains(METRICS_DATE_FORMAT_HINT),
            "expected date hint in error: {err:#}"
        );
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
    async fn search_accepts_month_shorthand_date_filters() {
        let service = build_service(
            concat!(
                r#"{"id":"01AAA","ts":"2026-04-14T08:30:00+00:00","key":"nutrition.energy_intake","value":850,"unit":"kcal","source":"manual"}"#,
                "\n",
                r#"{"id":"01AAB","ts":"2026-05-01T08:30:00+00:00","key":"nutrition.energy_intake","value":900,"unit":"kcal","source":"manual"}"#
            ),
            "Metrics/health.metrics.ndjson",
        );

        let results = service
            .search("key:nutrition.energy_intake date:2026-04", Some(5))
            .await
            .expect("search with month shorthand");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].record.id, "01AAA");
    }

    #[tokio::test]
    async fn search_all_returns_every_matching_record() {
        let mut rows = Vec::new();
        for index in 0..12 {
            rows.push(format!(
                r#"{{"id":"01A{index:02}","ts":"2026-04-14T08:{index:02}:00+00:00","key":"nutrition.energy_intake","value":{value},"unit":"kcal","source":"manual","date":"2026-04-14"}}"#,
                value = 100 + index
            ));
        }
        let service = build_service(&rows.join("\n"), "Metrics/health.metrics.ndjson");

        let results = service
            .search_all("key:nutrition.energy_intake")
            .await
            .expect("search all");
        assert_eq!(results.len(), 12);
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
