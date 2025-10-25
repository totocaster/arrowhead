use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use anyhow::{Context, Result};
use tracing::info;
use tracing_subscriber::{
    filter::{EnvFilter, LevelFilter},
    fmt::writer::MakeWriter,
};

#[derive(Clone)]
struct VaultLogWriter {
    path: Arc<Mutex<PathBuf>>,
}

impl<'a> MakeWriter<'a> for VaultLogWriter {
    type Writer = Box<dyn Write + Send + 'a>;

    fn make_writer(&'a self) -> Self::Writer {
        let path = self.path.lock().expect("log path mutex poisoned").clone();
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => Box::new(file),
            Err(err) => {
                eprintln!("failed to open daemon log {}: {err}", path.display());
                Box::new(io::stderr())
            }
        }
    }
}

struct LoggingState {
    path: Arc<Mutex<PathBuf>>,
}

static LOGGING_STATE: OnceLock<LoggingState> = OnceLock::new();

/// Guard type retained by the runtime to keep logging configured for the process lifetime.
#[derive(Debug, Default)]
pub struct LoggingGuard;

/// Initialise structured logging that writes to the supplied log file.
pub fn init_logging(log_path: &Path) -> Result<LoggingGuard> {
    let parent = log_path
        .parent()
        .context("log path must include a parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create log directory {}", parent.display()))?;

    let canonical = log_path.to_path_buf();

    let state = if let Some(state) = LOGGING_STATE.get() {
        state
    } else {
        let path = Arc::new(Mutex::new(PathBuf::new()));
        let writer = VaultLogWriter {
            path: Arc::clone(&path),
        };

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

        if let Err(err) = tracing::subscriber::set_global_default(subscriber) {
            eprintln!("global tracing subscriber already installed: {err}");
        }

        if let Err(existing) = LOGGING_STATE.set(LoggingState { path }) {
            drop(existing);
        }
        LOGGING_STATE.get().expect("logging state initialised")
    };

    {
        let mut guard = state.path.lock().expect("log path mutex poisoned");
        *guard = canonical.clone();
    }

    info!(path = %canonical.display(), "daemon logging initialised");

    Ok(LoggingGuard)
}
