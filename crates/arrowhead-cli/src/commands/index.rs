//! `arrowhead index` subcommands.

use std::{
    env, fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use arrowhead_core::{
    ActivityState, DaemonStatus, DownloadState, IssueSeverity, StatusFrame, Vault, VaultConfig,
};
use arrowhead_daemon::{
    ControlRequest, ControlResponse, StatusStream, send_control_request, status_stream,
};
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use serde_json;
use tokio::{
    signal,
    time::{Instant, sleep},
};
use tracing::{info, warn};

use super::CommandContext;
use crate::autostart::{
    AUTOSTART_DIR, AutoStartManager, AutoStartProvider, AutoStartStatus, MANIFEST_FILE,
    prompt_yes_no,
};
use crate::config::{DaemonConfig, DaemonStatusSummary};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
enum StartupWaitOutcome {
    Ready(Option<DaemonStatus>),
    Pending(Option<DaemonStatus>),
}

impl StartupWaitOutcome {
    fn status(&self) -> Option<&DaemonStatus> {
        match self {
            Self::Ready(status) | Self::Pending(status) => status.as_ref(),
        }
    }
}

/// Background index management entry point.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct IndexCommand {
    /// Specific indexer action to run.
    #[command(subcommand)]
    pub action: IndexAction,
}

/// Supported indexer subcommands.
#[derive(Debug, Subcommand, Clone, PartialEq)]
pub enum IndexAction {
    /// Launch the background indexer, spawning it if necessary.
    Start(IndexStartArgs),
    /// Send a shutdown signal to the indexer.
    Stop,
    /// Restart the indexer by issuing a stop followed by start.
    Restart(IndexStartArgs),
    /// Stream live indexer status updates.
    Status(IndexStatusArgs),
    /// Manage auto-start integration for the indexer.
    Autostart(IndexAutostartCommand),
}

/// Options for `index start`.
#[derive(Debug, Args, Clone, PartialEq, Eq)]
pub struct IndexStartArgs {
    /// Do not wait for the control socket to appear after spawning.
    #[arg(long)]
    pub no_wait: bool,
}

/// Options for `index status`.
#[derive(Debug, Args, Clone, PartialEq, Eq)]
pub struct IndexStatusArgs {
    /// Emit newline-delimited JSON frames instead of human output.
    #[arg(long)]
    pub json: bool,
}

/// Options for `index autostart`.
#[derive(Debug, Args, Clone, PartialEq, Eq)]
pub struct IndexAutostartCommand {
    /// Auto-start operation to perform.
    #[command(subcommand)]
    pub action: IndexAutostartAction,
}

/// Supported auto-start operations.
#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
pub enum IndexAutostartAction {
    /// Install and enable auto-start for this vault.
    Enable,
    /// Disable auto-start and remove installed units.
    Disable,
    /// Display the current auto-start status.
    Status,
}

/// Initialise options consumed by `arrowhead init`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitOptions {
    pub force: bool,
    pub no_start: bool,
    pub fts_only: bool,
}

/// Execute the index command.
pub async fn run(ctx: &mut CommandContext, command: &IndexCommand) -> Result<()> {
    match &command.action {
        IndexAction::Start(args) => handle_start(ctx, args).await?,
        IndexAction::Stop => handle_stop(ctx).await?,
        IndexAction::Restart(args) => handle_restart(ctx, args).await?,
        IndexAction::Status(args) => handle_status(ctx, args).await?,
        IndexAction::Autostart(command) => handle_autostart(ctx, command).await?,
    }

    Ok(())
}

