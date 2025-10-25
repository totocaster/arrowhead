//! SQLite persistence layer for Arrowhead.

use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use tracing::{debug, info};

use crate::{MetadataMap, NoteRecord, metadata::MetadataExtraction};

thread_local! {
    static THREAD_CONNECTIONS: RefCell<HashMap<usize, Vec<PooledConnection<SqliteConnectionManager>>>> =
        RefCell::new(HashMap::new());
}

static NEXT_DATABASE_ID: AtomicUsize = AtomicUsize::new(1);

/// Current schema version for the Arrowhead index database.
const INDEX_SCHEMA_VERSION: i32 = 2;

/// Tracks existing index metadata for a note to drive staleness checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteIndexState {
    /// Filesystem modification timestamp stored in the index.
    pub file_modified_at: DateTime<Utc>,
    /// When the note was last indexed.
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
            .max_size(8)
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

    /// Upsert the supplied note content and metadata into the index.
    pub fn upsert_note(
        &self,
        note: &NoteRecord,
        extraction: &MetadataExtraction,
        resolved_links: &[(String, Option<String>)],
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
        for (link_text, target_id) in resolved_links {
            tx.execute(
                "INSERT INTO note_links (source_id, target_id, link_text, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    &note.id,
                    target_id.as_deref(),
                    link_text,
                    indexed_at.timestamp()
                ],
            )
            .context("failed to insert note link")?;
        }

        tx.commit().context("failed to commit indexing transaction")
    }

    /// Execute a full-text search query against the FTS index.
    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<FtsMatch>> {
        let limit = limit.max(1) as i64;
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "SELECT n.id, n.title, n.relative_path, bm25(notes_fts) AS rank,
                    snippet(notes_fts, 1, '[[', ']]', '...', 20) AS snippet
             FROM notes_fts
             JOIN notes n ON notes_fts.id = n.id
             WHERE notes_fts MATCH ?1
             ORDER BY rank ASC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![query, limit], |row| {
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

    /// Load metadata maps for a collection of note identifiers.
    pub fn metadata_for_notes(&self, note_ids: &[String]) -> Result<HashMap<String, MetadataMap>> {
        let mut result: HashMap<String, MetadataMap> = HashMap::new();
        if note_ids.is_empty() {
            return Ok(result);
        }

        let conn = self.connection()?;
        let placeholders = std::iter::repeat("?")
            .take(note_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
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
            result
                .entry(note_id)
                .or_insert_with(MetadataMap::default)
                .insert(key, value);
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
        let placeholders = std::iter::repeat("?")
            .take(note_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
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

        CREATE TABLE IF NOT EXISTS note_links (
            source_id TEXT NOT NULL,
            target_id TEXT,
            link_text TEXT NOT NULL,
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
        "#,
    )
    .context("failed to apply schema migrations")?;

    conn.pragma_update(None, "user_version", INDEX_SCHEMA_VERSION)
        .context("failed to set schema version")
}

/// Connection handle tied to a worker thread for SQLite reuse.
pub struct ThreadConnection {
    id: usize,
    conn: Option<PooledConnection<SqliteConnectionManager>>,
}

impl std::ops::Deref for ThreadConnection {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        &*self
            .conn
            .as_ref()
            .expect("thread connection missing handle")
    }
}

impl std::ops::DerefMut for ThreadConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self
            .conn
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use chrono::Utc;
    use tempfile::TempDir;

    use crate::{
        metadata::MetadataExtractor,
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
    }

    #[test]
    fn upsert_note_persists_content_and_metadata() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");
        let note = load_note("Photography Equipment");
        let extraction = MetadataExtractor::new()
            .extract(&note)
            .expect("extract succeeds");

        let resolved_links: Vec<(String, Option<String>)> = extraction
            .wikilinks
            .iter()
            .map(|link| (link.clone(), Some(link.clone())))
            .collect();

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
    fn note_state_reports_indexed_timestamps() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");
        let note = load_note("2024-01-15");
        let extraction = MetadataExtractor::new()
            .extract(&note)
            .expect("extract succeeds");

        let indexed_at = Utc::now();
        let resolved_links: Vec<(String, Option<String>)> = extraction
            .wikilinks
            .iter()
            .map(|link| (link.clone(), Some(link.clone())))
            .collect();

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
            let resolved_links: Vec<(String, Option<String>)> = extraction
                .wikilinks
                .iter()
                .map(|link| (link.clone(), Some(link.clone())))
                .collect();
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
            let resolved_links: Vec<(String, Option<String>)> = extraction
                .wikilinks
                .iter()
                .map(|link| (link.clone(), Some(link.clone())))
                .collect();
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
        let resolved_links: Vec<(String, Option<String>)> = extraction
            .wikilinks
            .iter()
            .map(|link| (link.clone(), Some(link.clone())))
            .collect();
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
    fn search_fts_returns_ranked_results() {
        let dir = TempDir::new().expect("tempdir");
        let db = IndexDatabase::open(dir.path().join("index.db")).expect("database opens");
        let note = load_note("Photography Equipment");
        let extraction = MetadataExtractor::new()
            .extract(&note)
            .expect("extract succeeds");
        let resolved_links: Vec<(String, Option<String>)> = extraction
            .wikilinks
            .iter()
            .map(|link| (link.clone(), Some(link.clone())))
            .collect();
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
