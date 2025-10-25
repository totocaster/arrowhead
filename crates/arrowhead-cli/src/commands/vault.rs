//! `arrowhead vault` subcommands.

use std::{
    env, fs, mem,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use arrowhead_core::{ActivityState, DeamonStatus, IssueSeverity, StatusIssue, Vault, VaultConfig};
use arrowhead_deamon::{ControlRequest, ControlResponse, send_control_request};
use clap::{Args, Subcommand};
use tokio::time::{Instant, sleep};

use super::CommandContext;
use crate::config::DeamonConfig;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Vault-related utilities.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct VaultCommand {
    /// Specific vault action to run.
    #[command(subcommand)]
    pub action: VaultAction,
}

/// Supported vault subcommands.
#[derive(Debug, Subcommand, Clone, PartialEq)]
pub enum VaultAction {
    /// Prepare a vault for the background deamon and optionally launch it.
    Init(VaultInitArgs),
    /// Query the active deamon and render its status.
    Status(VaultStatusArgs),
    /// Launch the background deamon, spawning it if necessary.
    Start(VaultStartArgs),
    /// Send a shutdown signal to the deamon and clean up metadata.
    Stop,
    /// Stop the deamon (if running) and remove Arrowhead caches from the vault.
    Cleanup,
}

/// Options for `vault init`.
#[derive(Debug, Args, Clone, PartialEq, Eq)]
pub struct VaultInitArgs {
    /// Recreate Arrowhead directories even if they already exist.
    #[arg(long)]
    pub force: bool,
    /// Prepare the vault without launching the deamon.
    #[arg(long)]
    pub no_start: bool,
}

/// Options for `vault status`.
#[derive(Debug, Args, Clone, PartialEq, Eq)]
pub struct VaultStatusArgs {
    /// Emit status as JSON instead of human-readable output.
    #[arg(long)]
    pub json: bool,
}

/// Options for `vault start`.
#[derive(Debug, Args, Clone, PartialEq, Eq)]
pub struct VaultStartArgs {
    /// Do not wait for the control socket to appear after spawning.
    #[arg(long)]
    pub no_wait: bool,
}

/// Execute the vault command.
pub async fn run(ctx: &mut CommandContext, command: &VaultCommand) -> Result<()> {
    match &command.action {
        VaultAction::Init(args) => handle_init(ctx, args).await?,
        VaultAction::Status(args) => handle_status(ctx, args).await?,
        VaultAction::Start(args) => handle_start(ctx, args).await?,
        VaultAction::Stop => handle_stop(ctx).await?,
        VaultAction::Cleanup => handle_cleanup(ctx).await?,
    }

    Ok(())
}

async fn handle_status(ctx: &mut CommandContext, args: &VaultStatusArgs) -> Result<()> {
    let vault_path = resolve_vault_path(ctx)?;
    let (_vault, paths) = load_vault_environment(&vault_path)?;

    let status = fetch_status(&paths).await?.ok_or_else(|| {
        anyhow!("arrowhead deamon is not running. Start it with `arrowhead vault start`")
    })?;

    if args.json {
        let payload = serde_json::to_string_pretty(&status).context("failed to render JSON")?;
        println!("{}", payload);
    } else {
        render_status(&status);
    }

    update_config_with_status(ctx, &paths, Some(&status))?;

    Ok(())
}

async fn handle_start(ctx: &mut CommandContext, args: &VaultStartArgs) -> Result<()> {
    let vault_path = resolve_vault_path(ctx)?;
    let (vault, paths) = load_vault_environment(&vault_path)?;

    if is_socket_alive(&paths).await? {
        bail!("arrowhead deamon already running for this vault");
    }

    ensure_runtime_dirs(&vault, &paths)?;

    let pid = launch_deamon_process(&vault_path)?;
    write_pid_file(&paths.pid_path, pid)?;

    if !args.no_wait {
        wait_for_socket(&paths.socket_path, STARTUP_TIMEOUT)
            .await
            .with_context(|| "deamon failed to expose control socket in time")?;
    }

    update_config_with_status(ctx, &paths, None)?;

    println!(
        "arrowhead deamon started (pid {pid}). Control socket: {}",
        paths.socket_path.display()
    );

    Ok(())
}

async fn handle_stop(ctx: &mut CommandContext) -> Result<()> {
    let vault_path = resolve_vault_path(ctx)?;
    let (_vault, paths) = load_vault_environment(&vault_path)?;

    match send_control_request(&paths.socket_path, ControlRequest::Shutdown).await {
        Ok(ControlResponse::ShutdownAck) => {
            wait_for_socket_removal(&paths.socket_path, SHUTDOWN_TIMEOUT).await?;
            println!("shutdown signal sent to arrowhead deamon");
        }
        Ok(other) => {
            bail!("unexpected response from deamon: {:?}", other);
        }
        Err(err) => {
            if paths.socket_path.exists() {
                return Err(err.context("failed to contact arrowhead deamon"));
            }
            println!(
                "no active deamon detected; cleaning up stale metadata ({})",
                err
            );
        }
    }

    if let Some(pid) = read_pid_file(&paths.pid_path)? {
        println!("removed PID file for process {pid}");
    }
    remove_pid_file(&paths.pid_path)?;

    update_config_with_status(ctx, &paths, None)?;

    Ok(())
}

async fn handle_init(ctx: &mut CommandContext, args: &VaultInitArgs) -> Result<()> {
    let vault_path = resolve_vault_path(ctx)?;
    if !vault_path.exists() {
        bail!(
            "vault directory {} does not exist (use `arrowhead init --force` to create it)",
            vault_path.display()
        );
    }

    let (vault, paths) = load_vault_environment(&vault_path)?;

    if paths.arrowhead_dir.exists() && !args.force {
        bail!(
            "Arrowhead is already initialised for this vault. Re-run with --force to reinitialise."
        );
    }

    if args.force {
        cleanup_arrowhead_dirs(&paths)?;
    }

    ensure_runtime_dirs(&vault, &paths)?;

    ctx.config.vault = Some(vault.paths().root.clone());

    if args.no_start {
        ctx.config.deamon = DeamonConfig {
            socket_path: Some(paths.socket_path.clone()),
            status_path: Some(paths.status_path.clone()),
            auto_start_enabled: ctx.config.deamon.auto_start_enabled,
            last_status: None,
        };
        ctx.persist()?;
        println!(
            "Arrowhead directories initialised at {}. Start the deamon with `arrowhead vault start` when ready.",
            paths.arrowhead_dir.display()
        );
        return Ok(());
    }

    if is_socket_alive(&paths).await? {
        println!("arrowhead deamon already running; skipping launch");
        update_config_with_status(ctx, &paths, None)?;
        return Ok(());
    }

    let pid = launch_deamon_process(&vault_path)?;
    write_pid_file(&paths.pid_path, pid)?;

    wait_for_socket(&paths.socket_path, STARTUP_TIMEOUT)
        .await
        .with_context(|| "deamon failed to expose control socket in time")?;

    update_config_with_status(ctx, &paths, None)?;

    println!(
        "Arrowhead initialised and deamon started (pid {pid}). Monitor progress with `arrowhead vault status`."
    );

    Ok(())
}

async fn handle_cleanup(ctx: &mut CommandContext) -> Result<()> {
    let vault_path = resolve_vault_path(ctx)?;
    let (_vault, paths) = load_vault_environment(&vault_path)?;

    if paths.socket_path.exists() {
        match send_control_request(&paths.socket_path, ControlRequest::Shutdown).await {
            Ok(ControlResponse::ShutdownAck) => {
                wait_for_socket_removal(&paths.socket_path, SHUTDOWN_TIMEOUT).await?;
                println!("shutdown signal sent to arrowhead deamon");
            }
            Ok(ControlResponse::Error { message }) => {
                bail!("deamon reported an error during shutdown: {message}");
            }
            Ok(other) => {
                bail!("unexpected response from deamon: {:?}", other);
            }
            Err(err) => {
                if paths.socket_path.exists() {
                    println!(
                        "failed to contact arrowhead deamon ({}); continuing with cleanup",
                        err
                    );
                }
            }
        }
    }

    if let Some(pid) = read_pid_file(&paths.pid_path)? {
        println!("removed PID file for process {pid}");
    }
    remove_pid_file(&paths.pid_path)?;

    cleanup_arrowhead_dirs(&paths)?;

    ctx.config.deamon = DeamonConfig::default();
    ctx.persist()?;

    println!(
        "arrowhead caches removed from {}",
        paths.arrowhead_dir.display()
    );

    Ok(())
}

fn resolve_vault_path(ctx: &CommandContext) -> Result<PathBuf> {
    ctx.config
        .vault
        .clone()
        .context("no vault configured. Provide --vault or run `arrowhead init` first")
}

fn load_vault_environment(vault_path: &Path) -> Result<(Vault, DeamonPaths)> {
    let vault = Vault::new(VaultConfig::new(vault_path.to_path_buf()))?;
    let vault_paths = vault.paths().clone();
    let deamon_dir = vault_paths.arrowhead_dir.join("deamon");
    let paths = DeamonPaths {
        arrowhead_dir: vault_paths.arrowhead_dir.clone(),
        deamon_dir,
        status_path: vault_paths.arrowhead_dir.join("deamon/status.json"),
        socket_path: vault_paths.arrowhead_dir.join("deamon/control.sock"),
        pid_path: vault_paths.arrowhead_dir.join("deamon/deamon.pid"),
        log_path: vault_paths.logs_dir().join("arrowhead-deamon.log"),
    };

    Ok((vault, paths))
}

async fn fetch_status(paths: &DeamonPaths) -> Result<Option<DeamonStatus>> {
    match send_control_request(&paths.socket_path, ControlRequest::Status).await {
        Ok(ControlResponse::Status { status }) => Ok(Some(status)),
        Ok(ControlResponse::Error { message }) => bail!(message),
        Ok(ControlResponse::ShutdownAck) => Ok(None),
        Err(err) => {
            if let Some(status) = DeamonStatus::load_from_path(&paths.status_path)? {
                return Ok(Some(status));
            }
            if paths.socket_path.exists() {
                return Err(err.context("failed to query arrowhead deamon status"));
            }
            Ok(None)
        }
    }
}

async fn is_socket_alive(paths: &DeamonPaths) -> Result<bool> {
    match send_control_request(&paths.socket_path, ControlRequest::Status).await {
        Ok(ControlResponse::Status { .. }) => Ok(true),
        Ok(ControlResponse::Error { .. }) => Ok(true),
        Ok(ControlResponse::ShutdownAck) => Ok(false),
        Err(_) => Ok(false),
    }
}

fn render_status(status: &DeamonStatus) {
    println!(
        "arrowhead deamon status (updated {})",
        status.updated_at.to_rfc3339()
    );
    let activity_label = status
        .activity
        .description
        .as_deref()
        .unwrap_or_else(|| describe_activity(status.activity.state));
    println!("  Activity: {}", activity_label);
    if let Some(note) = &status.activity.note_id {
        println!("  Current note: {}", note);
    }
    if status.activity.queued_jobs > 0 {
        println!("  Queued jobs: {}", status.activity.queued_jobs);
    }
    println!("  Indexed notes: {}", status.indexed_notes);
    println!("  Error notes: {}", status.error_notes);
    println!("  Log file: {}", status.log_path.display());

    if status.downloads.is_empty() {
        println!("  Downloads: none");
    } else {
        println!("  Downloads:");
        for download in &status.downloads {
            println!(
                "    - {} ({:?}) {}/{}",
                download.item,
                download.state,
                download.bytes_downloaded,
                download
                    .bytes_total
                    .map(|total| total.to_string())
                    .unwrap_or_else(|| "?".to_string())
            );
            if let Some(message) = &download.message {
                println!("      {}", message);
            }
        }
    }

    if status.issues.is_empty() {
        println!("  Issues: none");
    } else {
        println!("  Issues:");
        for issue in &status.issues {
            print_issue(issue);
        }
    }
}

fn print_issue(issue: &StatusIssue) {
    println!(
        "    - [{}] {}: {} (at {})",
        severity_label(issue.severity),
        issue.code,
        issue.message,
        issue.occurred_at.to_rfc3339()
    );
    if let Some(detail) = &issue.detail {
        println!("      {}", detail);
    }
}

fn describe_activity(state: ActivityState) -> &'static str {
    match state {
        ActivityState::Idle => "idle",
        ActivityState::Indexing => "indexing",
        ActivityState::Removing => "removing stale notes",
        ActivityState::Downloading => "downloading assets",
        ActivityState::Faulted => "faulted",
    }
}