/// Prepare the vault for indexing, install auto-start (when desired), and optionally launch the indexer.
pub(crate) async fn initialise_vault(ctx: &mut CommandContext, options: InitOptions) -> Result<()> {
    let vault_path = resolve_vault_path(ctx)?;
    if !vault_path.exists() {
        bail!(
            "vault directory {} does not exist (use `arrowhead init --force` to create it)",
            vault_path.display()
        );
    }

    let (vault, paths) = load_vault_environment(&vault_path)?;

    let index_path = paths.arrowhead_dir.join("index.db");
    let already_initialised = [
        paths.status_path.as_path(),
        paths.socket_path.as_path(),
        paths.pid_path.as_path(),
        paths.autostart_manifest_path.as_path(),
        index_path.as_path(),
    ]
    .iter()
    .any(|path| path.exists());

    if already_initialised && !options.force {
        bail!(
            "Arrowhead is already initialised for this vault. Re-run with --force to reinitialise."
        );
    }

    if options.force {
        cleanup_auto_start(&paths)?;
        cleanup_arrowhead_dirs(&paths)?;
    }

    ensure_runtime_dirs(&vault, &paths)?;
    println!("initialising arrowhead indexer…");

    ctx.config.vault = Some(vault.paths().root.clone());
    if options.fts_only {
        ctx.config.embedding_model = None;
        info!("configured vault for full-text search only");
    }

    let manager = AutoStartManager::detect(paths.autostart_manifest_path.clone());
    let mut manifest = match manager.as_ref() {
        Some(manager) => manager.load_manifest()?,
        None => None,
    };

    let mut auto_start_preference = ctx.config.daemon.auto_start_enabled;

    if options.no_start {
        if manifest.is_some() {
            auto_start_preference = Some(true);
        } else if auto_start_preference.is_none() {
            auto_start_preference = Some(false);
        }

        ctx.config.daemon = DaemonConfig {
            socket_path: Some(paths.socket_path.clone()),
            status_path: Some(paths.status_path.clone()),
            auto_start_enabled: auto_start_preference,
            last_status: None,
        };
        ctx.persist()?;
        println!(
            "Arrowhead directories initialised at {}. Start the indexer with `arrowhead index start` when ready.",
            paths.arrowhead_dir.display()
        );
        return Ok(());
    }

    if is_socket_alive(&paths).await? {
        println!("arrowhead indexer already running; skipping launch");
        if manifest.is_some() {
            auto_start_preference = Some(true);
        } else if auto_start_preference.is_none() {
            auto_start_preference = Some(false);
        }
        ctx.config.daemon.auto_start_enabled = auto_start_preference;
        update_config_with_status(ctx, &paths, None)?;
        return Ok(());
    }

    if manifest.is_none() {
        if let Some(manager) = &manager {
            let enable = if let Some(preference) = auto_start_preference {
                preference
            } else {
                prompt_yes_no(
                    "Enable Arrowhead auto-start so the indexer launches automatically on login?",
                )?
                .unwrap_or_default()
            };

            if enable {
                let binary = find_daemon_binary()?;
                manifest = Some(
                    manager
                        .install(&vault_path, &binary, ctx.config.embedding_model.as_deref())
                        .context("failed to install auto-start service")?,
                );
                println!("Auto-start configured; Arrowhead will launch on login.");
                auto_start_preference = Some(true);
            } else {
                auto_start_preference = Some(false);
            }
        } else if auto_start_preference.is_none() {
            println!(
                "Auto-start is not supported on this platform; the indexer must be started manually."
            );
            auto_start_preference = Some(false);
        }
    } else {
        auto_start_preference = Some(true);
    }

    ctx.config.daemon.auto_start_enabled = auto_start_preference;

    let launched_at = Utc::now();
    let mut pid: Option<u32> = None;
    if let (Some(manager), Some(manifest)) = (&manager, manifest.as_ref()) {
        match manager.start_unit(manifest) {
            Ok(reported_pid) => {
                pid = reported_pid;
            }
            Err(err) => {
                println!(
                    "failed to start auto-start manager ({err}); falling back to direct spawn"
                );
            }
        }
    }

    if pid.is_none() {
        let spawned_pid =
            launch_daemon_process(&vault_path, ctx.config.embedding_model.as_deref())?;
        pid = Some(spawned_pid);
        if ctx.config.daemon.auto_start_enabled.is_none() {
            ctx.config.daemon.auto_start_enabled = Some(false);
        }
    }

    if let Some(actual_pid) = pid {
        write_pid_file(&paths.pid_path, actual_pid)?;
    } else {
        remove_pid_file(&paths.pid_path)?;
    }

    let startup = wait_for_startup(&paths, launched_at, STARTUP_TIMEOUT).await?;
    let status = startup.status().cloned();
    update_config_with_status(ctx, &paths, status.clone())?;
    print_start_result(
        &paths,
        pid,
        startup,
        "Arrowhead initialised and indexer launch requested.",
    );

    Ok(())
}

async fn handle_start(ctx: &mut CommandContext, args: &IndexStartArgs) -> Result<()> {
    let vault_path = resolve_vault_path(ctx)?;
    let (vault, paths) = load_vault_environment(&vault_path)?;

    if is_socket_alive(&paths).await? {
        bail!("arrowhead indexer already running for this vault");
    }

    remove_stale_runtime_metadata(&paths)?;

    ensure_runtime_dirs(&vault, &paths)?;
    println!("initialising arrowhead indexer…");

    let manager = AutoStartManager::detect(paths.autostart_manifest_path.clone());
    let launched_at = Utc::now();
    let mut pid: Option<u32> = None;

    if let Some(manager) = &manager {
        if let Some(manifest) = manager.load_manifest()? {
            match manager.start_unit(&manifest) {
                Ok(reported_pid) => {
                    pid = reported_pid;
                    ctx.config.daemon.auto_start_enabled = Some(true);
                }
                Err(err) => {
                    println!(
                        "failed to start auto-start service ({err}); falling back to direct spawn"
                    );
                }
            }
        }
    }

    if pid.is_none() {
        let spawned_pid =
            launch_daemon_process(&vault_path, ctx.config.embedding_model.as_deref())?;
        pid = Some(spawned_pid);
        if ctx.config.daemon.auto_start_enabled.is_none() {
            ctx.config.daemon.auto_start_enabled = Some(false);
        }
    }

    if let Some(actual_pid) = pid {
        write_pid_file(&paths.pid_path, actual_pid)?;
    } else {
        remove_pid_file(&paths.pid_path)?;
    }

    let status = if args.no_wait {
        None
    } else {
        let startup = wait_for_startup(&paths, launched_at, STARTUP_TIMEOUT).await?;
        let status = startup.status().cloned();
        print_start_result(&paths, pid, startup, "Arrowhead indexer launch requested.");
        status
    };

    update_config_with_status(ctx, &paths, status)?;

    if args.no_wait {
        if let Some(actual_pid) = pid {
            println!("Arrowhead indexer launch requested (pid {actual_pid}).");
        } else {
            println!("Arrowhead indexer launch requested.");
        }
        print_status_follow_up(&paths);
    }

    Ok(())
}

