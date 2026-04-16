//! SQLite persistence layer for Arrowhead.

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::Path,
    sync::{
        Once,
        atomic::{AtomicI32, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{
    Connection, OptionalExtension, Transaction, params, params_from_iter, types::Value as SqlValue,
};
use serde_json::Value;
use tracing::{debug, info};

use crate::{
    MetadataMap, NoteRecord,
    graph::{LinkResolutionRecord, normalise_link_lookup},
    metadata::MetadataExtraction,
    metrics::{
        MetricIssueCode, MetricIssueSeverity, MetricRecord, MetricValidationIssue,
        MetricValidationStatus, ParsedMetricRow,
    },
    metrics_service::{MetricFileSummary, MetricRecordEntry, MetricsQuery},
    query::{QueryFilters, parse_absolute_date},
};

thread_local! {
    static THREAD_CONNECTIONS: RefCell<HashMap<usize, Vec<PooledConnection<SqliteConnectionManager>>>> =
        RefCell::new(HashMap::new());
}

static NEXT_DATABASE_ID: AtomicUsize = AtomicUsize::new(1);
static SQLITE_VEC_REGISTER: Once = Once::new();
static SQLITE_VEC_STATUS: AtomicI32 = AtomicI32::new(rusqlite::ffi::SQLITE_OK);

/// Current schema version for the Arrowhead index database.
const INDEX_SCHEMA_VERSION: i32 = 6;

/// Tracks existing index metadata for a note to drive staleness checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteIndexState {
    /// Filesystem modification timestamp stored in the index.
    pub file_modified_at: DateTime<Utc>,
    /// When the note was last indexed.
    pub indexed_at: DateTime<Utc>,
}

/// Indexed note row with timestamps useful for exploratory context queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedNoteRecord {
    /// Stable note identifier.
    pub id: String,
    /// Optional note title.
    pub title: Option<String>,
    /// Vault-relative note path.
    pub relative_path: String,
    /// Filesystem modification timestamp stored in the index.
    pub file_modified_at: DateTime<Utc>,
    /// When the note was last indexed.
    pub indexed_at: DateTime<Utc>,
    /// Filesystem creation timestamp when known.
    pub created_at: Option<DateTime<Utc>>,
}

/// Tracks existing index metadata for a metrics file to drive staleness checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricFileState {
    /// Filesystem modification timestamp stored in the index.
    pub file_modified_at: DateTime<Utc>,
    /// When the metrics file was last indexed.
    pub indexed_at: DateTime<Utc>,
}

/// Wrapper around a connection pool for the SQLite index database.
#[derive(Debug, Clone)]
pub struct IndexDatabase {
    pool: Pool<SqliteConnectionManager>,
    id: usize,
}

impl IndexDatabase {
    /// Open (and initialise) the database at the supplied path.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        debug!(path = %path.display(), "initialising SQLite index database");

