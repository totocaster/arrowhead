//! `arrowhead vault` subcommands.

use std::{
    env, fs, mem,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use arrowhead_core::{DeamonStatus, Vault, VaultConfig};
use arrowhead_deamon::{ControlRequest, ControlResponse, send_control_request};
use clap::{Args, Subcommand};
use tokio::time::{Instant, sleep};
use tracing::info;

use super::CommandContext;
use crate::autostart::{
    AUTOSTART_DIR, AutoStartManager, AutoStartProvider, AutoStartStatus, MANIFEST_FILE,
    prompt_yes_no,
};
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
    /// Launch the background deamon, spawning it if necessary.
    Start(VaultStartArgs),
    /// Send a shutdown signal to the deamon and clean up metadata.
    Stop,
    /// Stop the deamon (if running) and remove Arrowhead caches from the vault.
    Cleanup,
    /// Manage auto-start integration for the vault.
    Autostart(VaultAutostartCommand),
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
    /// Disable semantic indexing when initialising the vault.
    #[arg(long)]
    pub fts_only: bool,
}

/// Options for `vault start`.
#[derive(Debug, Args, Clone, PartialEq, Eq)]
pub struct VaultStartArgs {
    /// Do not wait for the control socket to appear after spawning.
    #[arg(long)]
    pub no_wait: bool,
}

/// Options for `vault autostart`.
#[derive(Debug, Args, Clone, PartialEq, Eq)]
pub struct VaultAutostartCommand {
    /// Auto-start operation to perform.
    #[command(subcommand)]
    pub action: VaultAutostartAction,
}

/// Supported auto-start operations.
#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
pub enum VaultAutostartAction {
    /// Install and enable auto-start for this vault.
    Enable,
    /// Disable auto-start and remove installed units.
    Disable,
    /// Display the current auto-start status.
    Status,
}

/// Execute the vault command.
pub async fn run(ctx: &mut CommandContext, command: &VaultCommand) -> Result<()> {
    match &command.action {
        VaultAction::Init(args) => handle_init(ctx, args).await?,
        VaultAction::Start(args) => handle_start(ctx, args).await?,
        VaultAction::Stop => handle_stop(ctx).await?,
        VaultAction::Cleanup => handle_cleanup(ctx).await?,
        VaultAction::Autostart(command) => handle_autostart(ctx, command).await?,
    }

    Ok(())
}

