//! `arrowhead notes` command family.

use std::fs;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use tracing::info;

use super::CommandContext;
use crate::logging;
use arrowhead_core::{MetadataMap, Vault, VaultConfig};
use serde_json::Value as JsonValue;

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
        NoteAction::Create(args) => {
            info!(id = ?args.id, "creating note");
            create_note(&vault, args)?;
            println!(
                "Created note {}",
                args.id
                    .clone()
                    .or_else(|| args.title.clone())
                    .unwrap_or_else(|| "(generated)".to_string())
            );
            Ok(())
        }
        NoteAction::Update(args) => {
            info!(note_id = %args.note_id, "updating note");
            update_note(&vault, args)?;
            println!("Updated note {}", args.note_id);
            Ok(())
        }
        NoteAction::Delete(args) => {
            info!(note_id = %args.note_id, "deleting note");
            delete_note(&vault, args)?;
            println!("Deleted note {}", args.note_id);
            Ok(())
        }
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

fn create_note(vault: &Vault, args: &CreateArgs) -> Result<()> {
    let note_id = resolve_note_id(args)?;

    if vault.note_file_path(&note_id)?.exists() {
        bail!("note {note_id} already exists");
    }

    let mut metadata = MetadataMap::default();
    if let Some(title) = &args.title {
        metadata.insert("title".to_string(), JsonValue::String(title.clone()));
    }
    if let Some(category) = &args.category {
        metadata.insert("category".to_string(), JsonValue::String(category.clone()));
    }

    merge_metadata_json(&mut metadata, &args.metadata)?;

    if metadata.get("title").is_none() {
        metadata.insert("title".to_string(), JsonValue::String(note_id.clone()));
    }

    let body = load_content(args.content.as_ref(), args.file.as_ref())?;

    vault.write_note(&note_id, &metadata, &body)?;
    Ok(())
}

fn update_note(vault: &Vault, args: &UpdateArgs) -> Result<()> {
    let note = vault
        .load_note(&args.note_id)
        .with_context(|| format!("note {} not found", args.note_id))?;

    let mut metadata = note.metadata.clone();

    if let Some(title) = &args.title {
        if title.trim().is_empty() {
            metadata.remove("title");
        } else {
            metadata.insert("title".to_string(), JsonValue::String(title.clone()));
        }
    }

    merge_metadata_json(&mut metadata, &args.metadata)?;

    let body = if args.content.is_some() || args.file.is_some() {
        load_content(args.content.as_ref(), args.file.as_ref())?
    } else {
        note.content.clone()
    };

    vault.write_note(&args.note_id, &metadata, &body)?;
    Ok(())
}

fn delete_note(vault: &Vault, args: &DeleteArgs) -> Result<()> {
    if !args.yes {
        bail!("use --yes to confirm deletion");
    }

    let path = vault
        .note_file_path(&args.note_id)
        .with_context(|| format!("invalid note id {}", args.note_id))?;

    if !path.exists() {
        bail!("note {} does not exist", args.note_id);
    }

    fs::remove_file(&path)
        .with_context(|| format!("failed to delete note file {}", path.display()))?;

    cleanup_empty_dirs(path.parent(), &vault.paths().root)?;
    Ok(())
}

fn cleanup_empty_dirs(start: Option<&std::path::Path>, root: &std::path::Path) -> Result<()> {
    let mut current = match start {
        Some(path) => path.to_path_buf(),
        None => return Ok(()),
    };

    while current.starts_with(root) && current != root {
        if fs::read_dir(&current)?.next().is_some() {
            break;
        }
        fs::remove_dir(&current)?;
        if let Some(parent) = current.parent() {
            current = parent.to_path_buf();
        } else {
            break;
        }
    }

    Ok(())
}

fn resolve_note_id(args: &CreateArgs) -> Result<String> {
    if let Some(id) = &args.id {
        return clean_note_id(id);
    }

    if let Some(title) = &args.title {
        return clean_note_id(title);
    }

    Err(anyhow!("note id or title is required"))
}

fn clean_note_id(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("note id must not be empty"));
    }

    let without_ext = trimmed.strip_suffix(".md").unwrap_or(trimmed);
    let candidate = without_ext.trim_matches(|c| c == '/' || c == '\\').trim();
    if candidate.is_empty() {
        return Err(anyhow!("note id must not be empty"));
    }

    Ok(candidate.replace(char::from(b'\\'), "/"))
}

