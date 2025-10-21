//! `arrowhead search` subcommands.

use anyhow::{Result, bail};
use clap::{Args, Subcommand};

use super::CommandContext;

/// Top-level search command grouping the different search modes.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct SearchCommand {
    /// Choose which search strategy to execute.
    #[command(subcommand)]
    pub mode: SearchMode,
}

/// Enumerates the available search modes.
#[derive(Debug, Subcommand, Clone, PartialEq)]
pub enum SearchMode {
    /// Full-text search backed by SQLite FTS5.
    Fts(QueryArgs),
    /// Semantic vector search using embeddings.
    Semantic(QueryArgs),
    /// Hybrid of FTS and semantic search.
    Hybrid(QueryArgs),
}

/// Shared arguments for search queries.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct QueryArgs {
    /// Query string to execute.
    pub query: String,
    /// Maximum number of results to return.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Output JSON for machine consumption.
    #[arg(long)]
    pub json: bool,
}

/// Dispatch search execution.
pub async fn run(ctx: &CommandContext, command: &SearchCommand) -> Result<()> {
    let _ = ctx;
    let _ = command;
    bail!("search command not implemented yet")
}
