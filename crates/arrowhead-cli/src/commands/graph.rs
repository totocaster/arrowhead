//! `arrowhead graph` operations.

use anyhow::{Result, bail};
use clap::{Args, Subcommand};

use super::CommandContext;

/// Graph command dispatcher.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct GraphCommand {
    /// Graph operation to perform.
    #[command(subcommand)]
    pub action: GraphAction,
}

/// Supported graph subcommands.
#[derive(Debug, Subcommand, Clone, PartialEq)]
pub enum GraphAction {
    /// Show backlinks pointing to a note.
    Backlinks(NoteIdArg),
    /// Show forward links from a note.
    ForwardLinks(NoteIdArg),
    /// List orphan notes with no links.
    Orphans,
    /// List unresolved WikiLinks.
    Unresolved,
    /// Show a full context summary for a note.
    Context(NoteIdArg),
}

/// Common argument containing a note identifier.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct NoteIdArg {
    /// Target note identifier.
    pub note_id: String,
}

/// Execute the graph command.
pub async fn run(ctx: &CommandContext, command: &GraphCommand) -> Result<()> {
    let _ = ctx;
    let _ = command;
    bail!("graph command not implemented yet")
}