fn severity_label(severity: IssueSeverity) -> &'static str {
    match severity {
        IssueSeverity::Info => "info",
        IssueSeverity::Warning => "warning",
        IssueSeverity::Error => "error",
    }
}

fn ensure_runtime_dirs(vault: &Vault, paths: &DeamonPaths) -> Result<()> {
    vault.ensure_arrowhead_dirs()?;
    fs::create_dir_all(&paths.deamon_dir).with_context(|| {
        format!(
            "failed to create deamon directory {}",
            paths.deamon_dir.display()
        )
    })?;
    if let Some(parent) = paths.log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory {}", parent.display()))?;
    }
    Ok(())
}

fn find_deamon_binary() -> Result<PathBuf> {
    if let Some(path) = env::var_os("ARROWHEAD_DEAMON_PATH") {
        return Ok(PathBuf::from(path));
    }

    let binary_name = format!("arrowhead-deamon{}", std::env::consts::EXE_SUFFIX);

    if let Ok(current) = env::current_exe() {
        if let Some(dir) = current.parent() {
            let candidate = dir.join(&binary_name);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    Ok(PathBuf::from(binary_name))
}

fn launch_deamon_process(vault_path: &Path) -> Result<u32> {
    let binary = find_deamon_binary()?;
    let mut command = Command::new(&binary);
    command
        .env("ARROWHEAD_VAULT", vault_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child = command
        .spawn()
        .with_context(|| format!("failed to launch deamon binary {}", binary.display()))?;
    let pid = child.id();
    mem::forget(child);
    Ok(pid)
}

fn write_pid_file(path: &Path, pid: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create PID directory {}", parent.display()))?;
    }
    fs::write(path, pid.to_string())
        .with_context(|| format!("failed to write PID file {}", path.display()))
}

fn read_pid_file(path: &Path) -> Result<Option<u32>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read PID file {}", path.display()))?;
    let pid: u32 = content
        .trim()
        .parse()
        .with_context(|| format!("invalid PID file {}", path.display()))?;
    Ok(Some(pid))
}

fn remove_pid_file(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path)
            .with_context(|| format!("failed to remove PID file {}", path.display()))?;
    }
    Ok(())
}

