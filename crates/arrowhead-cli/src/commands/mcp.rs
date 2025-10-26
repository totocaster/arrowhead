//! `arrowhead --mcp` implementation.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use tracing::info;

use arrowhead_mcp::{
    handlers::HandlerRegistry,
    runtime::{McpRuntime, RuntimeOptions},
    stdio::StdioServer,
};

use super::CommandContext;
use crate::logging;

/// Run the MCP stdio server until EOF is observed on stdin.
pub async fn run(ctx: &mut CommandContext) -> Result<()> {
    let vault_path = vault_path(ctx)?;
    let runtime =
        Arc::new(McpRuntime::initialise(build_runtime_options(ctx, vault_path.clone())).await?);

    let logs_dir = runtime.vault().paths().logs_dir();
    let _logging_guard = logging::scoped_file_logging(&logs_dir, ctx.verbosity())?;

    info!(
        vault = %vault_path.display(),
        socket = %runtime.daemon().socket_path().display(),
        "starting MCP stdio server"
    );

    let handler = Arc::new(HandlerRegistry::new(Arc::clone(&runtime)));
    let server = StdioServer::new(handler);
    let metrics = server.metrics();

    server.run().await?;

    let snapshot = metrics.snapshot();
    info!(
        accepted = snapshot.accepted_requests,
        completed = snapshot.completed_requests,
        failed = snapshot.failed_requests,
        rejected = snapshot.rejected_requests,
        notifications = snapshot.notifications_received,
        notification_failures = snapshot.notifications_failed,
        parse_errors = snapshot.parse_errors,
        "MCP stdio server terminated"
    );

    Ok(())
}

fn vault_path(ctx: &CommandContext) -> Result<PathBuf> {
    ctx.config
        .vault
        .clone()
        .context("no vault configured. Provide --vault or run `arrowhead init`.")
}

fn build_runtime_options(ctx: &CommandContext, vault_path: PathBuf) -> RuntimeOptions {
    RuntimeOptions::new(vault_path)
        .with_embedding_model(ctx.config.embedding_model.clone())
        .with_daemon_socket(ctx.config.deamon.socket_path.clone())
        .with_daemon_status(ctx.config.deamon.status_path.clone())
}
