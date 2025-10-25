//! Arrowhead CLI
//!
//! Command-line interface for Obsidian vault indexing and search.

use std::path::PathBuf;

use anyhow::Result;
use clap::{ArgAction, Parser, Subcommand};

mod commands;
mod config;
mod logging;

use commands::{CommandContext, graph, index, init, notes, search, vault};
use config::AppConfig;

/// Arrowhead command-line interface options.
#[derive(Debug, Parser)]
#[command(name = "arrowhead")]
#[command(about = "Obsidian vault indexing and search with MCP integration")]
struct Cli {
    /// Path to the vault that should be used for this invocation.
    #[arg(long, value_name = "PATH", global = true)]
    vault: Option<PathBuf>,
    /// Path to the config file.
    #[arg(long, value_name = "PATH", global = true)]
    config: Option<PathBuf>,
    /// Increase output verbosity. Use multiple times for more detail.
    #[arg(short, long, global = true, action = ArgAction::Count)]
    verbose: u8,
    /// Command to execute.
    #[command(subcommand)]
    command: Commands,
}

/// Available top-level CLI commands.
#[derive(Debug, Subcommand)]
enum Commands {
    /// Initialise a vault and configuration.
    Init(init::InitCommand),
    /// Deprecated: indexing is handled by the background deamon.
    Index(index::IndexCommand),
    /// Execute searches.
    Search(search::SearchCommand),
    /// Perform note CRUD operations.
    Notes(notes::NotesCommand),
    /// Inspect the WikiLink graph.
    Graph(graph::GraphCommand),
    /// Vault utility commands.
    Vault(vault::VaultCommand),
}

fn init_tracing(verbosity: u8) {
    logging::init_base_tracing(verbosity);
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    init_tracing(cli.verbose);

    let mut config = AppConfig::load(cli.config.clone())?;
    if let Some(vault) = cli.vault.clone() {
        config.vault = Some(vault);
    }

    let mut ctx = CommandContext::new(config, cli.config.clone(), cli.verbose);

    match cli.command {
        Commands::Init(command) => {
            init::run(&mut ctx, &command).await?;
            ctx.persist()?;
        }
        Commands::Index(command) => {
            index::run(&ctx, &command).await?;
        }
        Commands::Search(command) => {
            search::run(&ctx, &command).await?;
        }
        Commands::Notes(command) => {
            notes::run(&ctx, &command).await?;
        }
        Commands::Graph(command) => {
            graph::run(&ctx, &command).await?;
        }
        Commands::Vault(command) => {
            vault::run(&mut ctx, &command).await?;
        }
    }

    Ok(())
}
