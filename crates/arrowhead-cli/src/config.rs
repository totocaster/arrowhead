//! CLI configuration loading and persistence.

use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use arrowhead_core::status::ActivityState;
use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// High-level Arrowhead configuration persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    /// Last-used vault path. May be overridden by CLI arguments.
    pub vault: Option<PathBuf>,
    /// Default embedding model identifier.
    pub embedding_model: Option<String>,
    /// Deamon configuration and cached status.
    #[serde(default, skip_serializing_if = "DeamonConfig::is_empty")]
    pub deamon: DeamonConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            vault: None,
            embedding_model: Some("fast".to_string()),
            deamon: DeamonConfig::default(),
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

/// Configuration persisted for the Arrowhead deamon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeamonConfig {
    /// Optional override for the control socket path.
    #[serde(default)]
    pub socket_path: Option<PathBuf>,
    /// Optional override for the status file location.
    #[serde(default)]
    pub status_path: Option<PathBuf>,
    /// Whether auto-start was approved by the user (None = unspecified).
    #[serde(default)]
    pub auto_start_enabled: Option<bool>,
    /// Last known summary of the deamon status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<DeamonStatusSummary>,
}

impl Default for DeamonConfig {
    fn default() -> Self {
        Self {
            socket_path: None,
            status_path: None,
            auto_start_enabled: None,
            last_status: None,
        }
    }
}

impl DeamonConfig {
    /// Determine whether the configuration holds any user-provided values.
    pub fn is_empty(&self) -> bool {
        self.socket_path.is_none()
            && self.status_path.is_none()
            && self.auto_start_enabled.is_none()
            && self.last_status.is_none()
    }
}

/// Lightweight cache of the most recently observed deamon status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeamonStatusSummary {
    /// When the status snapshot was recorded.
    pub updated_at: DateTime<Utc>,
    /// Activity state at that time.
    pub state: ActivityState,
    /// Number of notes indexed according to the snapshot.
    pub indexed_notes: u64,
    /// Number of notes reporting errors in the snapshot.
    pub error_notes: u64,
}

/// Determine the default configuration path using platform conventions.
pub fn default_config_path() -> PathBuf {
    ProjectDirs::from("com", "Arrowhead", "Arrowhead")
        .map(|dirs| dirs.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("~/.config/arrowhead/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn default_config_omits_empty_deamon_section() {
        let config = AppConfig::default();
        let toml = toml::to_string(&config).expect("serialize config");
        assert!(!toml.contains("deamon"));
    }

    #[test]
    fn populated_deamon_config_serialises_fields() {
        let mut config = AppConfig::default();
        config.deamon.socket_path = Some(PathBuf::from("/tmp/arrowhead.sock"));
        config.deamon.auto_start_enabled = Some(true);
        let toml = toml::to_string(&config).expect("serialize config");
        assert!(toml.contains("socket_path"));
        assert!(toml.contains("auto_start_enabled"));
    }
}