        ensure_sqlite_vec_registered()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create database directory {}", parent.display())
            })?;
        }

        if path.exists() {
            let conn = Connection::open(&path).with_context(|| {
                format!("failed to inspect existing database {}", path.display())
            })?;
            let stored_version: i32 = conn
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .context("failed to read SQLite user_version pragma")?;

            if stored_version != INDEX_SCHEMA_VERSION {
                info!(
                    path = %path.display(),
                    stored_version,
                    expected_version = INDEX_SCHEMA_VERSION,
                    "rebuilding index database due to schema version mismatch"
                );
                drop(conn);
                fs::remove_file(&path).with_context(|| {
                    format!("failed to remove incompatible database {}", path.display())
                })?;
            }
        }

        let manager = SqliteConnectionManager::file(&path);
        let pool = Pool::builder()
            .max_size(32)
            .connection_timeout(Duration::from_secs(120))
            .build(manager)
            .context("failed to build SQLite connection pool")?;

        {
            let conn = pool
                .get()
                .context("failed to obtain SQLite connection for migrations")?;
            init_connection(&conn)?;
            apply_migrations(&conn)?;
        }

        let id = NEXT_DATABASE_ID.fetch_add(1, Ordering::SeqCst);
        debug!(
            path = %path.display(),
            database_id = id,
            "SQLite index database ready"
        );

        Ok(Self { pool, id })
    }

    /// Borrow a pooled SQLite connection.
    pub fn connection(&self) -> Result<PooledConnection<SqliteConnectionManager>> {
        self.pool
            .get()
            .context("failed to acquire SQLite connection from pool")
    }

    /// Borrow a connection scoped to the current thread, reusing it across calls.
    pub fn connection_for_thread(&self) -> Result<ThreadConnection> {
        if let Some(conn) = THREAD_CONNECTIONS.with(|cell| {
            let mut map = cell.borrow_mut();
            map.get_mut(&self.id).and_then(|stack| stack.pop())
        }) {
            return Ok(ThreadConnection {
                id: self.id,
                conn: Some(conn),
            });
        }

        let conn = self.connection()?;
        Ok(ThreadConnection {
            id: self.id,
            conn: Some(conn),
        })
    }

    /// Retrieve existing indexing state for a note.
    pub fn note_state(&self, note_id: &str) -> Result<Option<NoteIndexState>> {
        let conn = self.connection()?;
        let row = conn
            .query_row(
                "SELECT file_modified_at, indexed_at FROM notes WHERE id = ?1",
                [note_id],
                |row| {
                    let file_modified: i64 = row.get(0)?;
                    let indexed: i64 = row.get(1)?;
                    Ok((file_modified, indexed))
                },
            )
            .optional()
            .context("failed to query note indexing state")?;

        row.map(|(file_modified, indexed)| -> Result<_> {
            Ok(NoteIndexState {
                file_modified_at: from_micros(file_modified)?,
                indexed_at: from_micros(indexed)?,
            })
        })
        .transpose()
    }

    /// Retrieve indexing state for every note as a single lookup table.
    pub fn note_states(&self) -> Result<HashMap<String, NoteIndexState>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare("SELECT id, file_modified_at, indexed_at FROM notes")?;
        let mut rows = stmt.query([])?;
        let mut result = HashMap::new();

        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let file_modified: i64 = row.get(1)?;
            let indexed: i64 = row.get(2)?;
            result.insert(
                id,
                NoteIndexState {
                    file_modified_at: from_micros(file_modified)?,
                    indexed_at: from_micros(indexed)?,
                },
            );
        }

        Ok(result)
    }

    /// List indexed notes whose creation timestamp falls inside the supplied inclusive range.
    pub fn notes_created_between(
        &self,
        start_micros: i64,
        end_micros: i64,
    ) -> Result<Vec<IndexedNoteRecord>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, title, relative_path, file_modified_at, indexed_at, created_at
             FROM notes
             WHERE created_at IS NOT NULL
               AND created_at >= ?1
               AND created_at <= ?2
             ORDER BY created_at DESC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![start_micros, end_micros], |row| {
                let file_modified_at: i64 = row.get(3)?;
                let indexed_at: i64 = row.get(4)?;
                let created_at: Option<i64> = row.get(5)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    file_modified_at,
                    indexed_at,
                    created_at,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(
                |(id, title, relative_path, file_modified_at, indexed_at, created_at)| -> Result<_> {
                    Ok(IndexedNoteRecord {
                        id,
                        title,
                        relative_path,
                        file_modified_at: from_micros(file_modified_at)?,
                        indexed_at: from_micros(indexed_at)?,
                        created_at: created_at.map(from_micros).transpose()?,
                    })
                },
            )
            .collect()
    }

    /// List indexed notes whose modification timestamp falls inside the supplied inclusive range.
    pub fn notes_modified_between(
        &self,
        start_micros: i64,
        end_micros: i64,
    ) -> Result<Vec<IndexedNoteRecord>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT id, title, relative_path, file_modified_at, indexed_at, created_at
             FROM notes
             WHERE file_modified_at >= ?1
               AND file_modified_at <= ?2
             ORDER BY file_modified_at DESC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![start_micros, end_micros], |row| {
                let file_modified_at: i64 = row.get(3)?;
                let indexed_at: i64 = row.get(4)?;
                let created_at: Option<i64> = row.get(5)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    file_modified_at,
                    indexed_at,
                    created_at,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(
                |(id, title, relative_path, file_modified_at, indexed_at, created_at)| -> Result<_> {
                    Ok(IndexedNoteRecord {
                        id,
                        title,
                        relative_path,
                        file_modified_at: from_micros(file_modified_at)?,
                        indexed_at: from_micros(indexed_at)?,
                        created_at: created_at.map(from_micros).transpose()?,
                    })
                },
            )
            .collect()
    }

    /// Retrieve existing indexing state for a metrics file.
    pub fn metric_file_state(&self, relative_path: &str) -> Result<Option<MetricFileState>> {
        let conn = self.connection()?;
        let row = conn
            .query_row(
                "SELECT file_modified_at, indexed_at FROM metric_files WHERE relative_path = ?1",
                [relative_path],
                |row| {
                    let file_modified: i64 = row.get(0)?;
                    let indexed: i64 = row.get(1)?;
                    Ok((file_modified, indexed))
                },
            )
            .optional()
            .context("failed to query metrics file indexing state")?;

        row.map(|(file_modified, indexed)| -> Result<_> {
            Ok(MetricFileState {
                file_modified_at: from_micros(file_modified)?,
                indexed_at: from_micros(indexed)?,
            })
        })
        .transpose()
    }

    /// Retrieve indexing state for every metrics file as a single lookup table.
    pub fn metric_file_states(&self) -> Result<HashMap<String, MetricFileState>> {
        let conn = self.connection()?;
        let mut stmt =
            conn.prepare("SELECT relative_path, file_modified_at, indexed_at FROM metric_files")?;
        let mut rows = stmt.query([])?;
        let mut result = HashMap::new();

        while let Some(row) = rows.next()? {
            let relative_path: String = row.get(0)?;
            let file_modified: i64 = row.get(1)?;
            let indexed: i64 = row.get(2)?;
            result.insert(
                relative_path,
                MetricFileState {
                    file_modified_at: from_micros(file_modified)?,
                    indexed_at: from_micros(indexed)?,
                },
            );
        }

        Ok(result)
    }

    /// List indexed metrics files with stored validation counts.
    pub fn list_metric_files(&self) -> Result<Vec<MetricFileSummary>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT relative_path, file_modified_at, indexed_at, row_count, record_count, warning_count, error_count
             FROM metric_files
             ORDER BY relative_path ASC",
        )?;
        let mut rows = stmt.query([])?;
        let mut files = Vec::new();

        while let Some(row) = rows.next()? {
            let relative_path: String = row.get(0)?;
            let file_modified_at: i64 = row.get(1)?;
            let indexed_at: i64 = row.get(2)?;
            let row_count: i64 = row.get(3)?;
            let record_count: i64 = row.get(4)?;
            let warning_count: i64 = row.get(5)?;
            let error_count: i64 = row.get(6)?;
            files.push(MetricFileSummary {
                relative_path: relative_path.into(),
                file_modified_at: from_micros(file_modified_at)?,
                indexed_at: from_micros(indexed_at)?,
                row_count: row_count.max(0) as u64,
                record_count: record_count.max(0) as u64,
                warning_count: warning_count.max(0) as u64,
                error_count: error_count.max(0) as u64,
            });
        }

        Ok(files)
    }

    /// Load a single indexed metric record by stable id.
    pub fn metric_record_by_id(&self, id: &str) -> Result<Option<MetricRecordEntry>> {
        let conn = self.connection()?;
        let row = conn
            .query_row(
                "SELECT source_file, source_line FROM metric_records WHERE id = ?1",
                [id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .context("failed to query metric record")?;

        row.map(|(source_file, source_line)| {
            load_metric_record_entry(&conn, &source_file, source_line as usize)
        })
        .transpose()
    }

    /// Search indexed metric records using the supplied structured query.
    pub fn search_metric_records(
        &self,
        query: &MetricsQuery,
        limit: usize,
    ) -> Result<Vec<MetricRecordEntry>> {
        let conn = self.connection()?;
        let mut sql = String::from(
            "SELECT source_file, source_line
             FROM metric_records
             WHERE 1 = 1",
        );
        let mut params: Vec<SqlValue> = Vec::new();

        for key in &query.key_filters {
            sql.push_str(" AND LOWER(key) = ?");
            params.push(SqlValue::from(key.to_ascii_lowercase()));
        }

        for source in &query.source_filters {
            sql.push_str(" AND LOWER(source) = ?");
            params.push(SqlValue::from(source.to_ascii_lowercase()));
        }

        for file in &query.file_filters {
            sql.push_str(" AND LOWER(source_file) LIKE ?");
            params.push(SqlValue::from(like_pattern(file)));
        }

        for note in &query.note_filters {
            sql.push_str(" AND LOWER(COALESCE(note, '')) LIKE ?");
            params.push(SqlValue::from(like_pattern(note)));
        }

        if let Some(range) = &query.date_range {
            if let Some(start) = range.lower_bound_micros() {
                sql.push_str(" AND COALESCE(date_micros, ts_utc) >= ?");
                params.push(SqlValue::from(start));
            }
            if let Some(end) = range.upper_bound_micros() {
                sql.push_str(" AND COALESCE(date_micros, ts_utc) <= ?");
                params.push(SqlValue::from(end));
            }
        }

        for term in &query.text_terms {
            sql.push_str(
                " AND (
                    LOWER(COALESCE(note, '')) LIKE ?
                    OR LOWER(raw_line) LIKE ?
                    OR LOWER(source_file) LIKE ?
                    OR LOWER(COALESCE(tags_json, '')) LIKE ?
                    OR LOWER(COALESCE(context_json, '')) LIKE ?
                    OR LOWER(COALESCE(extra_fields_json, '')) LIKE ?
                )",
            );
            let pattern = like_pattern(term);
            for _ in 0..6 {
                params.push(SqlValue::from(pattern.clone()));
            }
        }

        sql.push_str(
            " ORDER BY COALESCE(date_micros, ts_utc) DESC, source_file ASC, source_line ASC LIMIT ?",
        );
        params.push(SqlValue::from(limit.max(1) as i64));

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params.iter()), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        let mut records = Vec::new();
        for row in rows {
            let (source_file, source_line) = row?;
            records.push(load_metric_record_entry(
                &conn,
                &source_file,
                source_line as usize,
            )?);
        }

        Ok(records)
    }

    /// List every note identifier stored in the index.
    pub fn list_note_ids(&self) -> Result<Vec<String>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare("SELECT id FROM notes ORDER BY id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;

        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }

        Ok(ids)
    }

    /// Return the number of notes currently stored in the index.
    pub fn note_count(&self) -> Result<u64> {
        let conn = self.connection()?;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
            .context("failed to count notes")?;
        Ok(count.max(0) as u64)
    }

    /// Load resolution hints to support WikiLink matching (titles and aliases).
    pub fn link_resolution_maps(&self) -> Result<LinkResolutionMaps> {
        let conn = self.connection()?;
        let mut maps = LinkResolutionMaps::default();

        {
            let mut stmt = conn.prepare("SELECT id, title FROM notes WHERE title IS NOT NULL")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let title: String = row.get(1)?;
                let key = normalise_link_lookup(&title);
                if !key.is_empty() {
                    maps.titles.entry(key).or_insert(id);
                }
            }
        }

        {
            let mut stmt =
                conn.prepare("SELECT note_id, value FROM metadata WHERE key = 'aliases'")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let note_id: String = row.get(0)?;
                let raw_value: String = row.get(1)?;
                match serde_json::from_str::<Value>(&raw_value)? {
                    Value::Array(items) => {
                        for item in items {
                            if let Value::String(alias) = item {
                                let key = normalise_link_lookup(&alias);
                                if key.is_empty() {
                                    continue;
                                }
                                let entry = maps.aliases.entry(key).or_default();
                                if !entry.iter().any(|existing| existing == &note_id) {
                                    entry.push(note_id.clone());
                                }
                            }
                        }
                    }
                    Value::Null => {}
                    _ => {
                        debug!(
                            note_id = %note_id,
                            "skipping non-array aliases metadata while building resolution maps"
                        );
                    }
                }
            }
        }

        Ok(maps)
    }

    /// Remove a note (and associated metadata) from the index.
    pub fn remove_note(&self, note_id: &str) -> Result<bool> {
        let mut conn = self.connection_for_thread()?;
        let tx = conn
            .transaction()
            .context("failed to start removal transaction")?;

        tx.execute("DELETE FROM notes_fts WHERE id = ?1", [note_id])
            .context("failed to remove note from FTS index")?;
        let affected = tx
            .execute("DELETE FROM notes WHERE id = ?1", [note_id])
            .context("failed to remove note row")?;

        if affected > 0 {
            tx.execute(
                "UPDATE note_links SET target_id = NULL WHERE target_id = ?1",
                [note_id],
            )
            .context("failed to clear backlinks for removed note")?;
        }

        tx.commit()
            .context("failed to commit removal transaction")?;
        Ok(affected > 0)
    }

    /// Replace all indexed metrics rows for a single file.
    pub fn upsert_metrics_file(
        &self,
        relative_path: &str,
        file_modified_at: DateTime<Utc>,
        rows: &[ParsedMetricRow],
        indexed_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut conn = self.connection_for_thread()?;
        let tx = conn.transaction().context("failed to start transaction")?;
        clear_metrics_file_rows(&tx, relative_path)?;

        let record_count = rows
            .iter()
            .filter(|row| row.record.is_some() && !row.has_errors())
            .count() as i64;
        let warning_count = rows
            .iter()
            .flat_map(|row| &row.issues)
            .filter(|issue| issue.severity == MetricIssueSeverity::Warning)
            .count() as i64;
        let error_count = rows
            .iter()
            .flat_map(|row| &row.issues)
            .filter(|issue| issue.severity == MetricIssueSeverity::Error)
            .count() as i64;

        tx.execute(
            "INSERT INTO metric_files (
                relative_path,
                file_modified_at,
                indexed_at,
                row_count,
                record_count,
                warning_count,
                error_count
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(relative_path) DO UPDATE SET
                file_modified_at = excluded.file_modified_at,
                indexed_at = excluded.indexed_at,
                row_count = excluded.row_count,
                record_count = excluded.record_count,
                warning_count = excluded.warning_count,
                error_count = excluded.error_count",
            params![
                relative_path,
                file_modified_at.timestamp_micros(),
                indexed_at.timestamp_micros(),
                rows.len() as i64,
                record_count,
                warning_count,
                error_count,
            ],
        )
        .context("failed to upsert metric file row")?;

        for row in rows {
            if !row.has_errors() {
                if let Some(record) = &row.record {
                    let context_json = record
                        .context
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()
                        .context("failed to serialise metric context")?;
                    let tags_json = if record.tags.is_empty() {
                        None
                    } else {
                        Some(
                            serde_json::to_string(&record.tags)
                                .context("failed to serialise metric tags")?,
                        )
                    };
                    let extra_fields_json = if record.extra_fields.is_empty() {
                        None
                    } else {
                        Some(
                            serde_json::to_string(&record.extra_fields)
                                .context("failed to serialise metric extra fields")?,
                        )
                    };
                    let date_micros = record.date.and_then(date_to_micros);

                    tx.execute(
                        "INSERT INTO metric_records (
                            source_file,
                            source_line,
                            id,
                            ts,
                            ts_utc,
                            key,
                            value,
                            source,
                            date,
                            date_micros,
                            unit,
                            origin_id,
                            note,
                            context_json,
                            tags_json,
                            raw_line,
                            validation_status,
                            extra_fields_json
                        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                        params![
                            relative_path,
                            row.line_number as i64,
                            &record.id,
                            record.ts.to_rfc3339(),
                            record.ts.with_timezone(&Utc).timestamp_micros(),
                            &record.key,
                            record.value,
                            &record.source,
                            record.date.map(|value| value.to_string()),
                            date_micros,
                            record.unit.as_deref(),
                            record.origin_id.as_deref(),
                            record.note.as_deref(),
                            context_json.as_deref(),
                            tags_json.as_deref(),
                            &row.raw_line,
                            metric_validation_status(row),
                            extra_fields_json.as_deref(),
                        ],
                    )
                    .context("failed to insert metric record row")?;
                }
            }

            for (ordinal, issue) in row.issues.iter().enumerate() {
                tx.execute(
                    "INSERT INTO metric_issues (
                        source_file,
                        source_line,
                        issue_ordinal,
                        severity,
                        code,
                        field,
                        message
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        relative_path,
                        row.line_number as i64,
                        ordinal as i64,
                        metric_issue_severity(issue.severity),
                        metric_issue_code(issue.code),
                        issue.field.as_deref(),
                        &issue.message,
                    ],
                )
                .context("failed to insert metric issue row")?;
            }
        }

        tx.commit()
            .context("failed to commit metrics indexing transaction")
    }

    /// Remove a metrics file and associated rows from the index.
    pub fn remove_metrics_file(&self, relative_path: &str) -> Result<bool> {
        let mut conn = self.connection_for_thread()?;
        let tx = conn
            .transaction()
            .context("failed to start metrics removal transaction")?;
        tx.execute(
            "DELETE FROM metric_links WHERE source_file = ?1",
            [relative_path],
        )
        .context("failed to remove metric links")?;
        tx.execute(
            "DELETE FROM metric_issues WHERE source_file = ?1",
            [relative_path],
        )
        .context("failed to remove metric issues")?;
        tx.execute(
            "DELETE FROM metric_records WHERE source_file = ?1",
            [relative_path],
        )
        .context("failed to remove metric records")?;
        let affected = tx
            .execute(
                "DELETE FROM metric_files WHERE relative_path = ?1",
                [relative_path],
            )
            .context("failed to remove metric file row")?;
        tx.commit()
            .context("failed to commit metrics removal transaction")?;
        Ok(affected > 0)
    }

    /// Upsert the supplied note content and metadata into the index.
    pub fn upsert_note(
        &self,
        note: &NoteRecord,
        extraction: &MetadataExtraction,
        resolved_links: &[LinkResolutionRecord],
        indexed_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut conn = self.connection_for_thread()?;
        let tx = conn.transaction().context("failed to start transaction")?;

        tx.execute(
            "INSERT INTO notes (id, title, relative_path, file_modified_at, indexed_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                 title = excluded.title,
                 relative_path = excluded.relative_path,
                 file_modified_at = excluded.file_modified_at,
                 indexed_at = excluded.indexed_at",
            params![
                &note.id,
                note.title.as_deref(),
                note.relative_path.to_string_lossy(),
                note.file_modified_at.timestamp_micros(),
                indexed_at.timestamp_micros(),
                note.created_at.map(|dt| dt.timestamp_micros())
            ],
        )
        .context("failed to upsert notes row")?;

        tx.execute("DELETE FROM metadata WHERE note_id = ?1", [&note.id])
            .context("failed to clear metadata rows")?;
        for (key, value) in &extraction.metadata {
            let json =
                serde_json::to_string(value).context("failed to serialize metadata value")?;
            tx.execute(
                "INSERT INTO metadata (note_id, key, value) VALUES (?1, ?2, ?3)",
                params![&note.id, key, json],
            )
            .context("failed to insert metadata row")?;
        }

        tx.execute("DELETE FROM metadata_dates WHERE note_id = ?1", [&note.id])
            .context("failed to clear metadata date rows")?;
        for (key, micros) in collect_metadata_date_values(&extraction.metadata) {
            tx.execute(
                "INSERT INTO metadata_dates (note_id, key, value) VALUES (?1, ?2, ?3)",
                params![&note.id, key, micros],
            )
            .context("failed to insert metadata date row")?;
        }

        tx.execute("DELETE FROM notes_fts WHERE id = ?1", [&note.id])
            .context("failed to remove stale FTS row")?;
        let fts_content = format_content_for_fts(note);
        let fts_metadata = format_metadata_for_fts(note, &extraction.metadata);
        tx.execute(
            "INSERT INTO notes_fts (id, content, metadata) VALUES (?1, ?2, ?3)",
            params![&note.id, fts_content, fts_metadata],
        )
        .context("failed to insert FTS row")?;

        tx.execute("DELETE FROM note_links WHERE source_id = ?1", [&note.id])
            .context("failed to clear existing note links")?;
        for link in resolved_links {
            tx.execute(
                "INSERT INTO note_links (source_id, target_id, raw_text, display_text, heading, reason, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    &note.id,
                    link.target.as_deref(),
                    &link.raw,
                    link.display.as_deref(),
                    link.heading.as_deref(),
                    link.reason.as_str(),
                    indexed_at.timestamp_micros()
                ],
            )
            .context("failed to insert note link")?;
        }

        tx.commit().context("failed to commit indexing transaction")
    }

    /// Execute a full-text search query against the FTS index without additional filters.
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<FtsMatch>> {
        self.search_fts_with_filters(query, limit, &QueryFilters::default())
    }

    /// Execute a full-text search query with optional filesystem and metadata filters.
    pub fn search_fts_with_filters(
        &self,
        query: &str,
        limit: usize,
        filters: &QueryFilters,
    ) -> Result<Vec<FtsMatch>> {
        let limit = limit.max(1) as i64;
        let conn = self.connection()?;

        let mut sql = String::from(
            "SELECT n.id, n.title, n.relative_path, bm25(notes_fts) AS rank,
                    snippet(notes_fts, 1, '[[', ']]', '...', 20) AS snippet
             FROM notes_fts
             JOIN notes n ON notes_fts.id = n.id
             WHERE notes_fts MATCH ?",
        );

        let mut params: Vec<SqlValue> = vec![SqlValue::from(query.to_string())];
        Self::append_filter_clauses(&mut sql, &mut params, filters);

        sql.push_str(" ORDER BY rank ASC LIMIT ?");
        params.push(SqlValue::from(limit));

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params.iter()), |row| {
            let snippet: Option<String> = row.get(4)?;
            Ok(FtsMatch {
                note_id: row.get(0)?,
                title: row.get(1)?,
                relative_path: row.get(2)?,
                rank: row.get(3)?,
                snippet: snippet.filter(|value| !value.trim().is_empty()),
            })
        })?;

        let mut matches = Vec::new();
        for row in rows {
            matches.push(row?);
        }

        Ok(matches)
    }

    /// Return the subset of `note_ids` that satisfy the supplied filters.
    pub fn filter_note_ids(
        &self,
        note_ids: &[String],
        filters: &QueryFilters,
    ) -> Result<HashSet<String>> {
        if filters.is_empty() || note_ids.is_empty() {
            return Ok(note_ids.iter().cloned().collect());
        }

        let conn = self.connection()?;
        let placeholders = vec!["?"; note_ids.len()].join(", ");
        let mut sql = format!("SELECT n.id FROM notes n WHERE n.id IN ({placeholders})");
        let mut params: Vec<SqlValue> = note_ids.iter().cloned().map(SqlValue::from).collect();

        Self::append_filter_clauses(&mut sql, &mut params, filters);
        sql.push_str(" GROUP BY n.id");

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params.iter()), |row| {
            row.get::<_, String>(0)
        })?;

        let mut allowed = HashSet::new();
        for row in rows {
            allowed.insert(row?);
        }

        Ok(allowed)
    }

    /// Retrieve note identifiers that match an FTS expression without limit.
    pub fn matching_note_ids(&self, query: &str) -> Result<HashSet<String>> {
        let conn = self.connection()?;
        let mut stmt = conn
            .prepare("SELECT n.id FROM notes_fts JOIN notes n ON notes_fts.id = n.id WHERE notes_fts MATCH ?")
            .context("failed to prepare FTS id query")?;
        let rows = stmt.query_map(params![query], |row| row.get::<_, String>(0))?;
        let mut ids = HashSet::new();
        for row in rows {
            ids.insert(row?);
        }
        Ok(ids)
    }

    /// Retrieve note identifiers that satisfy the supplied filters, ordered by modified time.
    pub fn notes_for_filters(&self, filters: &QueryFilters, limit: usize) -> Result<Vec<String>> {
        let mut sql = String::from("SELECT n.id FROM notes n WHERE 1 = 1");
        let mut params: Vec<SqlValue> = Vec::new();
        Self::append_filter_clauses(&mut sql, &mut params, filters);
        sql.push_str(" ORDER BY n.file_modified_at DESC, n.id ASC");

        let limit = limit.max(1) as i64;
        sql.push_str(" LIMIT ?");
        params.push(SqlValue::from(limit));

        let conn = self.connection()?;
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(params.iter()), |row| {
            row.get::<_, String>(0)
        })?;

        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }

        Ok(ids)
    }

    fn append_filter_clauses(sql: &mut String, params: &mut Vec<SqlValue>, filters: &QueryFilters) {
        if let Some(range) = &filters.modified {
            if let Some(start) = range.lower_bound_micros() {
                sql.push_str(" AND n.file_modified_at >= ?");
                params.push(SqlValue::from(start));
            }
            if let Some(end) = range.upper_bound_micros() {
                sql.push_str(" AND n.file_modified_at <= ?");
                params.push(SqlValue::from(end));
            }
        }

        if let Some(range) = &filters.created {
            if let Some(start) = range.lower_bound_micros() {
                sql.push_str(" AND n.created_at >= ?");
                params.push(SqlValue::from(start));
            }
            if let Some(end) = range.upper_bound_micros() {
                sql.push_str(" AND n.created_at <= ?");
                params.push(SqlValue::from(end));
            }
        }

        for (field, range) in &filters.metadata_dates {
            sql.push_str(" AND EXISTS (SELECT 1 FROM metadata_dates md WHERE md.note_id = n.id AND md.key = ?");
            params.push(SqlValue::from(field.clone()));

            if let Some(start) = range.lower_bound_micros() {
                sql.push_str(" AND md.value >= ?");
                params.push(SqlValue::from(start));
            }
            if let Some(end) = range.upper_bound_micros() {
                sql.push_str(" AND md.value <= ?");
                params.push(SqlValue::from(end));
            }

            sql.push(')');
        }
    }

    /// Load metadata maps for a collection of note identifiers.
    pub fn metadata_for_notes(&self, note_ids: &[String]) -> Result<HashMap<String, MetadataMap>> {
        let mut result: HashMap<String, MetadataMap> = HashMap::new();
        if note_ids.is_empty() {
            return Ok(result);
        }

        let conn = self.connection()?;
        let placeholders = vec!["?"; note_ids.len()].join(", ");
        let sql = format!(
            "SELECT note_id, key, value FROM metadata WHERE note_id IN ({})",
            placeholders
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(note_ids.iter().map(|id| id.as_str())),
            |row| {
                let note_id: String = row.get(0)?;
                let key: String = row.get(1)?;
                let raw_value: String = row.get(2)?;
                Ok((note_id, key, raw_value))
            },
        )?;

        for row in rows {
            let (note_id, key, raw_value) = row?;
            let value: Value = serde_json::from_str(&raw_value).with_context(|| {
                format!("failed to deserialize metadata value for {note_id}:{key}")
            })?;
            result.entry(note_id).or_default().insert(key, value);
        }

        Ok(result)
    }

    /// Retrieve the stored titles for the supplied note identifiers.
    pub fn titles_for_notes(&self, note_ids: &[String]) -> Result<HashMap<String, Option<String>>> {
        let mut result: HashMap<String, Option<String>> = HashMap::new();
        if note_ids.is_empty() {
            return Ok(result);
        }

        let conn = self.connection()?;
        let placeholders = vec!["?"; note_ids.len()].join(", ");
        let sql = format!("SELECT id, title FROM notes WHERE id IN ({})", placeholders);
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(note_ids.iter().map(|id| id.as_str())),
            |row| {
                let id: String = row.get(0)?;
                let title: Option<String> = row.get(1)?;
                Ok((id, title))
            },
        )?;

        for row in rows {
            let (id, title) = row?;
            result.insert(id, title);
        }

        Ok(result)
    }

    /// Resolve relative paths for the supplied note identifiers.
    pub fn relative_paths_for_notes(&self, note_ids: &[String]) -> Result<HashMap<String, String>> {
        let mut result: HashMap<String, String> = HashMap::new();
        if note_ids.is_empty() {
            return Ok(result);
        }

        let conn = self.connection()?;
        let placeholders = vec!["?"; note_ids.len()].join(", ");
        let sql = format!(
            "SELECT id, relative_path FROM notes WHERE id IN ({})",
            placeholders
        );

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(note_ids.iter().map(|id| id.as_str())),
            |row| {
                let id: String = row.get(0)?;
                let relative_path: String = row.get(1)?;
                Ok((id, relative_path))
            },
        )?;

        for row in rows {
            let (id, relative_path) = row?;
            result.insert(id, relative_path);
        }

        Ok(result)
    }

    /// Fetch a brief excerpt of the note content for preview purposes.
    pub fn note_excerpt(&self, note_id: &str, limit: usize) -> Result<Option<String>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare("SELECT content FROM notes_fts WHERE id = ?1")?;
        let content: Option<String> = stmt.query_row([note_id], |row| row.get(0)).optional()?;

        let excerpt = content.map(|text| {
            if text.len() > limit {
                let mut truncated = text.chars().take(limit).collect::<String>();
                if !truncated.ends_with("...") {
                    truncated.push_str("...");
                }
                truncated
            } else {
                text
            }
        });

        Ok(excerpt)
    }
}

