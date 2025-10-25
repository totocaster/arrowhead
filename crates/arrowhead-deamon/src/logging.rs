use std::path::Path;

use anyhow::{Context, Result};
use tracing::dispatcher::DefaultGuard;
use tracing_appender::{non_blocking, rolling};
use tracing_subscriber::filter::{EnvFilter, LevelFilter};

/// Guard holding the subscriber and writer for the daemon logger.
pub struct LoggingGuard {
    _dispatcher_guard: DefaultGuard,
    _writer_guard: non_blocking::WorkerGuard,
}

/// Initialise structured logging that writes to the supplied log file.
pub fn init_logging(log_path: &Path) -> Result<LoggingGuard> {
    let parent = log_path
        .parent()
        .context("log path must include a parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create log directory {}", parent.display()))?;

    let file_name = log_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid log file name")?;

    let appender = rolling::never(parent, file_name);
    let (writer, writer_guard) = non_blocking(appender);

    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let subscriber = tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .with_ansi(false)
        .compact()
        .with_writer(writer)
        .with_env_filter(filter)
        .finish();

    let dispatcher_guard = tracing::subscriber::set_default(subscriber);

    Ok(LoggingGuard {
        _dispatcher_guard: dispatcher_guard,
        _writer_guard: writer_guard,
    })
}
