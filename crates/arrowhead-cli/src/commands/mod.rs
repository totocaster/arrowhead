//! CLI command dispatch helpers.

use std::path::PathBuf;

use anyhow::Result;

use crate::config::AppConfig;

pub mod graph;
pub mod index;
pub mod init;
pub mod notes;
pub mod search;
pub mod vault;

/// Shared context passed to command implementations.
#[derive(Debug, Clone)]
pub struct CommandContext {
    /// Loaded configuration file.
    pub config: AppConfig,
    /// Optional explicit config path supplied by the user.
    pub config_path: Option<PathBuf>,
}

impl CommandContext {
    /// Construct a new context.
    pub fn new(config: AppConfig, config_path: Option<PathBuf>) -> Self {
        Self {
            config,
            config_path,
        }
    }

    /// Save the configuration if the command mutated it.
    pub fn persist(&self) -> Result<()> {
        self.config.save(self.config_path.clone())
    }
}
