//! Core type definitions for Arrowhead.

use std::{collections::BTreeMap, path::PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Unique identifier for a note (filename without extension).
pub type NoteId = String;

/// Flexible metadata storage keyed by field name.
pub type MetadataMap = BTreeMap<String, serde_json::Value>;

/// Structured representation of a note pulled from the vault.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NoteRecord {
    /// Note identifier (usually the filename without `.md`).
    pub id: NoteId,
    /// Optional title sourced from frontmatter or first heading.
    pub title: Option<String>,
    /// Parsed metadata merged from frontmatter and inline extraction.
    pub metadata: MetadataMap,
    /// Markdown body content.
    pub content: String,
    /// Filesystem path to the note relative to the vault root.
    pub relative_path: PathBuf,
    /// Last modification timestamp reported by the filesystem.
    pub file_modified_at: DateTime<Utc>,
    /// Timestamp of when the note was added to the vault, if known.
    pub created_at: Option<DateTime<Utc>>,
}

impl NoteRecord {
    /// Creates a new note record with empty metadata for bootstrapping tests.
    pub fn new<I: Into<NoteId>, P: Into<PathBuf>>(
        id: I,
        relative_path: P,
        file_modified_at: DateTime<Utc>,
        content: String,
    ) -> Self {
        Self {
            id: id.into(),
            title: None,
            metadata: MetadataMap::default(),
            content,
            relative_path: relative_path.into(),
            file_modified_at,
            created_at: None,
        }
    }
}

/// Summary statistics returned from indexing operations.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexingStats {
    /// Total number of notes scanned during the run.
    pub total_notes: u64,
    /// Number of notes that were reindexed.
    pub indexed: u64,
    /// Number of notes skipped because they were fresh in the index.
    pub skipped: u64,
    /// Number of notes that failed to index.
    pub errors: u64,
}

/// Represents a filesystem location that Arrowhead cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultPaths {
    /// Root directory of the Obsidian vault.
    pub root: PathBuf,
    /// Directory that stores Arrowhead's index data.
    pub arrowhead_dir: PathBuf,
    /// Directory containing Obsidian configuration files.
    pub obsidian_dir: PathBuf,
    /// Directory containing attachments (images, PDFs, etc.).
    pub attachments_dir: Option<PathBuf>,
}

impl VaultPaths {
    /// Construct a new set of paths, making them absolute where possible.
    pub fn new(
        root: PathBuf,
        arrowhead_dir: PathBuf,
        obsidian_dir: PathBuf,
        attachments_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            root,
            arrowhead_dir,
            obsidian_dir,
            attachments_dir,
        }
    }

    /// Directory used for storing log files.
    pub fn logs_dir(&self) -> PathBuf {
        self.arrowhead_dir.join("logs")
    }
}
