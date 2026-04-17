//! `arrowhead search` subcommands.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::json;
use tracing::info;

use arrowhead_core::{
    SearchConfig, SearchResult, SearchService, Vault, VaultConfig, embeddings::EmbeddingPipeline,
    sqlite::IndexDatabase, status::DaemonStatus,
};
use arrowhead_daemon::{ControlRequest, ControlResponse, send_control_request};

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
    /// Query string to execute (supports boolean operators and date filters).
    pub query: String,
    /// Maximum number of results to return.
    #[arg(long)]
    pub limit: Option<usize>,
    /// Output JSON for machine consumption.
    #[arg(long)]
    pub json: bool,
    /// Include absolute file paths in JSON payloads (requires --json).
    #[arg(long = "include-paths")]
    pub include_paths: bool,
    /// Select an output format optimised for different pipelines.
    #[arg(long, value_enum, default_value_t)]
    pub format: OutputFormat,
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

    let logging_enabled = std::env::var_os("ARROWHEAD_ENABLE_FILE_LOGS").is_some();

    let _logging_guard = if logging_enabled {
        Some(logging::scoped_file_logging(&logs_dir, ctx.verbosity())?)
    } else {
        None
    };

    let selected_model = ctx
        .config
        .embedding_model
        .clone()
        .unwrap_or_else(|| "fast".to_string());
    let embeddings = match &command.mode {
        SearchMode::Fts(_) => None,
        _ => {
            let pipeline = EmbeddingPipeline::initialise(
                vault.as_ref(),
                Arc::clone(&database),
                &selected_model,
            )
            .await
            .with_context(|| format!("failed to prepare embedding pipeline `{selected_model}`"))?;
            Some(Arc::new(pipeline))
        }
    };

    let status = ensure_daemon_ready(ctx, vault.as_ref()).await?;
    if status.indexed_notes == 0 {
        info!("daemon status reports zero indexed notes; search results may be incomplete");
    }
    if status.error_notes > 0 {
        info!(
            errors = status.error_notes,
            "latest daemon run reported indexing errors"
        );
    }

    let service = SearchService::new(database, SearchConfig::default(), embeddings.clone());

    match &command.mode {
        SearchMode::Fts(args) => {
            info!(query = args.query.as_str(), limit = ?args.limit, "executing FTS search");
            let results = execute_fts_search(&service, args).await?;
            render_results(
                &results,
                args.json,
                args.include_paths,
                args.format,
                vault.as_ref(),
            )?;
            Ok(())
        }
        SearchMode::Semantic(args) => {
            let _ = embeddings
                .as_ref()
                .expect("embedding pipeline required for semantic search");
            info!(query = args.query.as_str(), limit = ?args.limit, "executing semantic search");
            let results = service
                .search_semantic(&args.query, args.limit)
                .await
                .context("failed to execute semantic search")?;
            render_results(
                &results,
                args.json,
                args.include_paths,
                args.format,
                vault.as_ref(),
            )?;
            Ok(())
        }
        SearchMode::Hybrid(args) => {
            let _ = embeddings
                .as_ref()
                .expect("embedding pipeline required for hybrid search");
            info!(query = args.query.as_str(), limit = ?args.limit, "executing hybrid search");
            let results = service
                .search_hybrid(&args.query, args.limit)
                .await
                .context("failed to execute hybrid search")?;
            render_results(
                &results,
                args.json,
                args.include_paths,
                args.format,
                vault.as_ref(),
            )?;
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

async fn ensure_daemon_ready(ctx: &CommandContext, vault: &Vault) -> Result<DaemonStatus> {
    let default_socket = vault.paths().arrowhead_dir.join("daemon/control.sock");
    let socket_path = ctx
        .config
        .daemon
        .socket_path
        .clone()
        .unwrap_or(default_socket);

    match send_control_request(&socket_path, ControlRequest::StatusSnapshot).await {
        Ok(ControlResponse::Status { status }) => Ok(status),
        Ok(ControlResponse::Error { message }) => {
            bail!("arrowhead daemon reported an error: {message}")
        }
        Ok(ControlResponse::ShutdownAck) => {
            bail!("arrowhead daemon acknowledged shutdown; restart it with `arrowhead index start`")
        }
        Err(err) => {
            let default_status = vault.paths().arrowhead_dir.join("daemon/status.json");
            let status_path = ctx
                .config
                .daemon
                .status_path
                .clone()
                .unwrap_or(default_status);
            if let Some(status) = DaemonStatus::load_from_path(&status_path)? {
                Err(anyhow!(
                    "arrowhead daemon appears offline (last update {}). Start it with `arrowhead index start` and retry.",
                    status.updated_at.to_rfc3339()
                ))
            } else {
                Err(anyhow!(
                    "arrowhead daemon is not running (socket {} unreachable: {}). Start it with `arrowhead index start` and retry.",
                    socket_path.display(),
                    err
                ))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Human-friendly multi-column output.
    Human,
    /// Emit matching note identifiers, one per line.
    Ids,
    /// Emit absolute note paths, one per line.
    Paths,
}

impl Default for OutputFormat {
    fn default() -> Self {
        Self::Human
    }
}

fn render_results(
    results: &[SearchResult],
    json_output: bool,
    include_paths: bool,
    format: OutputFormat,
    vault: &Vault,
) -> Result<()> {
    if include_paths && !json_output {
        bail!("--include-paths requires --json");
    }

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&search_results_json_payload(
                results,
                include_paths,
                vault
            ))?
        );
        return Ok(());
    }

    if results.is_empty() {
        println!("No results found.");
        return Ok(());
    }

    match format {
        OutputFormat::Human => {
            for result in results {
                let title = result_title(result).unwrap_or("-");
                let bm25_display = result
                    .bm25_score()
                    .map(|score| format!("{score:.2}"))
                    .unwrap_or_else(|| "N/A".to_string());
                println!(
                    "{}\t{:.3}\t{}\t{}",
                    result.note_id, result.score, bm25_display, title
                );
                if let Some(reason) = &result.reason {
                    println!("  Reason: {}", reason);
                }
                if let Some(preview) = &result.preview {
                    println!("  {}", preview.trim());
                }
            }
        }
        OutputFormat::Ids => {
            for result in results {
                println!("{}", result.note_id);
            }
        }
        OutputFormat::Paths => {
            for result in results {
                if let Some(path) = note_absolute_path(vault, result) {
                    println!("{}", path.display());
                } else {
                    println!("{}", result.note_id);
                }
            }
        }
    }

    Ok(())
}

fn search_results_json_payload(
    results: &[SearchResult],
    include_paths: bool,
    vault: &Vault,
) -> serde_json::Value {
    let items = results
        .iter()
        .map(|result| {
            let mut object = json!({
                "id": result.note_id,
                "title": result_title(result),
                "score": result.score,
                "relative_path": result.relative_path,
                "preview": result.preview,
                "reason": result.reason,
            });

            if include_paths {
                if let serde_json::Value::Object(ref mut map) = object {
                    map.insert(
                        "absolute_path".to_string(),
                        json!(
                            note_absolute_path(vault, result)
                                .map(|path| path.display().to_string())
                        ),
                    );
                }
            }

            object
        })
        .collect::<Vec<_>>();

    json!({
        "total": items.len(),
        "results": items,
    })
}

fn result_title(result: &SearchResult) -> Option<&str> {
    result.title.as_deref().or_else(|| {
        result
            .metadata
            .get("title")
            .and_then(|value| value.as_str())
    })
}

fn note_absolute_path(vault: &Vault, result: &SearchResult) -> Option<PathBuf> {
    if let Some(relative) = result.relative_path.as_deref() {
        Some(vault.note_path(relative))
    } else {
        vault.note_file_path(&result.note_id).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::Duration};

    use arrowhead_core::ActivityState;
    use arrowhead_daemon::{DaemonRuntimeBuilder, WatcherStrategy};
    use serde_json::json;
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
    async fn search_errors_when_daemon_missing() {
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
                include_paths: false,
                format: OutputFormat::default(),
            }),
        };

        let err = run(&ctx, &command).await.expect_err("search should fail");
        assert!(
            err.to_string().contains("arrowhead daemon"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn fts_search_with_running_daemon() {
        let vault_dir = TempDir::new().expect("vault");
        let note_path = vault_dir.path().join("Sample.md");
        write_note(&note_path);

        let handle = DaemonRuntimeBuilder::new(vault_dir.path())
            .disable_embeddings()
            .watcher_strategy(WatcherStrategy::Poll {
                interval: Duration::from_millis(50),
            })
            .spawn()
            .await
            .expect("spawn daemon");

        let socket_path = vault_dir.path().join(".arrowhead/daemon/control.sock");
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
                panic!("daemon failed to finish initial indexing in time");
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
                include_paths: false,
                format: OutputFormat::default(),
            }),
        };

        run(&ctx, &command).await.expect("fts search executes");

        handle.shutdown().await.expect("shutdown succeeds");
    }

    #[test]
    fn search_json_payload_is_wrapped_and_normalized() {
        let vault_dir = TempDir::new().expect("vault");
        let vault = Vault::new(VaultConfig::new(vault_dir.path().to_path_buf())).expect("vault");
        let payload = search_results_json_payload(
            &[SearchResult {
                note_id: "Sample".to_string(),
                score: 0.75,
                bm25: 2.0,
                relative_path: Some("Projects/Sample.md".to_string()),
                preview: Some("Preview text".to_string()),
                reason: Some("Hybrid blend".to_string()),
                metadata: [("title".to_string(), json!("Metadata Title"))]
                    .into_iter()
                    .collect(),
                title: None,
            }],
            true,
            &vault,
        );

        assert_eq!(payload.get("total").and_then(|value| value.as_u64()), Some(1));
        let results = payload
            .get("results")
            .and_then(|value| value.as_array())
            .expect("results array");
        assert_eq!(results.len(), 1);

        let item = results[0].as_object().expect("result object");
        assert_eq!(item.get("id"), Some(&json!("Sample")));
        assert_eq!(item.get("title"), Some(&json!("Metadata Title")));
        assert_eq!(item.get("score"), Some(&json!(0.75)));
        assert_eq!(item.get("relative_path"), Some(&json!("Projects/Sample.md")));
        assert_eq!(item.get("preview"), Some(&json!("Preview text")));
        assert_eq!(item.get("reason"), Some(&json!("Hybrid blend")));
        assert_eq!(
            item.get("absolute_path"),
            Some(&json!(
                vault
                    .note_path("Projects/Sample.md")
                    .display()
                    .to_string()
            ))
        );
        assert!(!item.contains_key("note_id"));
        assert!(!item.contains_key("bm25"));
        assert!(!item.contains_key("metadata"));
    }
}
