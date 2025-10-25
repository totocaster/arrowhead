//! `arrowhead search` subcommands.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde_json::json;
use tracing::info;

use arrowhead_core::IndexingStats;
use arrowhead_core::{
    SearchConfig, SearchResult, SearchService, Vault, VaultConfig,
    embeddings::EmbeddingPipeline,
    indexer::{Indexer, IndexerConfig},
    sqlite::IndexDatabase,
};

use crate::logging;

use super::CommandContext;

/// Top-level search command grouping the different search modes.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct SearchCommand {
    /// Choose which search strategy to execute.
    #[command(subcommand)]
    pub mode: SearchMode,
}

/// Enumerates the available search modes.
#[derive(Debug, Subcommand, Clone, PartialEq)]
pub enum SearchMode {
    /// Full-text search backed by SQLite FTS5.
    Fts(QueryArgs),
    /// Semantic vector search using embeddings.
    Semantic(QueryArgs),
    /// Hybrid of FTS and semantic search.
    Hybrid(QueryArgs),
}

/// Shared arguments for search queries.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct QueryArgs {
    /// Query string to execute.
    pub query: String,
    /// Maximum number of results to return.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Output JSON for machine consumption.
    #[arg(long)]
    pub json: bool,
}

/// Dispatch search execution.
pub async fn run(ctx: &CommandContext, command: &SearchCommand) -> Result<()> {
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

    #[cfg(feature = "vector-lancedb")]
    let logging_enabled = std::env::var_os("ARROWHEAD_ENABLE_FILE_LOGS").is_some();

    #[cfg(not(feature = "vector-lancedb"))]
    let logging_enabled = std::env::var_os("ARROWHEAD_DISABLE_FILE_LOGS").is_none();

    let _logging_guard = if logging_enabled {
        Some(logging::scoped_file_logging(&logs_dir, ctx.verbosity())?)
    } else {
        None
    };

    let model_id = ctx
        .config
        .embedding_model
        .clone()
        .unwrap_or_else(|| "fast".to_string());
    let embeddings = if EmbeddingPipeline::is_supported() {
        let pipeline = EmbeddingPipeline::initialise(vault.as_ref(), &model_id)
            .await
            .context("failed to prepare embedding pipeline")?;
        Some(Arc::new(pipeline))
    } else {
        None
    };

    let force_full = embeddings
        .as_ref()
        .map(|pipeline| pipeline.model_changed())
        .unwrap_or(false);

    let stats = ensure_index_fresh(
        Arc::clone(&vault),
        Arc::clone(&database),
        embeddings.clone(),
        force_full,
    )
    .await?;
    info!(
        indexed = stats.indexed,
        skipped = stats.skipped,
        removed = stats.removed,
        errors = stats.errors,
        "ensured index before search"
    );

    let service = SearchService::new(database, SearchConfig::default(), embeddings);

    match &command.mode {
        SearchMode::Fts(args) => {
            info!(query = args.query.as_str(), limit = ?args.limit, "executing FTS search");
            let results = execute_fts_search(&service, args).await?;
            render_results(&results, args.json)?;
            Ok(())
        }
        SearchMode::Semantic(args) => {
            info!(query = args.query.as_str(), limit = ?args.limit, "executing semantic search");
            let results = service
                .search_semantic(&args.query, args.limit)
                .await
                .context("failed to execute semantic search")?;
            render_results(&results, args.json)?;
            Ok(())
        }
        SearchMode::Hybrid(args) => {
            info!(query = args.query.as_str(), limit = ?args.limit, "executing hybrid search");
            let results = service
                .search_hybrid(&args.query, args.limit)
                .await
                .context("failed to execute hybrid search")?;
            render_results(&results, args.json)?;
            Ok(())
        }
    }
}

async fn execute_fts_search(
    service: &SearchService,
    args: &QueryArgs,
) -> Result<Vec<SearchResult>> {
    service
        .search_fts(&args.query, args.limit)
        .await
        .context("failed to execute FTS search")
}

async fn ensure_index_fresh(
    vault: Arc<Vault>,
    database: Arc<IndexDatabase>,
    embeddings: Option<Arc<EmbeddingPipeline>>,
    force: bool,
) -> Result<IndexingStats> {
    let mut config = IndexerConfig::default();
    config.force = force;
    let indexer = Indexer::new(vault, database, config, embeddings);
    indexer.index_all().await
}

fn render_results(results: &[SearchResult], json_output: bool) -> Result<()> {
    if json_output {
        let payload: Vec<_> = results
            .iter()
            .map(|result| {
                json!({
                    "note_id": result.note_id,
                    "title": result.title,
                    "score": result.score,
                    "bm25": result.bm25,
                    "preview": result.preview,
                    "reason": result.reason,
                    "metadata": result.metadata,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if results.is_empty() {
        println!("No results found.");
        return Ok(());
    }

    for result in results {
        let title = result
            .title
            .as_deref()
            .or_else(|| {
                result
                    .metadata
                    .get("title")
                    .and_then(|value| value.as_str())
            })
            .unwrap_or("-");
        println!(
            "{}\t{:.3}\t{:.2}\t{}",
            result.note_id, result.score, result.bm25, title
        );
        if let Some(reason) = &result.reason {
            println!("  Reason: {}", reason);
        }
        if let Some(preview) = &result.preview {
            println!("  {}", preview.trim());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, sync::Arc};

    use arrowhead_core::sqlite::IndexDatabase;
    use tempfile::TempDir;

    use crate::commands::CommandContext;
    use crate::config::AppConfig;

    fn write_note(path: &std::path::Path) {
        fs::write(
            path,
            "---\ntitle: Sample Note\ncategory: reference\ntags: [rust, testing]\n---\n\nRust testing fundamentals with SQLite.\n",
        )
        .expect("write note");
    }

    #[tokio::test]
    async fn fts_search_returns_results() {
        let vault_dir = TempDir::new().expect("vault");
        let note_path = vault_dir.path().join("Sample.md");
        write_note(&note_path);

        let ctx = CommandContext::new(
            AppConfig {
                vault: Some(vault_dir.path().to_path_buf()),
                ..AppConfig::default()
            },
            None,
            0,
        );

        let query = QueryArgs {
            query: "category:reference".to_string(),
            limit: Some(5),
            json: false,
        };

        let database = Arc::new(
            IndexDatabase::open(vault_dir.path().join(".arrowhead").join("index.db"))
                .expect("database opens"),
        );
        let vault =
            Arc::new(Vault::new(VaultConfig::new(vault_dir.path().to_path_buf())).expect("vault"));
        vault.ensure_arrowhead_dirs().expect("dirs");
        ensure_index_fresh(Arc::clone(&vault), Arc::clone(&database), None, false)
            .await
            .expect("indexing succeeds");
        let service = SearchService::new(database, SearchConfig::default(), None);
        let results = execute_fts_search(&service, &query)
            .await
            .expect("fts query succeeds");
        assert!(!results.is_empty());

        let command = SearchCommand {
            mode: SearchMode::Fts(query),
        };

        run(&ctx, &command).await.expect("fts search executes");
    }
}
