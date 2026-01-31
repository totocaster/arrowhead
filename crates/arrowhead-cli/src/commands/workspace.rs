//! `arrowhead workspace` command implementation.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};

use arrowhead_core::{
    workspace::{load_workspace_file, write_workspace_file, WorkspaceFile, WORKSPACE_CONFIG_FILE},
    Vault, VaultConfig,
};

use super::{CommandContext, index};
use crate::commands::paths::relative_path_string;

/// Workspace management commands.
#[derive(Debug, Args, Clone)]
pub struct WorkspaceCommand {
    /// Workspace action to run.
    #[command(subcommand)]
    pub action: WorkspaceAction,
}

/// Supported workspace subcommands.
#[derive(Debug, Subcommand, Clone)]
pub enum WorkspaceAction {
    /// Display the detected workspace configuration.
    Show,
    /// Update workspace settings stored in `.arrowhead/workspace.toml`.
    Set(WorkspaceSetCommand),
}

/// Arguments that mutate workspace configuration.
#[derive(Debug, Args, Clone, PartialEq, Eq)]
pub struct WorkspaceSetCommand {
    /// Relative attachments directory for non-Obsidian workspaces.
    #[arg(long, value_name = "PATH")]
    pub attachments_dir: Option<PathBuf>,
    /// Remove the attachments directory override.
    #[arg(long)]
    pub clear_attachments: bool,
    /// Replace the ignored folders list.
    #[arg(long = "ignore", value_name = "PATH")]
    pub ignored_folders: Vec<PathBuf>,
    /// Remove all ignored folders.
    #[arg(long)]
    pub clear_ignored: bool,
    /// Set the daily note file-name format (e.g., `YYYY-MM-DD`).
    #[arg(long, value_name = "FORMAT")]
    pub daily_note_format: Option<String>,
    /// Remove the daily note format setting.
    #[arg(long)]
    pub clear_daily_note_format: bool,
    /// Set the preferred link style (e.g., `relative`, `absolute`).
    #[arg(long, value_name = "STYLE")]
    pub link_style: Option<String>,
    /// Remove the link style override.
    #[arg(long)]
    pub clear_link_style: bool,
}

impl WorkspaceSetCommand {
    fn has_mutations(&self) -> bool {
        self.attachments_dir.is_some()
            || self.clear_attachments
            || !self.ignored_folders.is_empty()
            || self.clear_ignored
            || self
                .daily_note_format
                .as_ref()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
            || self.clear_daily_note_format
            || self
                .link_style
                .as_ref()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
            || self.clear_link_style
    }
}

/// Execute the workspace command.
pub async fn run(ctx: &mut CommandContext, command: &WorkspaceCommand) -> Result<()> {
    match &command.action {
        WorkspaceAction::Show => handle_show(ctx),
        WorkspaceAction::Set(args) => handle_set(ctx, args),
    }
}

fn handle_show(ctx: &CommandContext) -> Result<()> {
    let vault_path = index::resolve_vault_path(ctx)?;
    let vault = Vault::new(VaultConfig::new(vault_path.clone()))?;

    println!("Workspace root: {}", vault_path.display());
    println!("Workspace kind: {:?}", vault.workspace_kind());
    println!("Workspace source: {:?}", vault.workspace_source());

    match load_workspace_file(&vault.paths().arrowhead_dir.join(WORKSPACE_CONFIG_FILE))? {
        Some(file) => {
            println!(
                "Configured attachments directory: {}",
                file.attachments_dir
                    .as_deref()
                    .unwrap_or("<not set>")
            );
            if file.ignored_folders.is_empty() {
                println!("Ignored folders: <none>");
            } else {
                for folder in &file.ignored_folders {
                    println!("Ignored folder: {folder}");
                }
            }
            println!(
                "Daily note format: {}",
                file.daily_note_format.as_deref().unwrap_or("<not set>")
            );
            println!(
                "Link style: {}",
                file.link_style.as_deref().unwrap_or("<not set>")
            );
        }
        None => println!(
            "Workspace config: {} (missing)",
            vault.paths().arrowhead_dir.join(WORKSPACE_CONFIG_FILE).display()
        ),
    }

    Ok(())
}

