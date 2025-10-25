//! Shared status structures for the Arrowhead deamon.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Version number stored in status files for forward compatibility.
pub const DEAMON_STATUS_VERSION: u32 = 1;

/// Persisted snapshot of the deamon's health and recent activity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeamonStatus {
    /// Schema version recorded in the status file.
    #[serde(default = "status_version")]
    pub version: u32,
    /// Timestamp representing when the status snapshot was last updated.
    pub updated_at: DateTime<Utc>,
    /// Total number of notes currently indexed.
    pub indexed_notes: u64,
    /// Number of notes that encountered errors during the latest runs.
    pub error_notes: u64,
    /// Current activity being performed by the deamon.
    pub activity: ActivityStatus,
    /// Progress of any long-running downloads (e.g., embedding models).
    #[serde(default)]
    pub downloads: Vec<DownloadStatus>,
    /// Outstanding issues that require user attention.
    #[serde(default)]
    pub issues: Vec<StatusIssue>,
    /// Filesystem path to the deamon log file for further inspection.
    pub log_path: PathBuf,
}

impl DeamonStatus {
    /// Construct a new status snapshot with default values and the supplied log path.
    pub fn new<P: Into<PathBuf>>(log_path: P) -> Self {
        Self {
            version: DEAMON_STATUS_VERSION,
            updated_at: Utc::now(),
            indexed_notes: 0,
            error_notes: 0,
            activity: ActivityStatus::idle(),
            downloads: Vec::new(),
            issues: Vec::new(),
            log_path: log_path.into(),
        }
    }

    /// Persist the status snapshot to disk, creating parent directories if needed.
    pub fn save_to_path<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create status directory {}", parent.display())
            })?;
        }

        let payload =
            serde_json::to_vec_pretty(self).context("failed to serialise deamon status")?;

        let mut tmp_path = path.to_path_buf();
        tmp_path.set_extension("tmp");

        let mut file = fs::File::create(&tmp_path)
            .with_context(|| format!("failed to create status file {}", tmp_path.display()))?;
        file.write_all(&payload)
            .with_context(|| format!("failed to write status file {}", tmp_path.display()))?;
        file.flush()
            .with_context(|| format!("failed to flush status file {}", tmp_path.display()))?;
        drop(file);

        fs::rename(&tmp_path, path)
            .with_context(|| format!("failed to move status file into place {}", path.display()))
    }

    /// Load a status snapshot from disk, returning `Ok(None)` if the file is absent.
    pub fn load_from_path<P: AsRef<Path>>(path: P) -> Result<Option<Self>> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(None);
        }

        let bytes = fs::read(path)
            .with_context(|| format!("failed to read status file {}", path.display()))?;
        let mut status: DeamonStatus = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse status file {}", path.display()))?;

        if status.version != DEAMON_STATUS_VERSION {
            status.version = DEAMON_STATUS_VERSION;
        }

        Ok(Some(status))
    }

    /// Refresh the update timestamp to the current moment.
    pub fn touch(&mut self) {
        self.updated_at = Utc::now();
    }
}

/// High-level description of what the deamon is doing right now.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivityStatus {
    /// Overall activity state.
    pub state: ActivityState,
    /// Optional identifier of the note currently being processed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_id: Option<String>,
    /// Number of queued jobs waiting to be processed.
    #[serde(default)]
    pub queued_jobs: usize,
    /// Human-readable description of the current activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl ActivityStatus {
    /// Construct an activity status representing an idle deamon.
    pub fn idle() -> Self {
        Self {
            state: ActivityState::Idle,
            note_id: None,
            queued_jobs: 0,
            description: Some("idle".to_string()),
        }
    }

    /// Helper to construct a running state with note context.
    pub fn running(state: ActivityState, note_id: Option<String>, queued_jobs: usize) -> Self {
        Self {
            state,
            note_id,
            queued_jobs,
            description: None,
        }
    }
}

/// Enumerates high-level activity states for the deamon.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    /// No work is currently being performed.
    Idle,
    /// Indexing work is in progress.
    Indexing,
    /// Removal of stale records is in progress.
    Removing,
    /// The deamon is downloading assets (e.g., embedding models).
    Downloading,
    /// The deamon encountered an unrecoverable error but remains running.
    Faulted,
}

/// Records the progress of a download task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DownloadStatus {
    /// Identifier of the download (e.g., model name).
    pub item: String,
    /// Current state of the download.
    pub state: DownloadState,
    /// Bytes retrieved so far.
    #[serde(default)]
    pub bytes_downloaded: u64,
    /// Total bytes expected, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_total: Option<u64>,
    /// Optional human-friendly message about the download.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl DownloadStatus {
    /// Construct a pending download entry.
    pub fn pending(item: impl Into<String>) -> Self {
        Self {
            item: item.into(),
            state: DownloadState::Pending,
            bytes_downloaded: 0,
            bytes_total: None,
            message: None,
        }
    }
}

/// Enumerates download lifecycle states.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    /// Download has been scheduled but not started.
    Pending,
    /// Download is in progress.
    InProgress,
    /// Download finished successfully.
    Completed,
    /// Download failed (see status issues for details).
    Failed,
}

/// Captures issues surfaced by the deamon that require user visibility.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusIssue {
    /// Machine-oriented identifier for the issue.
    pub code: String,
    /// Human-readable summary of the problem.
    pub message: String,
    /// Additional context to aid troubleshooting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Severity of the issue.
    pub severity: IssueSeverity,
    /// When the issue was recorded.
    pub occurred_at: DateTime<Utc>,
}

impl StatusIssue {
    /// Construct a new issue entry with the current timestamp.
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        severity: IssueSeverity,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            detail: None,
            severity,
            occurred_at: Utc::now(),
        }
    }
}

/// Severity levels reported in status issues.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    /// Informational message (no immediate action required).
    Info,
    /// Warning (may require attention soon).
    Warning,
    /// Error (action required).
    Error,
}

fn status_version() -> u32 {
    DEAMON_STATUS_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn status_round_trip() {
        let dir = TempDir::new().expect("tempdir");
        let status_path = dir.path().join("status.json");

        let mut status = DeamonStatus::new("/tmp/daemon.log");
        status.indexed_notes = 42;
        status.error_notes = 2;
        status.activity =
            ActivityStatus::running(ActivityState::Indexing, Some("Sample".into()), 3);
        status.downloads.push(DownloadStatus::pending("fastembed"));
        status.issues.push(StatusIssue::new(
            "index_failure",
            "failed to index note Foo",
            IssueSeverity::Error,
        ));

        status.save_to_path(&status_path).expect("save status");

        let loaded = DeamonStatus::load_from_path(&status_path)
            .expect("load status")
            .expect("status exists");

        assert_eq!(loaded.version, DEAMON_STATUS_VERSION);
        assert_eq!(loaded.indexed_notes, 42);
        assert_eq!(loaded.error_notes, 2);
        assert_eq!(loaded.activity.state, ActivityState::Indexing);
        assert_eq!(loaded.downloads.len(), 1);
        assert_eq!(loaded.issues.len(), 1);
    }

    #[test]
    fn load_missing_status_returns_none() {
        let dir = TempDir::new().expect("tempdir");
        let status_path = dir.path().join("does-not-exist.json");
        let loaded = DeamonStatus::load_from_path(&status_path).expect("load should succeed");
        assert!(loaded.is_none());
    }
}
