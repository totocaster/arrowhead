//! `arrowhead notes` command family.

use std::fs;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use tracing::info;

use super::CommandContext;
use crate::logging;
use arrowhead_core::{Vault, VaultConfig};

/// CRUD operations for notes.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct NotesCommand {
    /// Select which note operation to perform.
    #[command(subcommand)]
    pub action: NoteAction,
}

/// Available note subcommands.
#[derive(Debug, Subcommand, Clone, PartialEq)]
pub enum NoteAction {
    /// Read a note by ID.
    Read(ReadArgs),
    /// List notes in the vault.
    List(ListArgs),
    /// Create a new note.
    Create(CreateArgs),
    /// Update an existing note.
    Update(UpdateArgs),
    /// Delete a note.
    Delete(DeleteArgs),
}

/// Arguments for reading a note.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct ReadArgs {
    /// Identifier of the note to read.
    pub note_id: String,
}

/// Arguments for listing notes.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct ListArgs {
    /// Only return note identifiers.
    #[arg(long)]
    pub ids_only: bool,
}

/// Arguments for creating a note.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct CreateArgs {
    /// Explicit note identifier. If omitted one will be generated.
    #[arg(long)]
    pub id: Option<String>,
    /// Title for the note.
    #[arg(long)]
    pub title: Option<String>,
    /// Category metadata field.
    #[arg(long)]
    pub category: Option<String>,
    /// Inline content for the note.
    #[arg(long)]
    pub content: Option<String>,
    /// Read content from a file.
    #[arg(long, value_name = "PATH")]
    pub file: Option<String>,
    /// Additional metadata encoded as JSON.
    #[arg(long, value_name = "JSON")]
    pub metadata: Option<String>,
}

/// Arguments for updating a note.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct UpdateArgs {
    /// Identifier of the note to update.
    pub note_id: String,
    /// Replacement content for the note.
    #[arg(long)]
    pub content: Option<String>,
    /// Provide content via file path.
    #[arg(long, value_name = "PATH")]
    pub file: Option<String>,
    /// Update the note title.
    #[arg(long)]
    pub title: Option<String>,
    /// Merge metadata provided as JSON.
    #[arg(long, value_name = "JSON")]
    pub metadata: Option<String>,
}

/// Arguments for deleting a note.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct DeleteArgs {
    /// Identifier of the note to delete.
    pub note_id: String,
    /// Skip confirmation prompt.
    #[arg(long)]
    pub yes: bool,
}

/// Execute the requested notes command.
pub async fn run(ctx: &CommandContext, command: &NotesCommand) -> Result<()> {
    let vault_path = ctx
        .config
        .vault
        .clone()
        .context("no vault configured. Provide --vault or run `arrowhead init`.")?;

    let vault = Vault::new(VaultConfig::new(vault_path))?;
    vault.ensure_arrowhead_dirs()?;

    let logs_dir = vault.paths().logs_dir();
    let _logging_guard = logging::scoped_file_logging(&logs_dir, ctx.verbosity())?;

    match &command.action {
        NoteAction::Read(args) => {
            info!(note_id = %args.note_id, "reading note contents");
            let content = read_note_raw(&vault, &args.note_id)?;
            print!("{content}");
            Ok(())
        }
        NoteAction::List(args) => {
            info!(ids_only = args.ids_only, "listing notes");
            let items = collect_note_list(&vault, args.ids_only)?;
            for (id, title) in items {
                if let Some(title) = title {
                    println!("{id}\t{title}");
                } else {
                    println!("{id}");
                }
            }
            Ok(())
        }
        NoteAction::Create(_) => bail!("note creation not implemented yet"),
        NoteAction::Update(_) => bail!("note updates not implemented yet"),
        NoteAction::Delete(_) => bail!("note deletion not implemented yet"),
    }
}

fn read_note_raw(vault: &Vault, note_id: &str) -> Result<String> {
    let note = vault
        .load_note(note_id)
        .with_context(|| format!("note {note_id} not found"))?;
    let path = vault.note_path(&note.relative_path);
    let content = fs::read_to_string(&path)
        .with_context(|| format!("failed to read note file {}", path.display()))?;
    Ok(content)
}

fn collect_note_list(vault: &Vault, ids_only: bool) -> Result<Vec<(String, Option<String>)>> {
    let mut results = Vec::new();
    for note_id in vault.list_note_ids()? {
        if ids_only {
            results.push((note_id, None));
        } else {
            let note = vault.load_note(&note_id)?;
            results.push((note_id, note.title.clone()));
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_vault() -> Vault {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tests")
            .join("fixtures")
            .join("test-vault");
        Vault::new(VaultConfig::new(root)).expect("fixture vault initialises")
    }

    #[test]
    fn read_note_returns_complete_content() {
        let vault = fixture_vault();
        let content = read_note_raw(&vault, "2024-01-15").expect("read note");
        assert!(content.starts_with("---"));
        assert!(content.contains("# January 15, 2024"));
    }

    #[test]
    fn list_ids_only_returns_identifiers() {
        let vault = fixture_vault();
        let entries = collect_note_list(&vault, true).expect("list notes");
        assert!(entries.iter().all(|(_, title)| title.is_none()));
        assert!(entries.iter().any(|(id, _)| id == "2024-01-15"));
    }

    #[test]
    fn list_with_titles_includes_note_titles() {
        let vault = fixture_vault();
        let entries = collect_note_list(&vault, false).expect("list notes");
        let photography_title = entries
            .iter()
            .find(|(id, _)| id == "Photography Equipment")
            .and_then(|(_, title)| title.clone())
            .expect("title present");
        assert_eq!(photography_title, "Photography Equipment");
    }
}
