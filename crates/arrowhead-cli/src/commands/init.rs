//! `arrowhead init` command implementation.

use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;

use arrowhead_core::{Vault, VaultConfig, embeddings::EmbeddingPreset};
use tracing::info;

use crate::commands::index::{self, InitOptions};

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
    /// Prepare the vault without starting the deamon (used for advanced setups).
    #[arg(long)]
    pub no_start: bool,
    /// Disable semantic indexing and configure the deamon for FTS-only operation.
    #[arg(long)]
    pub fts_only: bool,
}

/// Run the init command.
pub async fn run(ctx: &mut CommandContext, command: &InitCommand) -> Result<()> {
    let vault_path = command
        .vault
        .clone()
        .or_else(|| ctx.config.vault.clone())
        .or_else(|| std::env::current_dir().ok())
        .context("vault path not provided and current directory unavailable")?;

    if !vault_path.exists() {
        if command.force {
            fs::create_dir_all(&vault_path).with_context(|| {
                format!("failed to create vault directory {}", vault_path.display())
            })?;
        } else {
            bail!(
                "vault directory {} does not exist (use --force to create it)",
                vault_path.display()
            );
        }
    }

    let vault = Vault::new(VaultConfig::new(vault_path.clone()))?;

    let logs_dir = vault.paths().logs_dir();
    let _logging_guard = crate::logging::scoped_file_logging(&logs_dir, ctx.verbosity())?;

    info!(path = %vault_path.display(), force = command.force, "initialising vault");

    ctx.config.vault = Some(vault.paths().root.clone());
    if command.fts_only && command.embeddings.is_some() {
        bail!("--fts-only cannot be used together with --embeddings");
    }
    if command.fts_only {
        ctx.config.embedding_model = None;
        info!("configured vault for full-text search only");
    }
    if let Some(model) = &command.embeddings {
        EmbeddingPreset::from_identifier(model)
            .with_context(|| format!("unknown embedding preset `{model}`"))?;
        ctx.config.embedding_model = Some(model.clone());
        info!(model = model.as_str(), "set default embedding model");
    }

    index::initialise_vault(
        ctx,
        InitOptions {
            force: command.force,
            no_start: command.no_start,
            fts_only: command.fts_only,
        },
    )
    .await?;

    info!("initialisation complete");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use tempfile::TempDir;

    use crate::commands::CommandContext;
    use crate::config::AppConfig;

    #[tokio::test]
    async fn init_creates_arrowhead_dirs_and_updates_config() {
        let vault_dir = TempDir::new().expect("temp vault");
        let config_dir = TempDir::new().expect("config dir");
        let config_path = config_dir.path().join("config.toml");

        let mut ctx = CommandContext::new(AppConfig::default(), Some(config_path.clone()), 0);
        let command = InitCommand {
            vault: Some(vault_dir.path().to_path_buf()),
            embeddings: Some("fast".to_string()),
            force: true,
            no_start: true,
            fts_only: false,
        };

        run(&mut ctx, &command).await.expect("init succeeds");
        ctx.persist().expect("config saved");

        assert!(vault_dir.path().join(".arrowhead").exists());

        let config_contents = fs::read_to_string(&config_path).expect("config readable");
        assert!(config_contents.contains("fast"));
    }

    #[tokio::test]
    async fn init_with_fts_only_disables_embeddings() {
        let vault_dir = TempDir::new().expect("temp vault");
        let config_dir = TempDir::new().expect("config dir");
        let config_path = config_dir.path().join("config.toml");

        let mut ctx = CommandContext::new(AppConfig::default(), Some(config_path.clone()), 0);
        let command = InitCommand {
            vault: Some(vault_dir.path().to_path_buf()),
            embeddings: None,
            force: true,
            no_start: true,
            fts_only: true,
        };

        run(&mut ctx, &command).await.expect("init succeeds");
        assert!(
            ctx.config.embedding_model.is_none(),
            "embedding model should be cleared when --fts-only is used"
        );

        ctx.persist().expect("config saved");
        let config_contents = fs::read_to_string(&config_path).expect("config readable");
        assert!(
            !config_contents.contains("embedding_model"),
            "config should not persist embedding model when disabled"
        );
    }
}