async fn handle_stop(ctx: &mut CommandContext) -> Result<()> {
    let vault_path = resolve_vault_path(ctx)?;
    let (_vault, paths) = load_vault_environment(&vault_path)?;

    match send_control_request(&paths.socket_path, ControlRequest::Shutdown).await {
        Ok(ControlResponse::ShutdownAck) => {
            wait_for_socket_removal(&paths.socket_path, SHUTDOWN_TIMEOUT).await?;
            println!("shutdown signal sent to arrowhead indexer");
        }
        Ok(other) => {
            bail!("unexpected response from indexer: {other:?}");
        }
        Err(err) => {
            if paths.socket_path.exists() {
                println!(
                    "control socket exists but the indexer is unreachable; removing stale socket ({err})"
                );
                remove_stale_socket(&paths.socket_path)?;
            } else {
                println!("no active indexer detected; cleaning up stale metadata ({err})");
            }
        }
    }

    if let Some(pid) = read_pid_file(&paths.pid_path)? {
        println!("removed PID file for process {pid}");
    }
    remove_pid_file(&paths.pid_path)?;

    update_config_with_status(ctx, &paths, None)?;

    Ok(())
}

async fn handle_restart(ctx: &mut CommandContext, args: &IndexStartArgs) -> Result<()> {
    if let Err(err) = handle_stop(ctx).await {
        warn!("failed to stop indexer gracefully before restart: {err}");
    }
    handle_start(ctx, args).await
}

mod status_ui;

async fn handle_status(ctx: &CommandContext, args: &IndexStatusArgs) -> Result<()> {
    let vault_path = resolve_vault_path(ctx)?;
    let vault = Vault::new(VaultConfig::new(vault_path.clone()))?;
    vault.ensure_arrowhead_dirs()?;

    let paths = vault.paths();
    let socket_path = paths.arrowhead_dir.join("daemon/control.sock");
    let status_path = paths.arrowhead_dir.join("daemon/status.json");
    let stdout_is_tty = io::stdout().is_terminal();
    let snapshot = DaemonStatus::load_from_path(&status_path)?;

    match status_stream(&socket_path).await {
        Ok(mut stream) => {
            if !args.json && !stdout_is_tty {
                println!(
                    "Streaming indexer status from {} (Ctrl+C to exit).\n",
                    socket_path.display()
                );
            }
            stream_frames(&mut stream, args, stdout_is_tty, snapshot.clone()).await
        }
        Err(err) => {
            if let Some(status) = snapshot {
                if args.json {
                    let frame = StatusFrame::new(status);
                    println!("{}", serde_json::to_string(&frame)?);
                } else if stdout_is_tty {
                    status_ui::run_status_ui(None, Some(status)).await?;
                } else {
                    println!("Indexer stream unavailable ({err}). Showing latest snapshot.\n");
                    render_snapshot(&status, stdout_is_tty);
                }
                Ok(())
            } else {
                Err(err.context("failed to connect to indexer status stream"))
            }
        }
    }
}

async fn handle_autostart(ctx: &mut CommandContext, command: &IndexAutostartCommand) -> Result<()> {
    let vault_path = resolve_vault_path(ctx)?;
    let (vault, paths) = load_vault_environment(&vault_path)?;
    ctx.config.vault = Some(vault.paths().root.clone());

    match command.action {
        IndexAutostartAction::Enable => {
            ensure_runtime_dirs(&vault, &paths)?;

            let manager = match AutoStartManager::detect(paths.autostart_manifest_path.clone()) {
                Some(manager) => manager,
                None => {
                    println!("Auto-start is not supported on this platform.");
                    ctx.config.daemon.auto_start_enabled = Some(false);
                    ctx.persist()?;
                    return Ok(());
                }
            };

            let binary = find_daemon_binary()?;
            let embedding_model = ctx.config.embedding_model.as_deref();
            let manifest = manager.load_manifest()?;

            let manifest = match manifest {
                Some(existing) => {
                    println!(
                        "Auto-start already configured via {}.",
                        provider_label(existing.provider)
                    );
                    existing
                }
                None => {
                    let installed = manager
                        .install(&vault_path, &binary, embedding_model)
                        .context("failed to install auto-start service")?;
                    println!(
                        "Auto-start enabled via {}.",
                        provider_label(installed.provider)
                    );
                    installed
                }
            };

            if let Err(err) = manager.start_unit(&manifest) {
                println!(
                    "failed to start auto-start manager immediately ({err}); service will launch on next login"
                );
            }

            ctx.config.daemon.auto_start_enabled = Some(true);
            update_config_with_status(ctx, &paths, None)?;
            ctx.persist()?;
        }
        IndexAutostartAction::Disable => {
            let manager = match AutoStartManager::detect(paths.autostart_manifest_path.clone()) {
                Some(manager) => manager,
                None => {
                    println!("Auto-start is not supported on this platform.");
                    ctx.config.daemon.auto_start_enabled = Some(false);
                    ctx.persist()?;
                    return Ok(());
                }
            };

            if let Some(manifest) = manager.load_manifest()? {
                manager.uninstall(&manifest)?;
                manager.remove_manifest()?;
                println!(
                    "Auto-start disabled (removed {}).",
                    provider_label(manifest.provider)
                );
            } else {
                println!("Auto-start is already disabled for this vault.");
            }

            ctx.config.daemon.auto_start_enabled = Some(false);
            update_config_with_status(ctx, &paths, None)?;
            ctx.persist()?;
        }
        IndexAutostartAction::Status => {
            let status = auto_start_status(&paths)?;
            print_autostart_status(&status);
        }
    }

    Ok(())
}