async fn handle_start(ctx: &mut CommandContext, args: &VaultStartArgs) -> Result<()> {
    let vault_path = resolve_vault_path(ctx)?;
    let (vault, paths) = load_vault_environment(&vault_path)?;

    if is_socket_alive(&paths).await? {
        bail!("arrowhead deamon already running for this vault");
    }

    ensure_runtime_dirs(&vault, &paths)?;
    println!("initialising arrowhead daemon…");

    let manager = AutoStartManager::detect(paths.autostart_manifest_path.clone());
    let mut pid: Option<u32> = None;

    if let Some(manager) = &manager {
        if let Some(manifest) = manager.load_manifest()? {
            match manager.start_unit(&manifest) {
                Ok(reported_pid) => {
                    pid = reported_pid;
                    ctx.config.deamon.auto_start_enabled = Some(true);
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
            launch_deamon_process(&vault_path, ctx.config.embedding_model.as_deref())?;
        pid = Some(spawned_pid);
        if ctx.config.deamon.auto_start_enabled.is_none() {
            ctx.config.deamon.auto_start_enabled = Some(false);
        }
    }

    if let Some(actual_pid) = pid {
        write_pid_file(&paths.pid_path, actual_pid)?;
    } else {
        remove_pid_file(&paths.pid_path)?;
    }

    if !args.no_wait {
        wait_for_socket(&paths.socket_path, STARTUP_TIMEOUT)
            .await
            .with_context(|| "deamon failed to expose control socket in time")?;
    }

    update_config_with_status(ctx, &paths, None)?;

    if let Some(actual_pid) = pid {
        println!(
            "arrowhead deamon started (pid {actual_pid}). Control socket: {}",
            paths.socket_path.display()
        );
    } else {
        println!(
            "arrowhead deamon started. Control socket: {}",
            paths.socket_path.display()
        );
    }

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

    if already_initialised && !args.force {
        bail!(
            "Arrowhead is already initialised for this vault. Re-run with --force to reinitialise."
        );
    }

    if args.force {
        cleanup_auto_start(&paths)?;
        cleanup_arrowhead_dirs(&paths)?;
    }

    ensure_runtime_dirs(&vault, &paths)?;
    println!("initialising arrowhead daemon…");

    ctx.config.vault = Some(vault.paths().root.clone());
    if args.fts_only {
        ctx.config.embedding_model = None;
        info!("configured vault for full-text search only");
    }

    let manager = AutoStartManager::detect(paths.autostart_manifest_path.clone());
    let mut manifest = match manager.as_ref() {
        Some(manager) => manager.load_manifest()?,
        None => None,
    };

    let mut auto_start_preference = ctx.config.deamon.auto_start_enabled;

    if args.no_start {
        if manifest.is_some() {
            auto_start_preference = Some(true);
        } else if auto_start_preference.is_none() {
            auto_start_preference = Some(false);
        }

        ctx.config.deamon = DeamonConfig {
            socket_path: Some(paths.socket_path.clone()),
            status_path: Some(paths.status_path.clone()),
            auto_start_enabled: auto_start_preference,
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
        if manifest.is_some() {
            auto_start_preference = Some(true);
        } else if auto_start_preference.is_none() {
            auto_start_preference = Some(false);
        }
        ctx.config.deamon.auto_start_enabled = auto_start_preference;
        update_config_with_status(ctx, &paths, None)?;
        return Ok(());
    }

    if manifest.is_none() {
        if let Some(manager) = &manager {
            let enable = if let Some(preference) = auto_start_preference {
                preference
            } else {
                prompt_yes_no(
                    "Enable Arrowhead auto-start so the deamon launches automatically on login?",
                )?
                .unwrap_or_default()
            };

            if enable {
                let binary = find_deamon_binary()?;
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
                "Auto-start is not supported on this platform; the deamon must be started manually."
            );
            auto_start_preference = Some(false);
        }
    } else {
        auto_start_preference = Some(true);
    }

    ctx.config.deamon.auto_start_enabled = auto_start_preference;

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
            launch_deamon_process(&vault_path, ctx.config.embedding_model.as_deref())?;
        pid = Some(spawned_pid);
    }

    if let Some(actual_pid) = pid {
        write_pid_file(&paths.pid_path, actual_pid)?;
    } else {
        remove_pid_file(&paths.pid_path)?;
    }

    wait_for_socket(&paths.socket_path, STARTUP_TIMEOUT)
        .await
        .with_context(|| "deamon failed to expose control socket in time")?;

    update_config_with_status(ctx, &paths, None)?;

    if let Some(actual_pid) = pid {
        println!(
            "Arrowhead initialised and deamon started (pid {actual_pid}). Monitor progress with `arrowhead vault status`."
        );
    } else {
        println!(
            "Arrowhead initialised and deamon started. Monitor progress with `arrowhead vault status`."
        );
    }

    println!(
        "arrowheadd is performing the initial indexing pass in the background. Check `arrowhead vault status` for progress."
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

    cleanup_auto_start(&paths)?;
    cleanup_arrowhead_dirs(&paths)?;

    ctx.config.deamon = DeamonConfig::default();
    ctx.persist()?;

    println!(
        "arrowhead caches removed from {}",
        paths.arrowhead_dir.display()
    );

    Ok(())
}

async fn handle_autostart(ctx: &mut CommandContext, command: &VaultAutostartCommand) -> Result<()> {
    let vault_path = resolve_vault_path(ctx)?;
    let (vault, paths) = load_vault_environment(&vault_path)?;
    ctx.config.vault = Some(vault.paths().root.clone());

    match command.action {
        VaultAutostartAction::Enable => {
            ensure_runtime_dirs(&vault, &paths)?;

            let manager = match AutoStartManager::detect(paths.autostart_manifest_path.clone()) {
                Some(manager) => manager,
                None => {
                    println!("Auto-start is not supported on this platform.");
                    ctx.config.deamon.auto_start_enabled = Some(false);
                    ctx.persist()?;
                    return Ok(());
                }
            };

            let binary = find_deamon_binary()?;
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

            ctx.config.deamon.auto_start_enabled = Some(true);
            update_config_with_status(ctx, &paths, None)?;
            ctx.persist()?;
        }
        VaultAutostartAction::Disable => {
            let manager = match AutoStartManager::detect(paths.autostart_manifest_path.clone()) {
                Some(manager) => manager,
                None => {
                    println!("Auto-start is not supported on this platform.");
                    ctx.config.deamon.auto_start_enabled = Some(false);
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

            ctx.config.deamon.auto_start_enabled = Some(false);
            update_config_with_status(ctx, &paths, None)?;
            ctx.persist()?;
        }
        VaultAutostartAction::Status => {
            let status = auto_start_status(&paths)?;
            print_autostart_status(&status);
        }
    }

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
    let autostart_dir = deamon_dir.join(AUTOSTART_DIR);
    let paths = DeamonPaths {
        arrowhead_dir: vault_paths.arrowhead_dir.clone(),
        deamon_dir,
        autostart_dir: autostart_dir.clone(),
        autostart_manifest_path: autostart_dir.join(MANIFEST_FILE),
        status_path: vault_paths.arrowhead_dir.join("deamon/status.json"),
        socket_path: vault_paths.arrowhead_dir.join("deamon/control.sock"),
        pid_path: vault_paths.arrowhead_dir.join("deamon/deamon.pid"),
        log_path: vault_paths.logs_dir().join("daemon.log"),
    };

    Ok((vault, paths))
}

#[cfg(test)]
async fn fetch_status(paths: &DeamonPaths) -> Result<Option<DeamonStatus>> {
    match send_control_request(&paths.socket_path, ControlRequest::StatusSnapshot).await {
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
    match send_control_request(&paths.socket_path, ControlRequest::StatusSnapshot).await {
        Ok(ControlResponse::Status { .. }) => Ok(true),
        Ok(ControlResponse::Error { .. }) => Ok(true),
        Ok(ControlResponse::ShutdownAck) => Ok(false),
        Err(_) => Ok(false),
    }
}

fn auto_start_status(paths: &DeamonPaths) -> Result<AutoStartStatus> {
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
                "Auto-start enabled via {} ({state}).",
                provider_label(*provider)
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

fn ensure_runtime_dirs(vault: &Vault, paths: &DeamonPaths) -> Result<()> {
    vault.ensure_arrowhead_dirs()?;
    fs::create_dir_all(&paths.deamon_dir).with_context(|| {
        format!(
            "failed to create deamon directory {}",
            paths.deamon_dir.display()
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

fn find_deamon_binary() -> Result<PathBuf> {
    if let Some(path) =
        env::var_os("ARROWHEAD_DEAMON_PATH").or_else(|| env::var_os("ARROWHEADD_PATH"))
    {
        return Ok(PathBuf::from(path));
    }

    let candidate_names = [
        format!("arrowheadd{}", std::env::consts::EXE_SUFFIX),
        format!("arrowhead-deamon{}", std::env::consts::EXE_SUFFIX),
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

    Ok(PathBuf::from(&candidate_names[0]))
}

fn launch_deamon_process(vault_path: &Path, embedding_model: Option<&str>) -> Result<u32> {
    let binary = find_deamon_binary()?;
    let mut command = Command::new(&binary);
    command
        .env("ARROWHEAD_VAULT", vault_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match embedding_model {
        Some(model) => {
            command.env("ARROWHEAD_EMBEDDING_MODEL", model);
        }
        None => {
            command.env("ARROWHEAD_EMBEDDING_MODEL", "none");
        }
    }

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
    autostart_dir: PathBuf,
    autostart_manifest_path: PathBuf,
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

fn cleanup_auto_start(paths: &DeamonPaths) -> Result<()> {
    if let Some(manager) = AutoStartManager::detect(paths.autostart_manifest_path.clone()) {
        if let Some(manifest) = manager.load_manifest()? {
            manager.uninstall(&manifest)?;
        } else {
            manager.remove_manifest()?;
        }
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

        let app_config = AppConfig {
            vault: Some(vault_path.clone()),
            deamon: CliDeamonConfig {
                socket_path: Some(arrowhead_dir.join("deamon/control.sock")),
                status_path: Some(arrowhead_dir.join("deamon/status.json")),
                auto_start_enabled: Some(true),
                last_status: None,
            },
            ..AppConfig::default()
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
            fts_only: false,
        };

        handle_init(&mut ctx, &args).await.expect("init succeeds");

        let arrowhead_dir = vault_path.join(".arrowhead");
        assert!(
            arrowhead_dir.exists(),
            "Arrowhead directory should exist after init"
        );
        assert!(ctx.config.deamon.socket_path.is_some());
        assert!(ctx.config.deamon.status_path.is_some());
        assert_eq!(ctx.config.deamon.auto_start_enabled, Some(false));
    }
}
