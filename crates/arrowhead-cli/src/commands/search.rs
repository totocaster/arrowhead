//! `arrowhead search` subcommands.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use serde_json::json;
use tracing::info;

use arrowhead_core::{
    SearchConfig, SearchResult, SearchService, Vault, VaultConfig, embeddings::EmbeddingPipeline,
    sqlite::IndexDatabase, status::DeamonStatus,
};
use arrowhead_deamon::{ControlRequest, ControlResponse, send_control_request};

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

    let status = ensure_deamon_ready(ctx, vault.as_ref()).await?;
    if status.indexed_notes == 0 {
        info!("deamon status reports zero indexed notes; search results may be incomplete");
    }
    if status.error_notes > 0 {
        info!(
            errors = status.error_notes,
            "latest deamon run reported indexing errors"
        );
    }

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

async fn ensure_deamon_ready(ctx: &CommandContext, vault: &Vault) -> Result<DeamonStatus> {
    let default_socket = vault.paths().arrowhead_dir.join("deamon/control.sock");
    let socket_path = ctx
        .config
        .deamon
        .socket_path
        .clone()
        .unwrap_or(default_socket);

    match send_control_request(&socket_path, ControlRequest::Status).await {
        Ok(ControlResponse::Status { status }) => Ok(status),
        Ok(ControlResponse::Error { message }) => {
            bail!("arrowhead deamon reported an error: {message}")
        }
        Ok(ControlResponse::ShutdownAck) => {
            bail!("arrowhead deamon acknowledged shutdown; restart it with `arrowhead vault start`")
        }
        Err(err) => {
            let default_status = vault.paths().arrowhead_dir.join("deamon/status.json");
            let status_path = ctx
                .config
                .deamon
                .status_path
                .clone()
                .unwrap_or(default_status);
            if let Some(status) = DeamonStatus::load_from_path(&status_path)? {
                Err(anyhow!(
                    "arrowhead deamon appears offline (last update {}). Start it with `arrowhead vault start` and retry.",
                    status.updated_at.to_rfc3339()
                ))
            } else {
                Err(anyhow!(
                    "arrowhead deamon is not running (socket {} unreachable: {}). Start it with `arrowhead vault start` and retry.",
                    socket_path.display(),
                    err
                ))
            }
        }
    }
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
    use std::{fs, time::Duration};

    use arrowhead_core::ActivityState;
    use arrowhead_deamon::{DeamonRuntimeBuilder, WatcherStrategy};
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
    async fn search_errors_when_deamon_missing() {
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

        let command = SearchCommand {
            mode: SearchMode::Fts(QueryArgs {
                query: "category:reference".to_string(),
                limit: Some(5),
                json: false,
            }),
        };

        let err = run(&ctx, &command).await.expect_err("search should fail");
        assert!(
            err.to_string().contains("arrowhead deamon"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn fts_search_with_running_deamon() {
        let vault_dir = TempDir::new().expect("vault");
        let note_path = vault_dir.path().join("Sample.md");
        write_note(&note_path);

        let handle = DeamonRuntimeBuilder::new(vault_dir.path())
            .watcher_strategy(WatcherStrategy::Poll {
                interval: Duration::from_millis(50),
            })
            .spawn()
            .await
            .expect("spawn deamon");

        let socket_path = vault_dir.path().join(".arrowhead/deamon/control.sock");
        let mut wait_attempts = 0;
        while !socket_path.exists() {
            wait_attempts += 1;
            if wait_attempts > 100 {
                panic!("control socket did not appear in time");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Wait for the initial indexing pass to complete.
        let mut attempts = 0;
        loop {
            let status = handle.request_status().await.expect("status available");
            if status.activity.state == ActivityState::Idle && status.indexed_notes >= 1 {
                break;
            }
            attempts += 1;
            if attempts > 100 {
                panic!("deamon failed to finish initial indexing in time");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let ctx = CommandContext::new(
            AppConfig {
                vault: Some(vault_dir.path().to_path_buf()),
                ..AppConfig::default()
            },
            None,
            0,
        );

        let command = SearchCommand {
            mode: SearchMode::Fts(QueryArgs {
                query: "category:reference".to_string(),
                limit: Some(5),
                json: false,
            }),
        };

        run(&ctx, &command).await.expect("fts search executes");

        handle.shutdown().await.expect("shutdown succeeds");
    }
}
