//! SQLite persistence layer for Arrowhead.

use std::{fs, path::Path, time::Duration};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::{MetadataMap, NoteRecord, metadata::MetadataExtraction};

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
}

impl IndexDatabase {
    /// Open (and initialise) the database at the supplied path.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create database directory {}", parent.display())
            })?;
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

        Ok(Self { pool })
    }

    /// Borrow a pooled SQLite connection.
    pub fn connection(&self) -> Result<PooledConnection<SqliteConnectionManager>> {
        self.pool
            .get()
            .context("failed to acquire SQLite connection from pool")
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

    /// Upsert the supplied note content and metadata into the index.
    pub fn upsert_note(
        &self,
        note: &NoteRecord,
        extraction: &MetadataExtraction,
        indexed_at: DateTime<Utc>,
    ) -> Result<()> {
        let mut conn = self.connection()?;
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
        let fts_metadata = format_metadata_for_fts(note, &extraction.metadata);
        tx.execute(
            "INSERT INTO notes_fts (id, content, metadata) VALUES (?1, ?2, ?3)",
            params![&note.id, &note.content, fts_metadata],
        )
        .context("failed to insert FTS row")?;

        tx.execute("DELETE FROM note_links WHERE source_id = ?1", [&note.id])
            .context("failed to clear existing note links")?;
        for link in &extraction.wikilinks {
            tx.execute(
                "INSERT INTO note_links (source_id, target_id, link_text, created_at)
                 VALUES (?1, NULL, ?2, ?3)",
                params![&note.id, link, indexed_at.timestamp()],
            )
            .context("failed to insert note link")?;
        }

        tx.commit().context("failed to commit indexing transaction")
    }
}

fn init_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", &"WAL")
        .context("failed to set journal_mode")?;
    conn.pragma_update(None, "synchronous", &"NORMAL")
        .context("failed to set synchronous")?;
    conn.pragma_update(None, "foreign_keys", &true)
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
            tokenize = 'unicode61'
        );
        "#,
    )
    .context("failed to apply schema migrations")
}

fn from_micros(micros: i64) -> Result<DateTime<Utc>> {
    let seconds = micros.div_euclid(1_000_000);
    let micros_part = micros.rem_euclid(1_000_000) as u32;
    Utc.timestamp_opt(seconds, micros_part * 1_000)
        .single()
        .context("invalid timestamp stored in database")
}

fn format_metadata_for_fts(note: &NoteRecord, metadata: &MetadataMap) -> String {
    let mut parts = Vec::new();

    if let Some(title) = &note.title {
        if !title.trim().is_empty() {
            parts.push(format!("title:{}", title.trim()));
        }
    }

    for (key, value) in metadata {
        append_metadata_value(&mut parts, key, value);
    }

    parts.join(" ")
}

fn append_metadata_value(parts: &mut Vec<String>, key: &str, value: &Value) {
    match value {
        Value::Null => {}
        Value::Bool(bool_value) => parts.push(format!("{key}:{bool_value}")),
        Value::Number(number) => parts.push(format!("{key}:{number}")),
        Value::String(string) => {
            let trimmed = string.trim();
            if !trimmed.is_empty() {
                parts.push(format!("{key}:{trimmed}"));
            }
        }
        Value::Array(items) => {
            for item in items {
                append_metadata_value(parts, key, item);
            }
        }
        Value::Object(_) => {
            if let Ok(serialised) = serde_json::to_string(value) {
                parts.push(format!("{key}:{serialised}"));
            }
        }
    }
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

        db.upsert_note(&note, &extraction, Utc::now())
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
        db.upsert_note(&note, &extraction, indexed_at)
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
}
