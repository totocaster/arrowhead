use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};
use tracing::Level;
use tracing::dispatcher::DefaultGuard;
use tracing_appender::{non_blocking, rolling};

const CLI_LOG_FILE_NAME: &str = "cli.log";
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
    scoped_named_file_logging(log_root, verbosity, CLI_LOG_FILE_NAME)
}

/// Initialise a scoped file logger writing to the provided file name.
pub fn scoped_named_file_logging(
    log_root: &Path,
    verbosity: u8,
    file_name: &str,
) -> Result<LoggingGuard> {
    fs::create_dir_all(log_root)
        .with_context(|| format!("failed to create log directory {}", log_root.display()))?;

    prune_old_logs(log_root, file_name)?;

    let appender = rolling::never(log_root, file_name);
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

fn prune_old_logs(log_root: &Path, primary: &str) -> Result<()> {
    let path = log_root.join(primary);
    let metadata = match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to inspect log {}", path.display()));
        }
    };
    let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    if age > MAX_LOG_AGE || metadata.len() > MAX_LOG_SIZE_BYTES {
        let _ = fs::remove_file(&path);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_a_cli_logger_preserves_other_logs_and_expires_its_own() {
        let dir = tempfile::tempdir().expect("log folder");
        let old = SystemTime::now() - MAX_LOG_AGE - Duration::from_secs(60);
        for name in ["cli.log", "daemon.log", "mcp.log", "unrelated.txt"] {
            let path = dir.path().join(name);
            fs::write(&path, "previous log\n").expect("write log");
            fs::File::options()
                .write(true)
                .open(&path)
                .expect("open log")
                .set_times(fs::FileTimes::new().set_modified(old))
                .expect("age log");
        }

        let logger = scoped_file_logging(dir.path(), 0).expect("start logger");
        tracing::info!("current command");
        drop(logger);

        for name in ["daemon.log", "mcp.log", "unrelated.txt"] {
            assert_eq!(
                fs::read_to_string(dir.path().join(name)).expect("other log preserved"),
                "previous log\n"
            );
        }
        let log = fs::read_to_string(dir.path().join("cli.log")).expect("CLI log");
        assert!(log.contains("current command"));
        assert!(!log.contains("previous log"));
    }
}
