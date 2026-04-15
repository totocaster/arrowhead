//! CLI command dispatch helpers.

use std::path::PathBuf;

use anyhow::Result;

use crate::config::AppConfig;

pub mod context;
pub mod graph;
pub mod index;
pub mod init;
pub mod mcp;
pub mod metrics;
pub mod notes;
pub mod paths;
pub mod search;
pub mod vault;
pub mod workspace;

/// Shared context passed to command implementations.
#[derive(Debug, Clone)]
pub struct CommandContext {
    /// Loaded configuration file.
    pub config: AppConfig,
    /// Optional explicit config path supplied by the user.
    pub config_path: Option<PathBuf>,
    /// CLI verbosity level passed via `-v`/`--verbose`.
    pub verbosity: u8,
}

impl CommandContext {
    /// Construct a new context.
    pub fn new(config: AppConfig, config_path: Option<PathBuf>, verbosity: u8) -> Self {
        Self {
            config,
            config_path,
            verbosity,
        }
    }

    /// Save the configuration if the command mutated it.
    pub fn persist(&self) -> Result<()> {
        self.config.save(self.config_path.clone())
    }

    /// Access the CLI verbosity level.
    pub fn verbosity(&self) -> u8 {
        self.verbosity
    }
}