fn init_connection(conn: &Connection) -> Result<()> {
    ensure_sqlite_vec_registered()?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .context("failed to set journal_mode")?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .context("failed to set synchronous")?;
    conn.pragma_update(None, "foreign_keys", true)
        .context("failed to enable foreign keys")?;
    conn.busy_timeout(Duration::from_secs(5))
        .context("failed to set busy_timeout")?;
    Ok(())
}

fn apply_migrations(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS notes (
            id TEXT PRIMARY KEY,
            title TEXT,
            relative_path TEXT NOT NULL,
            file_modified_at INTEGER NOT NULL,
            indexed_at INTEGER NOT NULL,
            created_at INTEGER
        );

        CREATE TABLE IF NOT EXISTS metadata (
            note_id TEXT NOT NULL,
            key TEXT NOT NULL,
            value TEXT NOT NULL,
            FOREIGN KEY(note_id) REFERENCES notes(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_metadata_note_key ON metadata(note_id, key);
        CREATE INDEX IF NOT EXISTS idx_metadata_key ON metadata(key);

        CREATE TABLE IF NOT EXISTS metadata_dates (
            note_id TEXT NOT NULL,
            key TEXT NOT NULL,
            value INTEGER NOT NULL,
            FOREIGN KEY(note_id) REFERENCES notes(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_metadata_dates_note_key ON metadata_dates(note_id, key);
        CREATE INDEX IF NOT EXISTS idx_metadata_dates_key_value ON metadata_dates(key, value, note_id);

        CREATE TABLE IF NOT EXISTS note_links (
            source_id TEXT NOT NULL,
            target_id TEXT,
            raw_text TEXT NOT NULL,
            display_text TEXT,
            heading TEXT,
            reason TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            FOREIGN KEY(source_id) REFERENCES notes(id) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_note_links_source ON note_links(source_id);
        CREATE INDEX IF NOT EXISTS idx_note_links_target ON note_links(target_id);

        CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(
            id UNINDEXED,
            content,
            metadata,
            tokenize = 'porter unicode61',
            columnsize = 0
        );

        CREATE TABLE IF NOT EXISTS embedding_metadata (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            model_id TEXT NOT NULL,
            repository TEXT NOT NULL,
            dimension INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS metric_files (
            relative_path TEXT PRIMARY KEY,
            file_modified_at INTEGER NOT NULL,
            indexed_at INTEGER NOT NULL,
            row_count INTEGER NOT NULL,
            record_count INTEGER NOT NULL,
            warning_count INTEGER NOT NULL,
            error_count INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS metric_records (
            source_file TEXT NOT NULL,
            source_line INTEGER NOT NULL,
            id TEXT NOT NULL,
            ts TEXT NOT NULL,
            ts_utc INTEGER NOT NULL,
            key TEXT NOT NULL,
            value REAL NOT NULL,
            source TEXT NOT NULL,
            date TEXT,
            date_micros INTEGER,
            unit TEXT,
            origin_id TEXT,
            note TEXT,
            context_json TEXT,
            tags_json TEXT,
            raw_line TEXT NOT NULL,
            validation_status TEXT NOT NULL,
            extra_fields_json TEXT,
            PRIMARY KEY (source_file, source_line),
            FOREIGN KEY(source_file) REFERENCES metric_files(relative_path) ON DELETE CASCADE
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_metric_records_id ON metric_records(id);
        CREATE INDEX IF NOT EXISTS idx_metric_records_origin_id ON metric_records(origin_id);
        CREATE INDEX IF NOT EXISTS idx_metric_records_key_ts ON metric_records(key, ts_utc DESC);
        CREATE INDEX IF NOT EXISTS idx_metric_records_source_ts ON metric_records(source, ts_utc DESC);
        CREATE INDEX IF NOT EXISTS idx_metric_records_date ON metric_records(date_micros, key);

        CREATE TABLE IF NOT EXISTS metric_issues (
            source_file TEXT NOT NULL,
            source_line INTEGER NOT NULL,
            issue_ordinal INTEGER NOT NULL,
            severity TEXT NOT NULL,
            code TEXT NOT NULL,
            field TEXT,
            message TEXT NOT NULL,
            PRIMARY KEY (source_file, source_line, issue_ordinal),
            FOREIGN KEY(source_file) REFERENCES metric_files(relative_path) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_metric_issues_severity ON metric_issues(severity, code);

        CREATE TABLE IF NOT EXISTS metric_links (
            source_file TEXT NOT NULL,
            source_line INTEGER NOT NULL,
            link_kind TEXT NOT NULL,
            target_kind TEXT NOT NULL,
            target_value TEXT NOT NULL,
            reason TEXT,
            confidence REAL,
            evidence_json TEXT,
            PRIMARY KEY (source_file, source_line, link_kind, target_kind, target_value),
            FOREIGN KEY(source_file, source_line) REFERENCES metric_records(source_file, source_line) ON DELETE CASCADE
        );

        CREATE INDEX IF NOT EXISTS idx_metric_links_target ON metric_links(target_kind, target_value);
        "#,
    )
    .context("failed to apply schema migrations")?;

    conn.pragma_update(None, "user_version", INDEX_SCHEMA_VERSION)
        .context("failed to set schema version")
}

fn ensure_sqlite_vec_registered() -> Result<()> {
    SQLITE_VEC_REGISTER.call_once(|| unsafe {
        type ExtensionEntry = unsafe extern "C" fn(
            *mut rusqlite::ffi::sqlite3,
            *mut *mut std::os::raw::c_char,
            *const rusqlite::ffi::sqlite3_api_routines,
        ) -> i32;

        let entry: ExtensionEntry = std::mem::transmute::<*const (), ExtensionEntry>(
            sqlite_vec::sqlite3_vec_init as *const (),
        );

        let rc = rusqlite::ffi::sqlite3_auto_extension(Some(entry));
        SQLITE_VEC_STATUS.store(rc, Ordering::SeqCst);
    });

    let rc = SQLITE_VEC_STATUS.load(Ordering::SeqCst);
    if rc != rusqlite::ffi::SQLITE_OK {
        Err(anyhow!(
            "failed to register sqlite-vec extension (sqlite rc {rc})"
        ))
    } else {
        Ok(())
    }
}

/// Connection handle tied to a worker thread for SQLite reuse.
pub struct ThreadConnection {
    id: usize,
    conn: Option<PooledConnection<SqliteConnectionManager>>,
}

impl std::ops::Deref for ThreadConnection {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.conn
            .as_ref()
            .expect("thread connection missing handle")
    }
}

impl std::ops::DerefMut for ThreadConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.conn
            .as_mut()
            .expect("thread connection missing handle")
    }
}

impl Drop for ThreadConnection {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            THREAD_CONNECTIONS.with(|cell| {
                let mut map = cell.borrow_mut();
                map.entry(self.id).or_default().push(conn);
            });
        }
    }
}

/// Result row produced by the FTS search query.
#[derive(Debug, Clone, PartialEq)]
pub struct FtsMatch {
    /// Identifier for the note.
    pub note_id: String,
    /// Optional note title.
    pub title: Option<String>,
    /// Relative path on disk.
    pub relative_path: String,
    /// BM25 rank (lower is better).
    pub rank: f64,
    /// Highlighted content snippet.
    pub snippet: Option<String>,
}

fn from_micros(micros: i64) -> Result<DateTime<Utc>> {
    let seconds = micros.div_euclid(1_000_000);
    let micros_part = micros.rem_euclid(1_000_000) as u32;
    Utc.timestamp_opt(seconds, micros_part * 1_000)
        .single()
        .context("invalid timestamp stored in database")
}

fn clear_metrics_file_rows(tx: &Transaction<'_>, relative_path: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM metric_links WHERE source_file = ?1",
        [relative_path],
    )
    .context("failed to clear metric links")?;
    tx.execute(
        "DELETE FROM metric_issues WHERE source_file = ?1",
        [relative_path],
    )
    .context("failed to clear metric issues")?;
    tx.execute(
        "DELETE FROM metric_records WHERE source_file = ?1",
        [relative_path],
    )
    .context("failed to clear metric records")?;
    tx.execute(
        "DELETE FROM metric_files WHERE relative_path = ?1",
        [relative_path],
    )
    .context("failed to clear metric file row")?;
    Ok(())
}

fn date_to_micros(date: NaiveDate) -> Option<i64> {
    date.and_hms_opt(0, 0, 0)
        .map(|value| value.and_utc().timestamp_micros())
}

fn metric_validation_status(row: &ParsedMetricRow) -> &'static str {
    match row.validation_status() {
        MetricValidationStatus::Valid => "valid",
        MetricValidationStatus::Warning => "warning",
        MetricValidationStatus::Invalid => "invalid",
    }
}

fn metric_issue_severity(severity: MetricIssueSeverity) -> &'static str {
    match severity {
        MetricIssueSeverity::Warning => "warning",
        MetricIssueSeverity::Error => "error",
    }
}