/// Remove Arrowhead caches and reset daemon configuration.
pub(crate) async fn handle_reset(ctx: &mut CommandContext) -> Result<()> {
    let vault_path = resolve_vault_path(ctx)?;
    let (_vault, paths) = load_vault_environment(&vault_path)?;

    if paths.socket_path.exists() {
        match send_control_request(&paths.socket_path, ControlRequest::Shutdown).await {
            Ok(ControlResponse::ShutdownAck) => {
                wait_for_socket_removal(&paths.socket_path, SHUTDOWN_TIMEOUT).await?;
                println!("shutdown signal sent to arrowhead indexer");
            }
            Ok(ControlResponse::Error { message }) => {
                bail!("indexer reported an error during shutdown: {message}");
            }
            Ok(other) => {
                bail!("unexpected response from indexer: {other:?}");
            }
            Err(err) => {
                if paths.socket_path.exists() {
                    println!("failed to contact arrowhead indexer ({err}); continuing with reset");
                }
            }
        }
    }

    if let Some(pid) = read_pid_file(&paths.pid_path)? {
        println!("removed PID file for process {pid}");
    }
    remove_pid_file(&paths.pid_path)?;

    cleanup_auto_start(&paths)?;
    cleanup_arrowhead_dirs(&paths)?;

    ctx.config.daemon = DaemonConfig::default();
    ctx.persist()?;

    println!(
        "arrowhead caches removed from {}",
        paths.arrowhead_dir.display()
    );

    Ok(())
}

pub(crate) fn resolve_vault_path(ctx: &CommandContext) -> Result<PathBuf> {
    ctx.config
        .vault
        .clone()
        .context("no vault configured. Provide --vault or run `arrowhead init` first")
}

pub(crate) fn load_vault_environment(vault_path: &Path) -> Result<(Vault, DaemonPaths)> {
    let vault = Vault::new(VaultConfig::new(vault_path.to_path_buf()))?;
    let vault_paths = vault.paths().clone();
    let daemon_dir = vault_paths.arrowhead_dir.join("daemon");
    let autostart_dir = daemon_dir.join(AUTOSTART_DIR);
    let paths = DaemonPaths {
        arrowhead_dir: vault_paths.arrowhead_dir.clone(),
        daemon_dir,
        autostart_dir: autostart_dir.clone(),
        autostart_manifest_path: autostart_dir.join(MANIFEST_FILE),
        status_path: vault_paths.arrowhead_dir.join("daemon/status.json"),
        socket_path: vault_paths.arrowhead_dir.join("daemon/control.sock"),
        pid_path: vault_paths.arrowhead_dir.join("daemon/daemon.pid"),
        log_path: vault_paths.logs_dir().join("daemon.log"),
    };

    Ok((vault, paths))
}

#[derive(Debug, Clone)]
pub(crate) struct DaemonPaths {
    pub arrowhead_dir: PathBuf,
    pub daemon_dir: PathBuf,
    pub autostart_dir: PathBuf,
    pub autostart_manifest_path: PathBuf,
    pub status_path: PathBuf,
    pub socket_path: PathBuf,
    pub pid_path: PathBuf,
    pub log_path: PathBuf,
}

async fn fetch_status(paths: &DaemonPaths) -> Result<Option<DaemonStatus>> {
    match send_control_request(&paths.socket_path, ControlRequest::StatusSnapshot).await {
        Ok(ControlResponse::Status { status }) => Ok(Some(status)),
        Ok(ControlResponse::Error { message }) => bail!(message),
        Ok(ControlResponse::ShutdownAck) => Ok(None),
        Err(err) => {
            if let Some(status) = DaemonStatus::load_from_path(&paths.status_path)? {
                return Ok(Some(status));
            }
            if paths.socket_path.exists() {
                return Err(err.context("failed to query arrowhead indexer status"));
            }
            Ok(None)
        }
    }
}

async fn is_socket_alive(paths: &DaemonPaths) -> Result<bool> {
    match send_control_request(&paths.socket_path, ControlRequest::StatusSnapshot).await {
        Ok(ControlResponse::Status { .. }) => Ok(true),
        Ok(ControlResponse::Error { .. }) => Ok(true),
        Ok(ControlResponse::ShutdownAck) => Ok(false),
        Err(_) => Ok(false),
    }
}