async fn wait_for_socket(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    bail!("control socket {} did not appear", path.display());
}

async fn wait_for_socket_removal(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !path.exists() {
            return Ok(());
        }
        sleep(Duration::from_millis(100)).await;
    }
    bail!(
        "control socket {} still present after shutdown request",
        path.display()
    );
}

fn update_config_with_status(
    ctx: &mut CommandContext,
    paths: &DeamonPaths,
    status: Option<&DeamonStatus>,
) -> Result<()> {
    let config = &mut ctx.config.deamon;
    config.socket_path = Some(paths.socket_path.clone());
    config.status_path = Some(paths.status_path.clone());

    if let Some(status) = status {
        config.last_status = Some(crate::config::DeamonStatusSummary {
            updated_at: status.updated_at,
            state: status.activity.state,
            indexed_notes: status.indexed_notes,
            error_notes: status.error_notes,
        });
    } else {
        config.last_status = None;
    }

    ctx.persist()
}

#[derive(Debug, Clone)]
struct DeamonPaths {
    arrowhead_dir: PathBuf,
    deamon_dir: PathBuf,
    status_path: PathBuf,
    socket_path: PathBuf,
    pid_path: PathBuf,
    log_path: PathBuf,
}

fn cleanup_arrowhead_dirs(paths: &DeamonPaths) -> Result<()> {
    if paths.arrowhead_dir.exists() {
        fs::remove_dir_all(&paths.arrowhead_dir).with_context(|| {
            format!(
                "failed to remove Arrowhead directory {}",
                paths.arrowhead_dir.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, DeamonConfig as CliDeamonConfig};
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
        fs::create_dir_all(&paths.deamon_dir).expect("deamon dir");

        let mut status = DeamonStatus::new(paths.log_path.clone());
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
    async fn cleanup_removes_arrowhead_directory_and_resets_config() {
        let dir = TempDir::new().expect("temp dir");
        let vault_path = dir.path().join("vault");
        let arrowhead_dir = vault_path.join(".arrowhead");
        let config_path = dir.path().join("config.toml");

        fs::create_dir_all(arrowhead_dir.join("deamon")).expect("create deamon dir");
        fs::create_dir_all(arrowhead_dir.join("logs")).expect("create logs dir");
        fs::write(arrowhead_dir.join("index.db"), b"test").expect("write index");

        let mut app_config = AppConfig::default();
        app_config.vault = Some(vault_path.clone());
        app_config.deamon = CliDeamonConfig {
            socket_path: Some(arrowhead_dir.join("deamon/control.sock")),
            status_path: Some(arrowhead_dir.join("deamon/status.json")),
            auto_start_enabled: Some(true),
            last_status: None,
        };

        let mut ctx = CommandContext::new(app_config, Some(config_path), 0);

        fs::create_dir_all(&vault_path).expect("create vault");

        handle_cleanup(&mut ctx).await.expect("cleanup succeeds");

        assert!(
            !arrowhead_dir.exists(),
            "Arrowhead directory should be removed during cleanup"
        );
        assert!(ctx.config.deamon.is_empty());
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

        let args = VaultInitArgs {
            force: false,
            no_start: true,
        };

        handle_init(&mut ctx, &args).await.expect("init succeeds");

        let arrowhead_dir = vault_path.join(".arrowhead");
        assert!(
            arrowhead_dir.exists(),
            "Arrowhead directory should exist after init"
        );
        assert!(ctx.config.deamon.socket_path.is_some());
        assert!(ctx.config.deamon.status_path.is_some());
    }
}