fn metric_issue_code(code: MetricIssueCode) -> &'static str {
    match code {
        MetricIssueCode::InvalidJson => "invalid_json",
        MetricIssueCode::InvalidRowType => "invalid_row_type",
        MetricIssueCode::InvalidId => "invalid_id",
        MetricIssueCode::InvalidTimestamp => "invalid_timestamp",
        MetricIssueCode::InvalidKey => "invalid_key",
        MetricIssueCode::InvalidValue => "invalid_value",
        MetricIssueCode::InvalidSource => "invalid_source",
        MetricIssueCode::InvalidDate => "invalid_date",
        MetricIssueCode::InvalidUnit => "invalid_unit",
        MetricIssueCode::InvalidOriginId => "invalid_origin_id",
        MetricIssueCode::InvalidNote => "invalid_note",
        MetricIssueCode::InvalidContext => "invalid_context",
        MetricIssueCode::InvalidTags => "invalid_tags",
        MetricIssueCode::UnknownField => "unknown_field",
        MetricIssueCode::UnknownMetricKey => "unknown_metric_key",
        MetricIssueCode::UnknownUnit => "unknown_unit",
        MetricIssueCode::UnitMismatch => "unit_mismatch",
        MetricIssueCode::DuplicateId => "duplicate_id",
        MetricIssueCode::DuplicateOriginId => "duplicate_origin_id",
    }
}

