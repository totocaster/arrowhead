//! CLI configuration loading and persistence.

use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// High-level Arrowhead configuration persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    /// Last-used vault path. May be overridden by CLI arguments.
    pub vault: Option<PathBuf>,
    /// Default embedding model identifier.
    pub embedding_model: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            vault: None,
            embedding_model: Some("all-MiniLM-L6-v2".to_string()),
        }
    }
}

impl AppConfig {
    /// Load the configuration file from disk, returning defaults if missing.
    pub fn load(path_override: Option<PathBuf>) -> Result<Self> {
        let path = path_override.unwrap_or_else(default_config_path);

        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;

        let config = toml::from_str(&content)
            .with_context(|| format!("invalid config file {}", path.display()))?;

        Ok(config)
    }

    /// Persist the configuration back to disk.
    pub fn save(&self, path_override: Option<PathBuf>) -> Result<()> {
        let path = path_override.unwrap_or_else(default_config_path);

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create config directory {}", parent.display())
            })?;
        }

        let content = toml::to_string_pretty(self).context("failed to serialize config")?;
        fs::write(&path, content)
            .with_context(|| format!("failed to write config file {}", path.display()))?;

        Ok(())
    }
}

/// Determine the default configuration path using platform conventions.
pub fn default_config_path() -> PathBuf {
    ProjectDirs::from("com", "Arrowhead", "Arrowhead")
        .map(|dirs| dirs.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("~/.config/arrowhead/config.toml"))
}
