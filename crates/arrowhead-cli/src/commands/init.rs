//! `arrowhead init` command implementation.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Args;

use super::CommandContext;

/// Initialise a vault for Arrowhead usage.
#[derive(Debug, Args, Clone, PartialEq, Eq)]
pub struct InitCommand {
    /// Path to the vault that should be initialised.
    #[arg(long, value_name = "PATH")]
    pub vault: Option<PathBuf>,
    /// Embedding model identifier to store in the config.
    #[arg(long, value_name = "MODEL")]
    pub embeddings: Option<String>,
    /// Overwrite existing configuration and directories if present.
    #[arg(long)]
    pub force: bool,
}

/// Run the init command.
pub async fn run(ctx: &mut CommandContext, command: &InitCommand) -> Result<()> {
    let _ = ctx;
    let _ = command;
    bail!("init command not implemented yet")
}