fn auto_start_status(paths: &DaemonPaths) -> Result<AutoStartStatus> {
    if let Some(manager) = AutoStartManager::detect(paths.autostart_manifest_path.clone()) {
        if let Some(manifest) = manager.load_manifest()? {
            manager.query_status(&manifest)
        } else {
            Ok(AutoStartStatus::Disabled {
                provider: manager.provider(),
            })
        }
    } else {
        Ok(AutoStartStatus::Unsupported)
    }
}

fn provider_label(provider: AutoStartProvider) -> &'static str {
    match provider {
        AutoStartProvider::Launchd => "launchd",
        AutoStartProvider::SystemdUser => "systemd --user",
    }
}

fn print_autostart_status(status: &AutoStartStatus) {
    match status {
        AutoStartStatus::Enabled { provider, active } => {
            let state = if *active { "active" } else { "inactive" };
            println!(
                "Auto-start enabled via {} ({}).",
                provider_label(*provider),
                state
            );
        }
        AutoStartStatus::Disabled { provider } => {
            println!(
                "Auto-start is installed but disabled ({}).",
                provider_label(*provider)
            );
        }
        AutoStartStatus::Unsupported => {
            println!("Auto-start is not available on this platform.");
        }
    }
}

fn ensure_runtime_dirs(vault: &Vault, paths: &DaemonPaths) -> Result<()> {
    vault.ensure_arrowhead_dirs()?;
    fs::create_dir_all(&paths.daemon_dir).with_context(|| {
        format!(
            "failed to create indexer directory {}",
            paths.daemon_dir.display()
        )
    })?;
    fs::create_dir_all(&paths.autostart_dir).with_context(|| {
        format!(
            "failed to create auto-start directory {}",
            paths.autostart_dir.display()
        )
    })?;
    if let Some(parent) = paths.log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory {}", parent.display()))?;
    }
    Ok(())
}

fn cleanup_auto_start(paths: &DaemonPaths) -> Result<()> {
    if let Some(manager) = AutoStartManager::detect(paths.autostart_manifest_path.clone()) {
        if let Some(manifest) = manager.load_manifest()? {
            manager.uninstall(&manifest)?;
            manager.remove_manifest()?;
        }
    }
    Ok(())
}

fn cleanup_arrowhead_dirs(paths: &DaemonPaths) -> Result<()> {
    if paths.arrowhead_dir.exists() {
        fs::remove_dir_all(&paths.arrowhead_dir)
            .with_context(|| format!("failed to remove {}", paths.arrowhead_dir.display()))?
    }
    Ok(())
}

fn update_config_with_status(
    ctx: &mut CommandContext,
    paths: &DaemonPaths,
    status: Option<DaemonStatus>,
) -> Result<()> {
    ctx.config.daemon.socket_path = Some(paths.socket_path.clone());
    ctx.config.daemon.status_path = Some(paths.status_path.clone());
    ctx.config.daemon.last_status = status.map(|value| DaemonStatusSummary {
        updated_at: value.updated_at,
        state: value.activity.state,
        indexed_notes: value.indexed_notes,
        error_notes: value.error_notes,
    });
    ctx.persist()
}

fn launch_daemon_process(vault_path: &Path, embedding_model: Option<&str>) -> Result<u32> {
    let binary = find_daemon_binary()?;

    let mut command = Command::new(binary);
    configure_daemon_command(&mut command, vault_path, embedding_model);

    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());

    let child = command
        .spawn()
        .context("failed to launch arrowhead indexer")?;
    Ok(child.id())
}

fn configure_daemon_command(
    command: &mut Command,
    vault_path: &Path,
    embedding_model: Option<&str>,
) {
    let model = embedding_model.unwrap_or("none");
    command.arg("--vault").arg(vault_path);
    command.arg("--embedding-model").arg(model);
    command.env("ARROWHEAD_VAULT", vault_path);
    command.env("ARROWHEAD_EMBEDDING_MODEL", model);
}

fn find_daemon_binary() -> Result<PathBuf> {
    if let Some(path) =
        env::var_os("ARROWHEAD_DAEMON_PATH").or_else(|| env::var_os("ARROWHEADD_PATH"))
    {
        return Ok(PathBuf::from(path));
    }

    let candidate_names = [
        format!("arrowheadd{}", std::env::consts::EXE_SUFFIX),
        format!("arrowhead-daemon{}", std::env::consts::EXE_SUFFIX),
    ];

    if let Ok(current) = env::current_exe() {
        if let Some(dir) = current.parent() {
            for name in &candidate_names {
                let candidate = dir.join(name);
                if candidate.exists() {
                    return Ok(candidate);
                }
            }
        }
    }

    for dir in env::var("PATH").unwrap_or_default().split(':') {
        for name in &candidate_names {
            let candidate = Path::new(dir).join(name);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    bail!("unable to locate arrowheadd binary (set ARROWHEAD_DAEMON_PATH)")
}

fn write_pid_file(path: &Path, pid: u32) -> Result<()> {
    fs::write(path, pid.to_string()).with_context(|| format!("failed to write PID file {path:?}"))
}

fn read_pid_file(path: &Path) -> Result<Option<u32>> {
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read PID file {path:?}"))?;
    Ok(content.trim().parse().ok())
}

fn remove_pid_file(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("failed to remove PID file {path:?}"))?;
    }
    Ok(())
}