fn like_pattern(value: &str) -> String {
    format!("%{}%", value.trim().to_ascii_lowercase())
}

fn load_metric_record_entry(
    conn: &Connection,
    source_file: &str,
    source_line: usize,
) -> Result<MetricRecordEntry> {
    let row = conn
        .query_row(
            "SELECT id, ts, key, value, source, date, unit, origin_id, note, context_json, tags_json, raw_line, validation_status, extra_fields_json
             FROM metric_records
             WHERE source_file = ?1 AND source_line = ?2",
            params![source_file, source_line as i64],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<String>>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, Option<String>>(13)?,
                ))
            },
        )
        .with_context(|| {
            format!(
                "failed to load metric record {}:{}",
                source_file, source_line
            )
        })?;

    let (
        id,
        ts,
        key,
        value,
        source,
        date,
        unit,
        origin_id,
        note,
        context_json,
        tags_json,
        raw_line,
        validation_status,
        extra_fields_json,
    ) = row;

    let context = context_json
        .as_deref()
        .map(serde_json::from_str::<serde_json::Map<String, Value>>)
        .transpose()
        .with_context(|| {
            format!(
                "failed to parse stored metric context for {}:{}",
                source_file, source_line
            )
        })?;
    let tags = tags_json
        .as_deref()
        .map(serde_json::from_str::<Vec<String>>)
        .transpose()
        .with_context(|| {
            format!(
                "failed to parse stored metric tags for {}:{}",
                source_file, source_line
            )
        })?
        .unwrap_or_default();
    let extra_fields = extra_fields_json
        .as_deref()
        .map(serde_json::from_str::<BTreeMap<String, Value>>)
        .transpose()
        .with_context(|| {
            format!(
                "failed to parse stored metric extra fields for {}:{}",
                source_file, source_line
            )
        })?
        .unwrap_or_default();

    Ok(MetricRecordEntry {
        source_file: source_file.into(),
        source_line,
        record: MetricRecord {
            id,
            ts: DateTime::parse_from_rfc3339(&ts).with_context(|| {
                format!(
                    "failed to parse stored metric timestamp for {}:{}",
                    source_file, source_line
                )
            })?,
            key,
            value,
            source,
            date: date
                .as_deref()
                .map(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d"))
                .transpose()
                .with_context(|| {
                    format!(
                        "failed to parse stored metric date for {}:{}",
                        source_file, source_line
                    )
                })?,
            unit,
            origin_id,
            note,
            context,
            tags,
            extra_fields,
        },
        raw_line,
        validation_status: parse_metric_validation_status(&validation_status)?,
        issues: load_metric_issues(conn, source_file, source_line)?,
    })
}

