use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};
use tracing::Level;
use tracing::dispatcher::DefaultGuard;
use tracing_appender::{non_blocking, rolling};

const LOG_FILE_NAME: &str = "cli.log";
const MAX_LOG_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const MAX_LOG_SIZE_BYTES: u64 = 5 * 1024 * 1024;

/// Guard that keeps the logging subscriber and background writer alive for the
/// lifetime of a command.
pub struct LoggingGuard {
    _dispatcher_guard: DefaultGuard,
    _writer_guard: non_blocking::WorkerGuard,
}

/// Initialise a scoped file logger that writes into the supplied directory.
///
/// The subscriber remains active until the returned guard is dropped.
pub fn scoped_file_logging(log_root: &Path, verbosity: u8) -> Result<LoggingGuard> {
    fs::create_dir_all(log_root)
        .with_context(|| format!("failed to create log directory {}", log_root.display()))?;

    prune_old_logs(log_root)?;

    let appender = rolling::never(log_root, LOG_FILE_NAME);
    let (writer, writer_guard) = non_blocking(appender);

    let level = verbosity_to_level(verbosity);

    let subscriber = tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .with_max_level(level)
        .with_ansi(false)
        .compact()
        .with_writer(writer)
        .finish();

    let dispatcher_guard = tracing::subscriber::set_default(subscriber);

    Ok(LoggingGuard {
        _dispatcher_guard: dispatcher_guard,
        _writer_guard: writer_guard,
    })
}

/// Install the process-wide baseline subscriber that keeps stdout clean.
pub fn init_base_tracing(verbosity: u8) {
    let level = verbosity_to_level(verbosity);
    let subscriber = tracing_subscriber::fmt()
        .with_target(false)
        .with_level(true)
        .with_max_level(level)
        .with_ansi(false)
        .compact()
        .with_writer(std::io::sink)
        .finish();

    let _ = tracing::subscriber::set_global_default(subscriber);
}

fn verbosity_to_level(verbosity: u8) -> Level {
    match verbosity {
        0 => Level::INFO,
        1 => Level::DEBUG,
        _ => Level::TRACE,
    }
}

fn prune_old_logs(log_root: &Path) -> Result<()> {
    let now = SystemTime::now();

    for entry in fs::read_dir(log_root)
        .with_context(|| format!("failed to read log directory {}", log_root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let metadata = entry.metadata()?;
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let age = now.duration_since(modified).unwrap_or_default();

        let is_primary = path
            .file_name()
            .map(|name| name == LOG_FILE_NAME)
            .unwrap_or(false);

        let too_old = age > MAX_LOG_AGE;
        let too_large = is_primary && metadata.len() > MAX_LOG_SIZE_BYTES;

        if !is_primary || too_old || too_large {
            let _ = fs::remove_file(&path);
        }
    }

    Ok(())
}