async fn wait_for_socket_removal(path: &Path, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while path.exists() {
        if start.elapsed() > timeout {
            bail!(
                "timed out waiting for control socket removal {}",
                path.display()
            );
        }
        sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

async fn wait_for_startup(
    paths: &DaemonPaths,
    launched_at: DateTime<Utc>,
    timeout: Duration,
) -> Result<StartupWaitOutcome> {
    let start = Instant::now();
    let mut last_status = None;
    let mut last_reported_state = None;
    let mut announced_wait = false;

    loop {
        let live = is_socket_alive(paths).await?;
        if let Some(status) = load_fresh_status(paths, launched_at).await? {
            if last_reported_state != Some(status.activity.state) {
                println!("{}", startup_status_line(&status));
                last_reported_state = Some(status.activity.state);
            }
            last_status = Some(status);
        } else if !announced_wait && start.elapsed() >= Duration::from_secs(1) {
            println!(
                "Waiting for the Arrowhead control socket at {}…",
                paths.socket_path.display()
            );
            announced_wait = true;
        }

        if live {
            return Ok(StartupWaitOutcome::Ready(last_status));
        }

        if start.elapsed() > timeout {
            return Ok(StartupWaitOutcome::Pending(last_status));
        }

        sleep(Duration::from_millis(200)).await;
    }
}

async fn load_fresh_status(
    paths: &DaemonPaths,
    launched_at: DateTime<Utc>,
) -> Result<Option<DaemonStatus>> {
    let Some(status) = fetch_status(paths).await? else {
        return Ok(None);
    };
    if status.updated_at < launched_at {
        return Ok(None);
    }
    Ok(Some(status))
}

fn print_start_result(
    paths: &DaemonPaths,
    pid: Option<u32>,
    startup: StartupWaitOutcome,
    ready_message: &str,
) {
    match startup {
        StartupWaitOutcome::Ready(status) => {
            if let Some(actual_pid) = pid {
                println!("{ready_message} (pid {actual_pid}).");
            } else {
                println!("{ready_message}");
            }
            if let Some(status) = status {
                println!("Current state: {}", startup_status_brief(&status));
            }
            print_status_follow_up(paths);
        }
        StartupWaitOutcome::Pending(status) => {
            if let Some(actual_pid) = pid {
                println!(
                    "Arrowhead daemon is still starting in the background (pid {actual_pid}); the control socket is not ready yet."
                );
            } else {
                println!(
                    "Arrowhead daemon is still starting in the background; the control socket is not ready yet."
                );
            }
            if let Some(status) = status {
                println!("Latest state: {}", startup_status_brief(&status));
            } else {
                println!("Latest state: no fresh daemon status yet.");
            }
            print_status_follow_up(paths);
        }
    }
}

fn print_status_follow_up(paths: &DaemonPaths) {
    println!("Control socket: {}", paths.socket_path.display());
    println!("Status file: {}", paths.status_path.display());
    println!("Log: {}", paths.log_path.display());
    println!("Watch progress: `arrowhead index status` or `arrowhead index status --json`.");
}

fn startup_status_line(status: &DaemonStatus) -> String {
    match status.activity.state {
        ActivityState::Starting => "Arrowhead daemon is starting up.".to_string(),
        ActivityState::Downloading => {
            let detail = status
                .activity
                .description
                .clone()
                .unwrap_or_else(|| "downloading embeddings".to_string());
            format!("Arrowhead daemon is {detail}.")
        }
        ActivityState::Indexing => {
            let detail = status
                .activity
                .description
                .clone()
                .unwrap_or_else(|| "building the initial index".to_string());
            format!("Arrowhead daemon is {detail}.")
        }
        ActivityState::Removing => {
            "Arrowhead daemon is reconciling stale index entries.".to_string()
        }
        ActivityState::Faulted => "Arrowhead daemon reported a startup issue.".to_string(),
        ActivityState::Idle => "Arrowhead daemon is idle.".to_string(),
    }
}

fn startup_status_brief(status: &DaemonStatus) -> String {
    status
        .activity
        .description
        .clone()
        .unwrap_or_else(|| describe_activity(status.activity.state).to_string())
}

fn remove_stale_runtime_metadata(paths: &DaemonPaths) -> Result<()> {
    if paths.socket_path.exists() {
        remove_stale_socket(&paths.socket_path)?;
    }
    if let Some(pid) = read_pid_file(&paths.pid_path)? {
        println!("Removing stale PID file for process {pid}.");
        remove_pid_file(&paths.pid_path)?;
    }
    Ok(())
}

fn remove_stale_socket(path: &Path) -> Result<()> {
    fs::remove_file(path)
        .with_context(|| format!("failed to remove stale control socket {}", path.display()))
}

async fn stream_frames(
    stream: &mut StatusStream,
    args: &IndexStatusArgs,
    tty: bool,
    initial_status: Option<DaemonStatus>,
) -> Result<()> {
    if !args.json && tty {
        return status_ui::run_status_ui(Some(stream), initial_status).await;
    }

    loop {
        tokio::select! {
            biased;
            _ = signal::ctrl_c() => {
                if !args.json {
                    println!("\nReceived Ctrl+C. Stopping status stream.");
                }
                break;
            }
            frame = stream.next() => {
                match frame? {
                    Some(frame) => {
                        if args.json {
                            println!("{}", serde_json::to_string(&frame)?);
                        } else {
                            render_frame(&frame, tty)?;
                        }
                    }
                    None => {
                        if !args.json {
                            println!("Indexer closed the status stream.");
                        }
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}

fn render_frame(frame: &StatusFrame, tty: bool) -> Result<()> {
    if tty {
        print!("\u{001b}[2J\u{001b}[H");
    } else {
        println!();
        println!("[{}]", frame.emitted_at.to_rfc3339());
    }

    render_snapshot(&frame.status, tty);
    io::stdout().flush().ok();
    Ok(())
}

fn render_snapshot(status: &DaemonStatus, tty: bool) {
    if !tty {
        println!("Updated: {}", status.updated_at.to_rfc3339());
    } else {
        println!("arrowhead indexer status");
        println!("======================");
        println!("Updated: {}", status.updated_at.to_rfc3339());
    }
    let activity_label = status
        .activity
        .description
        .as_deref()
        .unwrap_or_else(|| describe_activity(status.activity.state));
    println!("Activity: {activity_label}");
    if let Some(note_id) = &status.activity.note_id {
        println!("  Note: {note_id}");
    }
    if status.activity.queued_jobs > 0 {
        println!("  Queue: {}", status.activity.queued_jobs);
    }
    println!(
        "Indexed notes: {} (errors: {})",
        status.indexed_notes, status.error_notes
    );

    if !status.downloads.is_empty() {
        println!("Downloads:");
        for download in &status.downloads {
            let total = download
                .bytes_total
                .map(|value| value.to_string())
                .unwrap_or_else(|| "?".to_string());
            let message = download
                .message
                .as_ref()
                .map(|value| format!(" ({value})"))
                .unwrap_or_default();
            println!(
                "  - {} [{}] {}/{}{}",
                download.item,
                describe_download_state(download.state),
                download.bytes_downloaded,
                total,
                message
            );
        }
    }

    if !status.issues.is_empty() {
        println!("Issues:");
        for issue in &status.issues {
            println!(
                "  - [{}] {}: {}",
                describe_issue_severity(issue.severity),
                issue.code,
                issue.message
            );
            if let Some(detail) = &issue.detail {
                println!("    {detail}");
            }
        }
    }

    println!("Log: {}", status.log_path.display());
    println!();
}

pub(super) fn describe_activity(state: ActivityState) -> &'static str {
    match state {
        ActivityState::Starting => "starting",
        ActivityState::Idle => "idle",
        ActivityState::Indexing => "indexing",
        ActivityState::Removing => "removing stale notes",
        ActivityState::Downloading => "downloading assets",
        ActivityState::Faulted => "faulted",
    }
}

pub(super) fn describe_download_state(state: DownloadState) -> &'static str {
    match state {
        DownloadState::Pending => "pending",
        DownloadState::InProgress => "in-progress",
        DownloadState::Completed => "completed",
        DownloadState::Failed => "failed",
    }
}

pub(super) fn describe_issue_severity(severity: IssueSeverity) -> &'static str {
    match severity {
        IssueSeverity::Info => "info",
        IssueSeverity::Warning => "warning",
        IssueSeverity::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, DaemonConfig as CliDaemonConfig};
    use arrowhead_core::status::ActivityStatus;
    use chrono::Utc;
    use tempfile::TempDir;

    #[tokio::test]
    async fn fetch_status_falls_back_to_file() {
        let dir = TempDir::new().expect("temp vault");
        let vault_path = dir.path().join("vault");
        fs::create_dir_all(&vault_path).expect("create vault");

        let (vault, paths) = load_vault_environment(&vault_path).expect("env");
        vault.ensure_arrowhead_dirs().expect("dirs");
        fs::create_dir_all(&paths.daemon_dir).expect("daemon dir");

        let mut status = DaemonStatus::new(paths.log_path.clone());
        status.updated_at = Utc::now();
        status.indexed_notes = 5;
        status.activity = ActivityStatus::idle();
        status
            .save_to_path(&paths.status_path)
            .expect("save status");

        let fetched = fetch_status(&paths)
            .await
            .expect("fetch status")
            .expect("status present");
        assert_eq!(fetched.indexed_notes, 5);
    }

    #[tokio::test]
    async fn wait_for_startup_reports_pending_when_fresh_status_exists() {
        let dir = TempDir::new().expect("temp vault");
        let vault_path = dir.path().join("vault");
        fs::create_dir_all(&vault_path).expect("create vault");

        let (_vault, paths) = load_vault_environment(&vault_path).expect("env");
        fs::create_dir_all(&paths.daemon_dir).expect("daemon dir");

        let launched_at = Utc::now();
        let mut status = DaemonStatus::new(paths.log_path.clone());
        status.updated_at = Utc::now();
        status.activity = ActivityStatus::running(ActivityState::Starting, None, 0);
        status.activity.description = Some("starting arrowhead daemon".to_string());
        status
            .save_to_path(&paths.status_path)
            .expect("save status");

        let outcome = wait_for_startup(&paths, launched_at, Duration::from_millis(1))
            .await
            .expect("startup wait succeeds");
        assert!(
            matches!(outcome, StartupWaitOutcome::Pending(Some(_))),
            "expected pending startup outcome, got {outcome:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_removes_stale_socket_metadata() {
        let dir = TempDir::new().expect("temp dir");
        let vault_path = dir.path().join("vault");
        let arrowhead_dir = vault_path.join(".arrowhead");
        let config_path = dir.path().join("config.toml");

        fs::create_dir_all(arrowhead_dir.join("daemon")).expect("create daemon dir");
        fs::create_dir_all(arrowhead_dir.join("logs")).expect("create logs dir");
        fs::create_dir_all(&vault_path).expect("create vault");

        let socket_path = arrowhead_dir.join("daemon/control.sock");
        fs::write(&socket_path, b"stale").expect("write stale socket placeholder");
        let pid_path = arrowhead_dir.join("daemon/daemon.pid");
        fs::write(&pid_path, b"12345").expect("write pid file");

        let app_config = AppConfig {
            vault: Some(vault_path.clone()),
            daemon: CliDaemonConfig {
                socket_path: Some(socket_path.clone()),
                status_path: Some(arrowhead_dir.join("daemon/status.json")),
                auto_start_enabled: Some(false),
                last_status: None,
            },
            ..AppConfig::default()
        };

        let mut ctx = CommandContext::new(app_config, Some(config_path), 0);
        handle_stop(&mut ctx).await.expect("stop succeeds");

        assert!(!socket_path.exists(), "stale socket should be removed");
        assert!(!pid_path.exists(), "stale pid file should be removed");
    }

    #[tokio::test]
    async fn reset_removes_arrowhead_directory_and_resets_config() {
        let dir = TempDir::new().expect("temp dir");
        let vault_path = dir.path().join("vault");
        let arrowhead_dir = vault_path.join(".arrowhead");
        let config_path = dir.path().join("config.toml");

        fs::create_dir_all(arrowhead_dir.join("daemon")).expect("create daemon dir");
        fs::create_dir_all(arrowhead_dir.join("logs")).expect("create logs dir");
        fs::write(arrowhead_dir.join("index.db"), b"test").expect("write index");

        let app_config = AppConfig {
            vault: Some(vault_path.clone()),
            daemon: CliDaemonConfig {
                socket_path: Some(arrowhead_dir.join("daemon/control.sock")),
                status_path: Some(arrowhead_dir.join("daemon/status.json")),
                auto_start_enabled: Some(true),
                last_status: None,
            },
            ..AppConfig::default()
        };

        let mut ctx = CommandContext::new(app_config, Some(config_path), 0);

        fs::create_dir_all(&vault_path).expect("create vault");

        handle_reset(&mut ctx).await.expect("reset succeeds");

        assert!(
            !arrowhead_dir.exists(),
            "Arrowhead directory should be removed during reset"
        );
        assert!(ctx.config.daemon.is_empty());
    }

    #[tokio::test]
    async fn init_without_start_prepares_directories() {
        let dir = TempDir::new().expect("temp dir");
        let vault_path = dir.path().join("vault");
        fs::create_dir_all(&vault_path).expect("create vault");

        let config_path = dir.path().join("config.toml");
        let mut ctx = CommandContext::new(
            AppConfig {
                vault: Some(vault_path.clone()),
                ..AppConfig::default()
            },
            Some(config_path.clone()),
            0,
        );

        initialise_vault(
            &mut ctx,
            InitOptions {
                force: false,
                no_start: true,
                fts_only: false,
            },
        )
        .await
        .expect("init succeeds");

        let arrowhead_dir = vault_path.join(".arrowhead");
        assert!(
            arrowhead_dir.exists(),
            "Arrowhead directory should exist after init"
        );
        assert!(ctx.config.daemon.socket_path.is_some());
        assert!(ctx.config.daemon.status_path.is_some());
        assert_eq!(ctx.config.daemon.auto_start_enabled, Some(false));
    }

    #[test]
    fn configure_daemon_command_sets_args_and_env_for_compatibility() {
        let mut command = Command::new("arrowheadd");
        configure_daemon_command(&mut command, Path::new("/tmp/vault"), None);

        let args = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            vec!["--vault", "/tmp/vault", "--embedding-model", "none"]
        );

        let envs = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|entry| entry.to_string_lossy().into_owned()),
                )
            })
            .collect::<Vec<_>>();
        assert!(
            envs.contains(&(
                "ARROWHEAD_VAULT".to_string(),
                Some("/tmp/vault".to_string())
            )),
            "vault env should be configured"
        );
        assert!(
            envs.contains(&(
                "ARROWHEAD_EMBEDDING_MODEL".to_string(),
                Some("none".to_string())
            )),
            "embedding env should be configured"
        );
    }
}
