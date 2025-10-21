//! `arrowhead vault` subcommands.

use anyhow::{Result, bail};
use clap::{Args, Subcommand};

use super::CommandContext;

/// Vault-related utilities.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct VaultCommand {
    /// Specific vault action to run.
    #[command(subcommand)]
    pub action: VaultAction,
}

/// Supported vault subcommands.
#[derive(Debug, Subcommand, Clone, PartialEq)]
pub enum VaultAction {
    /// Print vault statistics.
    Stats,
    /// Discover vault conventions for AI agents.
    Conventions,
    /// Validate vault integrity.
    Check,
}

/// Execute the vault command.
pub async fn run(ctx: &CommandContext, command: &VaultCommand) -> Result<()> {
    let _ = ctx;
    let _ = command;
    bail!("vault command not implemented yet")
}
