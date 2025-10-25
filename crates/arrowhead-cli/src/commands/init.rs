//! `arrowhead init` command implementation.

use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;

use arrowhead_core::{Vault, VaultConfig, embeddings::EmbeddingPreset};
use tracing::info;

use crate::commands::vault::{VaultAction, VaultCommand, VaultInitArgs};

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

    info!(path = %vault_path.display(), force = command.force, "initialising vault");

    ctx.config.vault = Some(vault.paths().root.clone());
    if let Some(model) = &command.embeddings {
        EmbeddingPreset::from_identifier(model)
            .with_context(|| format!("unknown embedding preset `{model}`"))?;
        ctx.config.embedding_model = Some(model.clone());
        info!(model = model.as_str(), "set default embedding model");
    }

    let init_command = VaultCommand {
        action: VaultAction::Init(VaultInitArgs {
            force: command.force,
            no_start: true,
        }),
    };

    super::vault::run(ctx, &init_command).await?;

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
        };

        run(&mut ctx, &command).await.expect("init succeeds");
        ctx.persist().expect("config saved");

        assert!(vault_dir.path().join(".arrowhead").exists());

        let config_contents = fs::read_to_string(&config_path).expect("config readable");
        assert!(config_contents.contains("fast"));
    }
}
