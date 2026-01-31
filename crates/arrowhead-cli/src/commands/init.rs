//! `arrowhead init` command implementation.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::Args;

use arrowhead_core::{
    Vault, VaultConfig,
    embeddings::EmbeddingPreset,
    workspace::{WORKSPACE_CONFIG_FILE, WorkspaceFile, write_workspace_file},
};
use tracing::{info, warn};

use crate::commands::index::{self, InitOptions};
use crate::commands::paths::{relative_path_string, resolve_relative_path};

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
    /// Prepare the vault without starting the daemon (used for advanced setups).
    #[arg(long)]
    pub no_start: bool,
    /// Disable semantic indexing and configure the daemon for FTS-only operation.
    #[arg(long)]
    pub fts_only: bool,
    /// Relative directory storing attachments when outside Obsidian.
    #[arg(long, value_name = "PATH")]
    pub attachments_dir: Option<PathBuf>,
    /// Additional relative directories to ignore when indexing generic workspaces.
    #[arg(long = "ignore", value_name = "PATH")]
    pub ignored_folders: Vec<PathBuf>,
    /// Daily note naming format when initialising generic workspaces.
    #[arg(long, value_name = "FORMAT")]
    pub daily_note_format: Option<String>,
    /// Preferred link style when initialising generic workspaces.
    #[arg(long, value_name = "STYLE")]
    pub link_style: Option<String>,
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

    let mut vault_config = VaultConfig::new(vault_path.clone());
    if let Some(attachments) = &command.attachments_dir {
        let relative = resolve_relative_path(&vault_path, attachments)?;
        vault_config.attachments_dir = Some(relative);
    }

    let obsidian_dir = vault_path.join(".obsidian");
    let workspace_config = if !obsidian_dir.exists() {
        let config = build_workspace_file_from_command(command, &vault_path)?;
        let arrowhead_dir = vault_config.resolve_arrowhead_dir();
        prepare_generic_workspace(&arrowhead_dir, command, &config)?;
        Some(config)
    } else {
        if command.has_workspace_overrides() {
            warn!(
                obsidian = %obsidian_dir.display(),
                "Obsidian workspace detected; ignoring generic workspace flags"
            );
        }
        None
    };

    let vault = Vault::new(vault_config)?;

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

    if let Some(config) = &workspace_config {
        if command.force {
            let workspace_path = vault.paths().arrowhead_dir.join(WORKSPACE_CONFIG_FILE);
            write_workspace_file(&workspace_path, config)?;
            info!(
                path = %workspace_path.display(),
                "restored Arrowhead workspace config after cleanup"
            );
        }
    }

    info!("initialisation complete");

    Ok(())
}

fn prepare_generic_workspace(
    arrowhead_dir: &Path,
    command: &InitCommand,
    config: &WorkspaceFile,
) -> Result<()> {
    fs::create_dir_all(arrowhead_dir).with_context(|| {
        format!(
            "failed to create Arrowhead directory {}",
            arrowhead_dir.display()
        )
    })?;

    let workspace_path = arrowhead_dir.join(WORKSPACE_CONFIG_FILE);
    let workspace_exists = workspace_path.exists();

    if workspace_exists && command.has_workspace_overrides() && !command.force {
        bail!(
            "workspace config {} already exists; re-run with --force to overwrite it",
            workspace_path.display()
        );
    }

    let should_write = !workspace_exists || command.force || command.has_workspace_overrides();
    if should_write {
        write_workspace_file(&workspace_path, config)?;
        info!(
            path = %workspace_path.display(),
            overwrite = workspace_exists,
            "configured Arrowhead workspace"
        );
    }

    Ok(())
}

fn build_workspace_file_from_command(
    command: &InitCommand,
    vault_root: &Path,
) -> Result<WorkspaceFile> {
    let mut file = WorkspaceFile::default();

    if let Some(dir) = &command.attachments_dir {
        file.attachments_dir = Some(relative_path_string(vault_root, dir)?);
    }

    if !command.ignored_folders.is_empty() {
        let mut ignored = Vec::new();
        for ignore in &command.ignored_folders {
            ignored.push(relative_path_string(vault_root, ignore)?);
        }
        ignored.sort();
        ignored.dedup();
        file.ignored_folders = ignored;
    }

    file.daily_note_format = normalise_optional_string(&command.daily_note_format);
    file.link_style = normalise_optional_string(&command.link_style);

    Ok(file)
}

fn normalise_optional_string(value: &Option<String>) -> Option<String> {
    value.as_ref().and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

impl InitCommand {
    fn has_workspace_overrides(&self) -> bool {
        self.attachments_dir.is_some()
            || !self.ignored_folders.is_empty()
            || self
                .daily_note_format
                .as_ref()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
            || self
                .link_style
                .as_ref()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use tempfile::TempDir;

    use arrowhead_core::workspace::{WORKSPACE_CONFIG_FILE, load_workspace_file};

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
            attachments_dir: None,
            ignored_folders: Vec::new(),
            daily_note_format: None,
            link_style: None,
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
            attachments_dir: None,
            ignored_folders: Vec::new(),
            daily_note_format: None,
            link_style: None,
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

    #[tokio::test]
    async fn init_prepares_generic_workspace_config() {
        let vault_dir = TempDir::new().expect("temp vault");
        let config_dir = TempDir::new().expect("config dir");
        let config_path = config_dir.path().join("config.toml");

        let mut ctx = CommandContext::new(AppConfig::default(), Some(config_path), 0);
        let command = InitCommand {
            vault: Some(vault_dir.path().to_path_buf()),
            embeddings: None,
            force: true,
            no_start: true,
            fts_only: false,
            attachments_dir: Some(PathBuf::from("Assets")),
            ignored_folders: vec![PathBuf::from("Private"), PathBuf::from("Drafts")],
            daily_note_format: Some("YYYY-MM-DD".to_string()),
            link_style: Some("absolute".to_string()),
        };

        run(&mut ctx, &command).await.expect("init succeeds");

        let workspace_path = vault_dir
            .path()
            .join(".arrowhead")
            .join(WORKSPACE_CONFIG_FILE);
        assert!(workspace_path.exists(), "workspace config should exist");

        let file = load_workspace_file(&workspace_path)
            .expect("workspace file loads")
            .expect("workspace file present");

        assert_eq!(file.attachments_dir.as_deref(), Some("Assets"));
        assert_eq!(file.daily_note_format.as_deref(), Some("YYYY-MM-DD"));
        assert_eq!(file.link_style.as_deref(), Some("absolute"));
        assert_eq!(
            file.ignored_folders,
            vec!["Drafts".to_string(), "Private".to_string()]
        );
    }
}