fn load_metric_issues(
    conn: &Connection,
    source_file: &str,
    source_line: usize,
) -> Result<Vec<MetricValidationIssue>> {
    let mut stmt = conn.prepare(
        "SELECT severity, code, field, message
         FROM metric_issues
         WHERE source_file = ?1 AND source_line = ?2
         ORDER BY issue_ordinal ASC",
    )?;
    let rows = stmt.query_map(params![source_file, source_line as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;

    let mut issues = Vec::new();
    for row in rows {
        let (severity, code, field, message) = row?;
        issues.push(MetricValidationIssue {
            severity: parse_metric_issue_severity(&severity)?,
            code: parse_metric_issue_code(&code)?,
            field,
            message,
        });
    }
    Ok(issues)
}

fn parse_metric_validation_status(value: &str) -> Result<MetricValidationStatus> {
    match value {
        "valid" => Ok(MetricValidationStatus::Valid),
        "warning" => Ok(MetricValidationStatus::Warning),
        "invalid" => Ok(MetricValidationStatus::Invalid),
        _ => Err(anyhow!("unknown metric validation status `{value}`")),
    }
}

fn parse_metric_issue_severity(value: &str) -> Result<MetricIssueSeverity> {
    match value {
        "warning" => Ok(MetricIssueSeverity::Warning),
        "error" => Ok(MetricIssueSeverity::Error),
        _ => Err(anyhow!("unknown metric issue severity `{value}`")),
    }
}

fn parse_metric_issue_code(value: &str) -> Result<MetricIssueCode> {
    match value {
        "invalid_json" => Ok(MetricIssueCode::InvalidJson),
        "invalid_row_type" => Ok(MetricIssueCode::InvalidRowType),
        "invalid_id" => Ok(MetricIssueCode::InvalidId),
        "invalid_timestamp" => Ok(MetricIssueCode::InvalidTimestamp),
        "invalid_key" => Ok(MetricIssueCode::InvalidKey),
        "invalid_value" => Ok(MetricIssueCode::InvalidValue),
        "invalid_source" => Ok(MetricIssueCode::InvalidSource),
        "invalid_date" => Ok(MetricIssueCode::InvalidDate),
        "invalid_unit" => Ok(MetricIssueCode::InvalidUnit),
        "invalid_origin_id" => Ok(MetricIssueCode::InvalidOriginId),
        "invalid_note" => Ok(MetricIssueCode::InvalidNote),
        "invalid_context" => Ok(MetricIssueCode::InvalidContext),
        "invalid_tags" => Ok(MetricIssueCode::InvalidTags),
        "unknown_field" => Ok(MetricIssueCode::UnknownField),
        "unknown_metric_key" => Ok(MetricIssueCode::UnknownMetricKey),
        "unknown_unit" => Ok(MetricIssueCode::UnknownUnit),
        "unit_mismatch" => Ok(MetricIssueCode::UnitMismatch),
        "duplicate_id" => Ok(MetricIssueCode::DuplicateId),
        "duplicate_origin_id" => Ok(MetricIssueCode::DuplicateOriginId),
        _ => Err(anyhow!("unknown metric issue code `{value}`")),
    }
}

fn format_content_for_fts(note: &NoteRecord) -> String {
    let mut segments = Vec::new();
    segments.push(note.id.clone());

    if let Some(title) = &note.title {
        let trimmed = title.trim();
        if !trimmed.is_empty() {
            segments.push(trimmed.to_string());
        }
    }

    segments.push(note.content.clone());
    segments.join("\n\n")
}

fn format_metadata_for_fts(note: &NoteRecord, metadata: &MetadataMap) -> String {
    let mut parts = Vec::new();

    if let Some(title) = &note.title {
        push_metadata_tokens(&mut parts, "title", title);
    }

    for (key, value) in metadata {
        append_metadata_value(&mut parts, key, value);
    }

    parts.join(" ")
}

fn append_metadata_value(parts: &mut Vec<String>, key: &str, value: &Value) {
    match value {
        Value::Null => {}
        Value::Bool(bool_value) => {
            let token = if *bool_value { "true" } else { "false" };
            push_metadata_tokens(parts, key, token);
        }
        Value::Number(number) => push_metadata_tokens(parts, key, &number.to_string()),
        Value::String(string) => push_metadata_tokens(parts, key, string),
        Value::Array(items) => {
            for item in items {
                append_metadata_value(parts, key, item);
            }
        }
        Value::Object(map) => {
            if let Ok(serialised) = serde_json::to_string(value) {
                push_metadata_tokens(parts, key, &serialised);
            }
            for (nested_key, nested_value) in map {
                let nested = format!("{key}.{}", nested_key);
                append_metadata_value(parts, &nested, nested_value);
            }
        }
    }
}

fn push_metadata_tokens(parts: &mut Vec<String>, key: &str, raw_value: &str) {
    let trimmed = raw_value.trim();
    if trimmed.is_empty() {
        return;
    }

    parts.push(format!("{key}:{trimmed}"));
    parts.push(trimmed.to_string());
}

fn collect_metadata_date_values(metadata: &MetadataMap) -> Vec<(String, i64)> {
    let mut results = Vec::new();
    for (key, value) in metadata {
        collect_dates_from_value(key, value, &mut results);
    }
    results
}

fn collect_dates_from_value(key: &str, value: &Value, output: &mut Vec<(String, i64)>) {
    match value {
        Value::String(text) => {
            if let Ok(parsed) = parse_absolute_date(text) {
                output.push((key.to_string(), parsed.instant.timestamp_micros()));
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_dates_from_value(key, item, output);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Cursor,
        path::{Path, PathBuf},
    };

    use chrono::Utc;
    use tempfile::TempDir;

    use crate::{
        graph::{LinkReason, LinkResolutionRecord},
        metadata::MetadataExtractor,
        metrics::parse_metrics_reader,
        vault::{Vault, VaultConfig},
    };

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tests")
            .join("fixtures")
            .join("test-vault")
    }

    fn load_note(id: &str) -> NoteRecord {
        let vault =
            Vault::new(VaultConfig::new(fixture_root())).expect("fixture vault must initialise");
        vault
            .load_note(id)
            .unwrap_or_else(|_| panic!("expected note {id} to load"))
    }

    fn make_resolved_links(extraction: &MetadataExtraction) -> Vec<LinkResolutionRecord> {
        extraction
            .wikilinks
            .iter()
            .map(|link| LinkResolutionRecord {
                raw: link.raw.clone(),
                target: Some(link.target.clone()),
                display: link.display.clone(),
                heading: link.heading.clone(),
                reason: LinkReason::Direct,
            })
            .collect()
    }

    fn parse_metric_rows(contents: &str, relative_path: &str) -> Vec<ParsedMetricRow> {
        parse_metrics_reader(Cursor::new(contents), Path::new(relative_path))
            .expect("metrics rows parse")
    }

    #[test]
    fn creates_schema_on_open() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("index.db");
        let db = IndexDatabase::open(&db_path).expect("database opens");

        let conn = db.connection().expect("connection available");
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .expect("query tables");
        let tables: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .expect("iterate tables")
            .filter_map(Result::ok)
            .collect();

        assert!(tables.iter().any(|name| name == "notes"));
        assert!(tables.iter().any(|name| name == "metadata"));
        assert!(tables.iter().any(|name| name == "notes_fts"));
        assert!(tables.iter().any(|name| name == "metric_files"));
        assert!(tables.iter().any(|name| name == "metric_records"));
        assert!(tables.iter().any(|name| name == "metric_issues"));
        assert!(tables.iter().any(|name| name == "metric_links"));
    }

    #[test]
    fn upsert_metrics_file_persists_records_and_issues() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");
        let relative_path = "Metrics/withings.metrics.ndjson";
        let rows = parse_metric_rows(
            concat!(
                r#"{"id":"01AAA","ts":"2026-04-14T08:30:00+00:00","date":"2026-04-14","key":"body.weight","value":105.6,"unit":"kg","source":"withings","origin_id":"withings:1","note":"Morning weigh-in","context":{"device":"scale"},"tags":["health","weight"]}"#,
                "\n",
                r#"{"id":"01AAB","ts":"2026-04-14T09:00:00+00:00","key":"body.weight","value":105.2,"unit":"kg","source":"withings","extra":"kept"}"#,
                "\n",
                r##"{"id":"01AAC","ts":"2026-04-14T10:00:00+00:00","value":104.9,"source":"withings"}"##
            ),
            relative_path,
        );
        let file_modified_at = Utc::now();
        let indexed_at = Utc::now();

        db.upsert_metrics_file(relative_path, file_modified_at, &rows, indexed_at)
            .expect("upsert succeeds");

        let conn = db.connection().expect("connection");
        let file_row: (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT row_count, record_count, warning_count, error_count
                 FROM metric_files WHERE relative_path = ?1",
                [relative_path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("fetch metric file row");
        assert_eq!(file_row, (3, 2, 1, 1));

        let record_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM metric_records", [], |row| row.get(0))
            .expect("count metric records");
        assert_eq!(record_count, 2);

        let first_record: (String, Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT validation_status, tags_json, context_json, extra_fields_json
                 FROM metric_records
                 WHERE source_file = ?1 AND source_line = 1",
                [relative_path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("fetch first metric record");
        assert_eq!(first_record.0, "valid");
        assert_eq!(first_record.1.as_deref(), Some(r#"["health","weight"]"#));
        assert_eq!(first_record.2.as_deref(), Some(r#"{"device":"scale"}"#));
        assert!(first_record.3.is_none());

        let second_record: (String, Option<String>) = conn
            .query_row(
                "SELECT validation_status, extra_fields_json
                 FROM metric_records
                 WHERE source_file = ?1 AND source_line = 2",
                [relative_path],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("fetch second metric record");
        assert_eq!(second_record.0, "warning");
        assert_eq!(second_record.1.as_deref(), Some(r#"{"extra":"kept"}"#));

        let issues: Vec<(i64, String, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT source_line, severity, code
                     FROM metric_issues
                     WHERE source_file = ?1
                     ORDER BY source_line, issue_ordinal",
                )
                .expect("prepare metric issue query");
            let rows = stmt
                .query_map([relative_path], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .expect("iterate metric issues");
            rows.map(|row| row.expect("issue row")).collect()
        };
        assert_eq!(
            issues,
            vec![
                (2, "warning".to_string(), "unknown_field".to_string()),
                (3, "error".to_string(), "invalid_key".to_string()),
            ]
        );

        let state = db
            .metric_file_state(relative_path)
            .expect("metric file state query")
            .expect("metric file state present");
        assert_eq!(
            state.file_modified_at.timestamp_micros(),
            file_modified_at.timestamp_micros()
        );
        assert_eq!(
            state.indexed_at.timestamp_micros(),
            indexed_at.timestamp_micros()
        );
    }

    #[test]
    fn upsert_metrics_file_replaces_existing_rows() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");
        let relative_path = "Metrics/withings.metrics.ndjson";
        let initial_rows = parse_metric_rows(
            r#"{"id":"01AAA","ts":"2026-04-14T08:30:00+00:00","key":"body.weight","value":105.6,"unit":"kg","source":"withings","extra":"kept"}"#,
            relative_path,
        );
        db.upsert_metrics_file(relative_path, Utc::now(), &initial_rows, Utc::now())
            .expect("first upsert succeeds");

        let replacement_rows = parse_metric_rows(
            r#"{"id":"01BBB","ts":"2026-04-15T08:30:00+00:00","key":"body.weight","value":104.4,"unit":"kg","source":"withings"}"#,
            relative_path,
        );
        db.upsert_metrics_file(relative_path, Utc::now(), &replacement_rows, Utc::now())
            .expect("second upsert succeeds");

        let conn = db.connection().expect("connection");
        let ids: Vec<String> = {
            let mut stmt = conn
                .prepare(
                    "SELECT id FROM metric_records
                     WHERE source_file = ?1
                     ORDER BY source_line",
                )
                .expect("prepare metric record query");
            let rows = stmt
                .query_map([relative_path], |row| row.get(0))
                .expect("iterate metric record ids");
            rows.map(|row| row.expect("metric record id")).collect()
        };
        assert_eq!(ids, vec!["01BBB".to_string()]);

        let issue_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM metric_issues WHERE source_file = ?1",
                [relative_path],
                |row| row.get(0),
            )
            .expect("count metric issues");
        assert_eq!(issue_count, 0);

        let counts: (i64, i64, i64) = conn
            .query_row(
                "SELECT row_count, record_count, warning_count
                 FROM metric_files WHERE relative_path = ?1",
                [relative_path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("fetch metric file counts");
        assert_eq!(counts, (1, 1, 0));
    }

    #[test]
    fn upsert_metrics_file_skips_invalid_duplicate_records() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");
        let relative_path = "Metrics/withings.metrics.ndjson";
        let rows = parse_metric_rows(
            concat!(
                r#"{"id":"01AAA","ts":"2026-04-14T08:30:00+00:00","key":"body.weight","value":105.6,"unit":"kg","source":"withings"}"#,
                "\n",
                r#"{"id":"01AAA","ts":"2026-04-14T09:00:00+00:00","key":"body.weight","value":105.1,"unit":"kg","source":"withings"}"#
            ),
            relative_path,
        );

        db.upsert_metrics_file(relative_path, Utc::now(), &rows, Utc::now())
            .expect("upsert succeeds");

        let conn = db.connection().expect("connection");
        let counts: (i64, i64, i64) = conn
            .query_row(
                "SELECT row_count, record_count, error_count
                 FROM metric_files WHERE relative_path = ?1",
                [relative_path],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("fetch metric file counts");
        assert_eq!(counts, (2, 0, 2));

        let record_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM metric_records WHERE source_file = ?1",
                [relative_path],
                |row| row.get(0),
            )
            .expect("count metric records");
        assert_eq!(record_count, 0);
    }

    #[test]
    fn metric_file_states_returns_all_records() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");

        for (relative_path, record_id) in [
            ("Metrics/withings.metrics.ndjson", "01AAA"),
            ("Metrics/whoop.metrics.ndjson", "01AAB"),
        ] {
            let rows = parse_metric_rows(
                &format!(
                    r#"{{"id":"{record_id}","ts":"2026-04-14T08:30:00+00:00","key":"body.weight","value":105.6,"unit":"kg","source":"withings"}}"#
                ),
                relative_path,
            );
            db.upsert_metrics_file(relative_path, Utc::now(), &rows, Utc::now())
                .expect("upsert succeeds");
        }

        let states = db.metric_file_states().expect("metric file states query");
        assert_eq!(states.len(), 2);
        assert!(states.contains_key("Metrics/withings.metrics.ndjson"));
        assert!(states.contains_key("Metrics/whoop.metrics.ndjson"));
    }

    #[test]
    fn remove_metrics_file_cleans_up_rows() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");
        let relative_path = "Metrics/withings.metrics.ndjson";
        let rows = parse_metric_rows(
            r#"{"id":"01AAA","ts":"2026-04-14T08:30:00+00:00","key":"body.weight","value":105.6,"unit":"kg","source":"withings","extra":"kept"}"#,
            relative_path,
        );
        db.upsert_metrics_file(relative_path, Utc::now(), &rows, Utc::now())
            .expect("upsert succeeds");

        assert!(
            db.remove_metrics_file(relative_path)
                .expect("remove metrics file")
        );
        assert!(
            !db.remove_metrics_file(relative_path)
                .expect("second remove returns false")
        );

        let conn = db.connection().expect("connection");
        let file_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM metric_files", [], |row| row.get(0))
            .expect("count metric files");
        let record_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM metric_records", [], |row| row.get(0))
            .expect("count metric records");
        let issue_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM metric_issues", [], |row| row.get(0))
            .expect("count metric issues");
        assert_eq!((file_count, record_count, issue_count), (0, 0, 0));
    }

    #[test]
    fn upsert_note_persists_content_and_metadata() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");
        let note = load_note("Photography Equipment");
        let extraction = MetadataExtractor::new()
            .extract(&note)
            .expect("extract succeeds");

        let resolved_links = make_resolved_links(&extraction);

        db.upsert_note(&note, &extraction, &resolved_links, Utc::now())
            .expect("upsert succeeds");

        let conn = db.connection().expect("connection");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE id = ?1",
                [&note.id],
                |row| row.get(0),
            )
            .expect("count notes");
        assert_eq!(count, 1);

        let category: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE note_id = ?1 AND key = 'category'",
                [&note.id],
                |row| row.get(0),
            )
            .expect("fetch metadata");
        assert_eq!(category, "\"reference\"");

        let fts_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes_fts WHERE id = ?1",
                [&note.id],
                |row| row.get(0),
            )
            .expect("count fts row");
        assert_eq!(fts_count, 1);

        let link_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM note_links WHERE source_id = ?1 AND target_id IS NOT NULL",
                [&note.id],
                |row| row.get(0),
            )
            .expect("count links");
        assert!(link_count >= 1);
    }

    #[test]
    fn metadata_date_rows_are_recorded() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");
        let note = load_note("Complex Metadata Types");
        let extraction = MetadataExtractor::new()
            .extract(&note)
            .expect("extract succeeds");
        let resolved_links = make_resolved_links(&extraction);

        db.upsert_note(&note, &extraction, &resolved_links, Utc::now())
            .expect("upsert succeeds");

        let conn = db.connection().expect("connection");
        let mut stmt = conn
            .prepare("SELECT value FROM metadata_dates WHERE note_id = ?1 AND key = 'date'")
            .expect("prepare metadata date query");
        let rows = stmt
            .query_map([&note.id], |row| row.get::<_, i64>(0))
            .expect("iterate metadata dates");

        let mut values = Vec::new();
        for row in rows {
            values.push(row.expect("row value"));
        }

        let expected = parse_absolute_date("2024-01-20")
            .expect("parse frontmatter date")
            .instant
            .timestamp_micros();
        assert!(values.contains(&expected));
    }

    #[test]
    fn filter_note_ids_respects_metadata_dates() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");
        let note = load_note("Complex Metadata Types");
        let extraction = MetadataExtractor::new()
            .extract(&note)
            .expect("extract succeeds");
        let resolved_links = make_resolved_links(&extraction);

        db.upsert_note(&note, &extraction, &resolved_links, Utc::now())
            .expect("upsert succeeds");

        let matching_filters =
            crate::query::parse_query("metadata AND date:2024-01-01..2024-01-31")
                .expect("parse filters")
                .filters;
        let allowed = db
            .filter_note_ids(&[note.id.clone()], &matching_filters)
            .expect("filter applies");
        assert!(allowed.contains(&note.id));

        let non_matching_filters =
            crate::query::parse_query("metadata AND date:2023-01-01..2023-12-31")
                .expect("parse filters")
                .filters;
        let rejected = db
            .filter_note_ids(&[note.id.clone()], &non_matching_filters)
            .expect("filter applies");
        assert!(!rejected.contains(&note.id));
    }

    #[test]
    fn note_state_reports_indexed_timestamps() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");
        let note = load_note("2024-01-15");
        let extraction = MetadataExtractor::new()
            .extract(&note)
            .expect("extract succeeds");

        let indexed_at = Utc::now();
        let resolved_links = make_resolved_links(&extraction);

        db.upsert_note(&note, &extraction, &resolved_links, indexed_at)
            .expect("upsert succeeds");

        let state = db
            .note_state(&note.id)
            .expect("state query succeeds")
            .expect("state present");

        assert_eq!(
            state.file_modified_at.timestamp_micros(),
            note.file_modified_at.timestamp_micros()
        );
        assert_eq!(
            state.indexed_at.timestamp_micros(),
            indexed_at.timestamp_micros()
        );
    }

    #[test]
    fn note_states_returns_all_records() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");

        for note_id in ["2024-01-15", "Photography Equipment"] {
            let note = load_note(note_id);
            let extraction = MetadataExtractor::new()
                .extract(&note)
                .expect("extract succeeds");
            let resolved_links = make_resolved_links(&extraction);
            db.upsert_note(&note, &extraction, &resolved_links, Utc::now())
                .expect("upsert succeeds");
        }

        let states = db.note_states().expect("note states query");
        assert_eq!(states.len(), 2);
        assert!(states.contains_key("2024-01-15"));
        assert!(states.contains_key("Photography Equipment"));
    }

    #[test]
    fn connection_for_thread_reuses_same_handle() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");

        {
            let conn = db.connection_for_thread().expect("thread connection");
            conn.execute("CREATE TEMP TABLE temp_conn_test (id INTEGER)", [])
                .expect("create temp table");
        }

        let conn = db.connection_for_thread().expect("thread connection reuse");
        let temp_table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_temp_master WHERE name = 'temp_conn_test'",
                [],
                |row| row.get(0),
            )
            .expect("inspect temp tables");

        assert_eq!(temp_table_count, 1);
    }

    #[test]
    fn rebuilds_database_when_schema_version_changes() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("index.db");

        {
            let db = IndexDatabase::open(&db_path).expect("database opens");
            let note = load_note("Photography Equipment");
            let extraction = MetadataExtractor::new()
                .extract(&note)
                .expect("extract succeeds");
            let resolved_links = make_resolved_links(&extraction);
            db.upsert_note(&note, &extraction, &resolved_links, Utc::now())
                .expect("upsert succeeds");
        }

        {
            let conn = Connection::open(&db_path).expect("open raw connection");
            conn.pragma_update(None, "user_version", 999)
                .expect("set bogus version");
        }

        let db = IndexDatabase::open(&db_path).expect("database rebuilds");
        let conn = db.connection().expect("connection available");
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read schema version");
        assert_eq!(version, INDEX_SCHEMA_VERSION);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
            .expect("count notes");
        assert_eq!(count, 0, "incompatible database should be cleared");
    }

    #[test]
    fn metadata_for_notes_returns_expected_fields() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");
        let note = load_note("Photography Equipment");
        let extraction = MetadataExtractor::new()
            .extract(&note)
            .expect("extract succeeds");
        let resolved_links = make_resolved_links(&extraction);
        db.upsert_note(&note, &extraction, &resolved_links, Utc::now())
            .expect("upsert succeeds");

        let metadata = db
            .metadata_for_notes(&[note.id.clone()])
            .expect("metadata query succeeds");
        let map = metadata.get(&note.id).expect("metadata present");
        assert_eq!(
            map.get("category").and_then(|value| value.as_str()),
            Some("reference")
        );
        assert!(map.get("tags").is_some());
    }

    #[test]
    fn list_note_ids_returns_inserted_notes() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");
        let note = load_note("Photography Equipment");
        let extraction = MetadataExtractor::new()
            .extract(&note)
            .expect("extract succeeds");
        db.upsert_note(&note, &extraction, &[], Utc::now())
            .expect("upsert succeeds");

        let ids = db.list_note_ids().expect("list note ids");
        assert_eq!(ids, vec![note.id.clone()]);
    }

    #[test]
    fn remove_note_cleans_up_row_and_fts() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");
        let note = load_note("Photography Equipment");
        let extraction = MetadataExtractor::new()
            .extract(&note)
            .expect("extract succeeds");
        db.upsert_note(&note, &extraction, &[], Utc::now())
            .expect("upsert succeeds");

        assert!(db.remove_note(&note.id).expect("remove note"));
        assert!(
            !db.remove_note(&note.id)
                .expect("second removal returns false")
        );

        let ids = db.list_note_ids().expect("list note ids");
        assert!(ids.is_empty());

        let conn = db.connection().expect("connection");
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM notes_fts WHERE id = ?1",
                [&note.id],
                |row| row.get(0),
            )
            .expect("query fts table");
        assert_eq!(remaining, 0);
    }

    #[test]
    fn search_fts_returns_ranked_results() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");
        let note = load_note("Photography Equipment");
        let extraction = MetadataExtractor::new()
            .extract(&note)
            .expect("extract succeeds");
        let resolved_links = make_resolved_links(&extraction);
        db.upsert_note(&note, &extraction, &resolved_links, Utc::now())
            .expect("upsert succeeds");

        let matches = db
            .search_fts("metadata:\"category:reference\"", 5)
            .expect("fts search succeeds");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].note_id, note.id);
        assert!(matches[0].rank.is_finite());
        assert!(matches[0].snippet.is_some());
    }
}
/// Cached lookup tables used when resolving WikiLinks.
#[derive(Debug, Default, Clone)]
pub struct LinkResolutionMaps {
    /// Lower-cased note titles mapped to note identifiers.
    pub titles: HashMap<String, String>,
    /// Lower-cased aliases mapped to note identifiers (multiple entries when ambiguous).
    pub aliases: HashMap<String, Vec<String>>,
}
