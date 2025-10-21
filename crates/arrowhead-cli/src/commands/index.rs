//! `arrowhead index` command.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Args;

use arrowhead_core::{
    Vault, VaultConfig,
    indexer::{Indexer, IndexerConfig},
    sqlite::IndexDatabase,
};
use tracing::info;

use super::CommandContext;
use crate::logging;

/// Parameters for the `index` CLI command.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct IndexCommand {
    /// Force reindexing every note regardless of staleness.
    #[arg(long)]
    pub force: bool,
    /// Limit indexing to a single note ID.
    #[arg(long, value_name = "NOTE_ID")]
    pub note: Option<String>,
    /// Override parallel worker count.
    #[arg(long, value_name = "N")]
    pub parallel: Option<usize>,
    /// Display a progress indicator during indexing.
    #[arg(long)]
    pub progress: bool,
}

/// Execute indexing.
pub async fn run(ctx: &CommandContext, command: &IndexCommand) -> Result<()> {
    let vault_path = ctx
        .config
        .vault
        .clone()
        .context("no vault configured. Provide --vault or run `arrowhead init`.")?;

    let vault = Arc::new(Vault::new(VaultConfig::new(vault_path.clone()))?);
    vault.ensure_arrowhead_dirs()?;
    let db_path = vault.paths().arrowhead_dir.join("index.db");
    let database = Arc::new(IndexDatabase::open(&db_path)?);

    let logs_dir = vault.paths().logs_dir();
    let _logging_guard = logging::scoped_file_logging(&logs_dir, ctx.verbosity())?;

    info!(
        force = command.force,
        note = command.note.as_deref().unwrap_or("<all>"),
        "starting index command"
    );

    let mut config = IndexerConfig::default();
    config.force = command.force;
    if let Some(parallel) = command.parallel {
        config.parallelism = parallel.max(1);
    }

    let indexer = Indexer::new(vault, database, config);

    if let Some(note_id) = &command.note {
        indexer.index_note(note_id).await?;
        println!("Indexed note {note_id}");
        info!(note_id = note_id.as_str(), "indexed single note");
    } else {
        let stats = indexer.index_all().await?;
        println!(
            "Indexed {} notes ({} updated, {} skipped, {} errors)",
            stats.total_notes, stats.indexed, stats.skipped, stats.errors
        );
        info!(
            total = stats.total_notes,
            indexed = stats.indexed,
            skipped = stats.skipped,
            errors = stats.errors,
            "completed full index"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    use arrowhead_core::sqlite::IndexDatabase;
    use tempfile::TempDir;

    use crate::commands::CommandContext;
    use crate::config::AppConfig;

    fn write_note(path: &PathBuf) {
        fs::write(
            path,
            "---\ncategory: test\n---\n\n# Sample Note\n\nContent body\n",
        )
        .expect("write note");
    }

    #[tokio::test]
    async fn index_command_creates_database() {
        let vault_dir = TempDir::new().expect("temp vault");
        let note_path = vault_dir.path().join("Sample.md");
        write_note(&note_path);

        let config = AppConfig {
            vault: Some(vault_dir.path().to_path_buf()),
            embedding_model: None,
        };
        let ctx = CommandContext::new(config, None, 0);
        let command = IndexCommand {
            force: false,
            note: None,
            parallel: None,
            progress: false,
        };

        run(&ctx, &command).await.expect("index succeeds");

        let db_path = vault_dir.path().join(".arrowhead").join("index.db");
        assert!(db_path.exists());

        let database = IndexDatabase::open(&db_path).expect("open database");
        let conn = database.connection().expect("connection");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
            .expect("count notes");
        assert_eq!(count, 1);
    }
}