fn load_content(inline: Option<&String>, file: Option<&String>) -> Result<String> {
    if let Some(path) = file {
        return fs::read_to_string(path)
            .with_context(|| format!("failed to read content file {}", path));
    }

    Ok(inline.cloned().unwrap_or_default())
}

fn merge_metadata_json(metadata: &mut MetadataMap, payload: &Option<String>) -> Result<()> {
    if let Some(raw) = payload {
        if raw.trim().is_empty() {
            return Ok(());
        }

        let value: JsonValue = serde_json::from_str(raw)
            .with_context(|| "metadata must be a JSON object".to_string())?;
        let object = value
            .as_object()
            .ok_or_else(|| anyhow!("metadata must be a JSON object"))?;

        for (key, value) in object {
            if value.is_null() {
                metadata.remove(key);
            } else {
                metadata.insert(key.clone(), value.clone());
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use tempfile::TempDir;

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

    #[test]
    fn create_note_writes_frontmatter() {
        let temp_dir = TempDir::new().expect("tempdir");
        let vault = Vault::new(VaultConfig::new(temp_dir.path().to_path_buf())).expect("vault");

        let args = CreateArgs {
            id: Some("Notes/Test Note".to_string()),
            title: Some("Test Note".to_string()),
            category: Some("testing".to_string()),
            content: Some("Body content".to_string()),
            file: None,
            metadata: Some("{\"status\": \"draft\"}".to_string()),
        };

        create_note(&vault, &args).expect("create");

        let note_path = vault.note_file_path("Notes/Test Note").expect("path");
        let written = fs::read_to_string(note_path).expect("read note");
        assert!(written.starts_with("---\n"));
        assert!(written.contains("title: Test Note"));
        assert!(written.contains("category: testing"));
        assert!(written.contains("status: draft"));
        assert!(written.contains("Body content"));
    }

    #[test]
    fn update_note_merges_metadata() {
        let temp_dir = TempDir::new().expect("tempdir");
        let vault = Vault::new(VaultConfig::new(temp_dir.path().to_path_buf())).expect("vault");

        let create_args = CreateArgs {
            id: Some("Note".to_string()),
            title: Some("Original".to_string()),
            category: Some("initial".to_string()),
            content: Some("Original body".to_string()),
            file: None,
            metadata: Some("{\"tags\": [\"one\"]}".to_string()),
        };
        create_note(&vault, &create_args).expect("create");

        let update_args = UpdateArgs {
            note_id: "Note".to_string(),
            content: Some("Updated body".to_string()),
            file: None,
            title: Some("Updated".to_string()),
            metadata: Some("{\"tags\": [\"two\"], \"status\": \"done\"}".to_string()),
        };

        update_note(&vault, &update_args).expect("update");

        let updated = vault.load_note("Note").expect("load note");
        assert_eq!(
            updated
                .metadata
                .get("title")
                .and_then(|value| value.as_str()),
            Some("Updated")
        );
        let tags = updated
            .metadata
            .get("tags")
            .and_then(|value| value.as_array())
            .expect("tags array");
        assert_eq!(tags, &vec![JsonValue::String("two".to_string())]);
        assert_eq!(
            updated
                .metadata
                .get("status")
                .and_then(|value| value.as_str()),
            Some("done")
        );
        assert!(updated.content.contains("Updated body"));
    }

    #[test]
    fn delete_note_removes_file() {
        let temp_dir = TempDir::new().expect("tempdir");
        let vault = Vault::new(VaultConfig::new(temp_dir.path().to_path_buf())).expect("vault");

        let args = CreateArgs {
            id: Some("Disposable".to_string()),
            title: None,
            category: None,
            content: Some("Temporary".to_string()),
            file: None,
            metadata: None,
        };
        create_note(&vault, &args).expect("create");

        let delete_args = DeleteArgs {
            note_id: "Disposable".to_string(),
            yes: true,
        };
        delete_note(&vault, &delete_args).expect("delete");

        assert!(!vault.note_file_path("Disposable").unwrap().exists());
    }
}
