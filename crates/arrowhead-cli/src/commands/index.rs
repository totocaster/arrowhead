//! `arrowhead index` command.

use anyhow::{Result, bail};
use clap::Args;

use super::CommandContext;

/// Parameters for the `index` CLI command.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct IndexCommand {
    /// Force reindexing every note regardless of staleness.
    #[arg(long)]
    pub force: bool,
    /// Limit indexing to a single note ID.
    #[arg(long, value_name = "NOTE_ID")]
    pub note: Option<String>,
    /// Override parallel worker count.
    #[arg(long, value_name = "N")]
    pub parallel: Option<usize>,
    /// Display a progress indicator during indexing.
    #[arg(long)]
    pub progress: bool,
}

/// Execute indexing.
pub async fn run(ctx: &CommandContext, command: &IndexCommand) -> Result<()> {
    let _ = ctx;
    let _ = command;
    bail!("index command not implemented yet")
}
