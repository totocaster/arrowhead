//! Arrowhead workspace configuration helpers.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::metrics::MetricsConfigFile;

/// Default file name used to store Arrowhead workspace metadata.
pub const WORKSPACE_CONFIG_FILE: &str = "workspace.toml";

/// Workspace flavour derived during vault initialisation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceKind {
    /// Obsidian workspace with `.obsidian` metadata.
    Obsidian,
    /// Generic Markdown workspace configured via `.arrowhead/workspace.toml`.
    Generic,
}

/// On-disk representation of `.arrowhead/workspace.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct WorkspaceFile {
    /// Relative path where attachments are stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments_dir: Option<String>,
    /// Relative directories ignored during indexing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored_folders: Vec<String>,
    /// Optional daily note title format (mirrors Obsidian's `daily-notes.json`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily_note_format: Option<String>,
    /// Preferred link style (mirrors Obsidian's `newLinkFormat`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_style: Option<String>,
    /// Optional metrics conventions overrides stored under `[metrics]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<MetricsConfigFile>,
}

/// Load an Arrowhead workspace file from disk if present.
pub fn load_workspace_file(path: &Path) -> Result<Option<WorkspaceFile>> {
    if !path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read workspace file {}", path.display()))?;

    let config: WorkspaceFile = toml::from_str(&content)
        .with_context(|| format!("invalid workspace file {}", path.display()))?;

    Ok(Some(config))
}

/// Persist an Arrowhead workspace file to disk, creating parent directories.
pub fn write_workspace_file(path: &Path, config: &WorkspaceFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create workspace directory {}", parent.display())
        })?;
    }

    let content = toml::to_string_pretty(config).context("failed to serialise workspace config")?;
    fs::write(path, content)
        .with_context(|| format!("failed to write workspace file {}", path.display()))?;
    Ok(())
}

/// Describe where the active workspace settings originated from.
#[derive(Debug, Clone)]
pub enum WorkspaceSource {
    /// Resolved from `.obsidian`.
    Obsidian(PathBuf),
    /// Resolved from `.arrowhead/workspace.toml`.
    Arrowhead(PathBuf),
    /// No explicit config file present (defaults applied).
    Default,
}
