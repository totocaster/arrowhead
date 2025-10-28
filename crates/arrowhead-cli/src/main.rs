//! Arrowhead CLI
//!
//! Command-line interface for Arrowhead vault operations and search.

use std::path::PathBuf;

use anyhow::Result;
use clap::{ArgAction, CommandFactory, Parser, Subcommand};

mod autostart;
mod commands;
mod config;
mod logging;

use commands::mcp::McpServerCliArgs;
use commands::{CommandContext, graph, init, mcp, notes, search, status, vault};
use config::AppConfig;

/// Arrowhead command-line interface options.
#[derive(Debug, Parser)]
#[command(name = "arrowhead")]
#[command(about = "Obsidian vault search and MCP integration")]
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
    /// Start the MCP stdio server instead of running a CLI subcommand.
    #[arg(long, conflicts_with = "command", conflicts_with = "mcp_server")]
    mcp: bool,
    /// Start the MCP HTTP server.
    #[arg(long, conflicts_with = "command", conflicts_with = "mcp")]
    mcp_server: bool,
    /// Options controlling MCP HTTP server behaviour.
    #[command(flatten)]
    mcp_server_opts: McpServerCliArgs,
    /// Command to execute.
    #[command(subcommand)]
    command: Option<Commands>,
}

/// Available top-level CLI commands.
#[derive(Debug, Subcommand)]
enum Commands {
    /// Initialise a vault and configuration.
    Init(init::InitCommand),
    /// Execute searches.
    Search(search::SearchCommand),
    /// Perform note CRUD operations.
    Notes(notes::NotesCommand),
    /// Inspect the WikiLink graph.
    Graph(graph::GraphCommand),
    /// Stream live daemon status updates.
    Status(status::StatusCommand),
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

    if cli.mcp {
        mcp::run_stdio(&mut ctx).await?;
        return Ok(());
    }

    if cli.mcp_server {
        mcp::run_server(&mut ctx, &cli.mcp_server_opts).await?;
        return Ok(());
    }

    let command = match cli.command {
        Some(command) => command,
        None => {
            Cli::command().print_help()?;
            println!();
            return Ok(());
        }
    };

    match command {
        Commands::Init(command) => {
            init::run(&mut ctx, &command).await?;
            ctx.persist()?;
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
        Commands::Status(command) => {
            status::run(&ctx, &command).await?;
        }
        Commands::Vault(command) => {
            vault::run(&mut ctx, &command).await?;
        }
    }

    Ok(())
}
