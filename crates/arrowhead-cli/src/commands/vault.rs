//! `arrowhead vault` subcommands.
//!
//! Focused on filesystem state and cache maintenance.

use std::fs;

use anyhow::Result;
use clap::{Args, Subcommand};

use super::CommandContext;
use crate::commands::index;

/// Vault utilities unrelated to the runtime indexer.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct VaultCommand {
    /// Specific vault action to run.
    #[command(subcommand)]
    pub action: VaultAction,
}

/// Supported vault subcommands.
#[derive(Debug, Subcommand, Clone, PartialEq)]
pub enum VaultAction {
    /// Show vault directory status and cache health.
    Status,
    /// Remove Arrowhead caches and reset daemon configuration.
    Reset,
}

/// Execute the vault command.
pub async fn run(ctx: &mut CommandContext, command: &VaultCommand) -> Result<()> {
    match command.action {
        VaultAction::Status => handle_status(ctx)?,
        VaultAction::Reset => index::handle_reset(ctx).await?,
    }

    Ok(())
}

fn handle_status(ctx: &CommandContext) -> Result<()> {
    let vault_path = index::resolve_vault_path(ctx)?;
    let (_vault, paths) = index::load_vault_environment(&vault_path)?;

    println!("Vault root: {}", vault_path.display());
    println!(
        "Arrowhead directory: {} ({})",
        paths.arrowhead_dir.display(),
        state(paths.arrowhead_dir.exists())
    );

    print_cache_entry("index database", &paths.arrowhead_dir.join("index.db"));
    print_cache_entry("status file", &paths.status_path);
    print_cache_entry("control socket", &paths.socket_path);
    print_cache_entry("PID file", &paths.pid_path);
    print_cache_entry("autostart manifest", &paths.autostart_manifest_path);
    print_cache_entry("daemon log", &paths.log_path);

    if let Some(db_size) = file_size(paths.arrowhead_dir.join("index.db")) {
        println!("Index size: {db_size} bytes");
    }

    if let Some(auto) = ctx.config.daemon.auto_start_enabled {
        println!("Auto-start configured: {}", if auto { "yes" } else { "no" });
    } else {
        println!("Auto-start configured: unknown");
    }

    if let Some(last) = &ctx.config.daemon.last_status {
        println!(
            "Last recorded index snapshot: {}",
            last.updated_at.to_rfc3339()
        );
    } else {
        println!("Last recorded index snapshot: unavailable");
    }

    Ok(())
}

fn print_cache_entry(label: &str, path: &std::path::Path) {
    println!("{label:^20}: {} ({})", path.display(), state(path.exists()));
}

fn file_size(path: impl AsRef<std::path::Path>) -> Option<u64> {
    fs::metadata(path).map(|meta| meta.len()).ok()
}

fn state(exists: bool) -> &'static str {
    if exists { "present" } else { "missing" }
}