fn handle_set(ctx: &CommandContext, command: &WorkspaceSetCommand) -> Result<()> {
    if !command.has_mutations() {
        bail!("no workspace settings were provided; use --attachments-dir/--ignore/... to update values");
    }

    let vault_path = index::resolve_vault_path(ctx)?;
    let obsidian_dir = vault_path.join(".obsidian");
    if obsidian_dir.exists() {
        bail!("Obsidian metadata detected at {}. Edit settings inside Obsidian instead of using `arrowhead workspace`.", obsidian_dir.display());
    }

    let arrowhead_dir = vault_path.join(".arrowhead");
    fs::create_dir_all(&arrowhead_dir).with_context(|| {
        format!(
            "failed to create Arrowhead directory {}",
            arrowhead_dir.display()
        )
    })?;

    let workspace_path = arrowhead_dir.join(WORKSPACE_CONFIG_FILE);
    let mut file = load_workspace_file(&workspace_path)?.unwrap_or_default();
    apply_mutations(&vault_path, command, &mut file)?;
    write_workspace_file(&workspace_path, &file)?;

    println!(
        "Updated workspace configuration at {}",
        workspace_path.display()
    );

    Ok(())
}

fn apply_mutations(
    vault_root: &Path,
    args: &WorkspaceSetCommand,
    file: &mut WorkspaceFile,
) -> Result<()> {
    if args.clear_attachments && args.attachments_dir.is_some() {
        bail!("cannot set and clear the attachments directory simultaneously");
    }
    if args.clear_daily_note_format && args.daily_note_format.is_some() {
        bail!("cannot set and clear the daily note format simultaneously");
    }
    if args.clear_link_style && args.link_style.is_some() {
        bail!("cannot set and clear the link style simultaneously");
    }

    if args.clear_attachments {
        file.attachments_dir = None;
    } else if let Some(dir) = &args.attachments_dir {
        file.attachments_dir = Some(relative_path_string(vault_root, dir)?);
    }

    if args.clear_ignored {
        file.ignored_folders.clear();
    } else if !args.ignored_folders.is_empty() {
        let mut ignored = Vec::new();
        for folder in &args.ignored_folders {
            ignored.push(relative_path_string(vault_root, folder)?);
        }
        ignored.sort();
        ignored.dedup();
        file.ignored_folders = ignored;
    }

    if args.clear_daily_note_format {
        file.daily_note_format = None;
    } else if let Some(format) = &args.daily_note_format {
        file.daily_note_format = normalise_optional_string(format);
    }

    if args.clear_link_style {
        file.link_style = None;
    } else if let Some(style) = &args.link_style {
        file.link_style = normalise_optional_string(style);
    }

    Ok(())
}

fn normalise_optional_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn set_command_detects_mutations() {
        let cmd = WorkspaceSetCommand {
            attachments_dir: None,
            clear_attachments: false,
            ignored_folders: Vec::new(),
            clear_ignored: false,
            daily_note_format: None,
            clear_daily_note_format: false,
            link_style: None,
            clear_link_style: false,
        };
        assert!(!cmd.has_mutations(), "empty command should report no mutations");

        let mut mutated = cmd.clone();
        mutated.attachments_dir = Some(PathBuf::from("Assets"));
        assert!(mutated.has_mutations(), "attachments dir counts as mutation");
    }

    #[test]
    fn apply_mutations_updates_workspace_file() {
        let temp = TempDir::new().expect("tempdir");
        let args = WorkspaceSetCommand {
            attachments_dir: Some(PathBuf::from("Assets")),
            clear_attachments: false,
            ignored_folders: vec![PathBuf::from("Drafts"), PathBuf::from("Private")],
            clear_ignored: false,
            daily_note_format: Some("YYYY-MM-DD".to_string()),
            clear_daily_note_format: false,
            link_style: Some("relative".to_string()),
            clear_link_style: false,
        };
        let mut file = WorkspaceFile::default();

        apply_mutations(temp.path(), &args, &mut file).expect("mutations apply");
        assert_eq!(file.attachments_dir.as_deref(), Some("Assets"));
        assert_eq!(
            file.ignored_folders,
            vec!["Drafts".to_string(), "Private".to_string()]
        );
        assert_eq!(
            file.daily_note_format.as_deref(),
            Some("YYYY-MM-DD")
        );
        assert_eq!(file.link_style.as_deref(), Some("relative"));
    }

    #[test]
    fn apply_mutations_rejects_conflicting_flags() {
        let temp = TempDir::new().expect("tempdir");
        let args = WorkspaceSetCommand {
            attachments_dir: Some(PathBuf::from("Assets")),
            clear_attachments: true,
            ignored_folders: Vec::new(),
            clear_ignored: false,
            daily_note_format: None,
            clear_daily_note_format: false,
            link_style: None,
            clear_link_style: false,
        };
        let mut file = WorkspaceFile::default();
        let err = apply_mutations(temp.path(), &args, &mut file)
            .expect_err("conflicting flags should error");
        assert!(
            err.to_string()
                .contains("set and clear the attachments directory"),
            "unexpected error: {err}"
        );
    }
}
