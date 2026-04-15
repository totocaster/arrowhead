//! Metrics conventions and discovery helpers.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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
