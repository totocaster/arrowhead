//! Arrowhead CLI
//!
//! Command-line interface for Arrowhead vault operations and search.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{ArgAction, CommandFactory, Parser, Subcommand};

mod autostart;
mod commands;
mod config;
mod logging;

use commands::mcp::McpServerCliArgs;
use commands::{CommandContext, graph, index, init, mcp, metrics, notes, search, vault, workspace};
use config::AppConfig;

/// Arrowhead command-line interface options.
#[derive(Debug, Parser)]
#[command(name = "arrowhead")]
#[command(
    about = "Obsidian vault search and MCP integration",
    long_about = "Obsidian vault search and MCP integration. When exposing MCP transports, \
                  call mcp.discovery.get_vault_conventions before any note creation, update, \
                  or deletion so agents respect local naming rules."
)]
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
    #[arg(long, conflicts_with = "mcp_server")]
    mcp: bool,
    /// Start the MCP HTTP server.
    #[arg(long, conflicts_with = "mcp")]
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
    /// Read indexed metrics files and records.
    Metrics(metrics::MetricsCommand),
    /// Inspect the WikiLink graph.
    Graph(graph::GraphCommand),
    /// Manage the background indexer lifecycle.
    Index(index::IndexCommand),
    /// Vault utility commands.
    Vault(vault::VaultCommand),
    /// Manage Arrowhead workspace metadata for non-Obsidian directories.
    Workspace(workspace::WorkspaceCommand),
}

fn init_tracing(verbosity: u8) {
    logging::init_base_tracing(verbosity);
}

fn validate_cli(cli: &Cli) -> Result<()> {
    if (cli.mcp || cli.mcp_server) && cli.command.is_some() {
        bail!("--mcp/--mcp-server cannot be used with subcommands");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    init_tracing(cli.verbose);
    validate_cli(&cli)?;

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
        Commands::Metrics(command) => {
            metrics::run(&ctx, &command).await?;
        }
        Commands::Graph(command) => {
            graph::run(&ctx, &command).await?;
        }
        Commands::Index(command) => {
            index::run(&mut ctx, &command).await?;
        }
        Commands::Vault(command) => {
            vault::run(&mut ctx, &command).await?;
        }
        Commands::Workspace(command) => {
            workspace::run(&mut ctx, &command).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn mcp_flags_conflict_with_subcommands() {
        let cli = Cli::try_parse_from(["arrowhead", "--mcp", "index", "status"])
            .expect("parse mcp + subcommand");
        assert!(validate_cli(&cli).is_err());

        let cli = Cli::try_parse_from(["arrowhead", "--mcp-server", "index", "status"])
            .expect("parse mcp-server + subcommand");
        assert!(validate_cli(&cli).is_err());
    }
}
