//! `arrowhead notes` command family.

use anyhow::{Result, bail};
use clap::{Args, Subcommand};

use super::CommandContext;

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
    let _ = ctx;
    let _ = command;
    bail!("notes command not implemented yet")
}
