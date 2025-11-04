//! Auto-start integration for the Arrowhead daemon.
//!
//! This module hides the platform-specific details for registering Arrowhead
//! with user-level service managers (launchd on macOS, systemd --user on
//! Linux). It persists lightweight metadata under the vault so CLI commands
//! can surface auto-start status and perform cleanup.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write as _};
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use directories::UserDirs;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Relative directory under `.arrowhead/daemon/` where metadata is stored.
pub const AUTOSTART_DIR: &str = "autostart";
/// File name used for the auto-start manifest.
pub const MANIFEST_FILE: &str = "manifest.json";

/// High-level service manager that can supervise the Arrowhead daemon.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AutoStartProvider {
    /// macOS `launchd` user agents.
    Launchd,
    /// Linux `systemd --user` services.
    SystemdUser,
}

/// Persisted metadata about the installed auto-start unit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoStartManifest {
    /// Which platform integration is active.
    pub provider: AutoStartProvider,
    /// Service label (launchd) or unit name (systemd).
    pub unit_name: String,
    /// Path to the generated unit file.
    pub unit_path: PathBuf,
    /// When the manifest was written.
    pub installed_at: DateTime<Utc>,
}

impl AutoStartManifest {
    /// Persist the manifest to disk.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create auto-start metadata directory {}",
                    parent.display()
                )
            })?;
        }

        let payload =
            serde_json::to_vec_pretty(self).context("failed to serialise auto-start metadata")?;
        fs::write(path, payload).with_context(|| format!("failed to write {}", path.display()))
    }

    /// Load a manifest from disk if present.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }

        let contents =
            fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        let manifest = serde_json::from_slice(&contents)
            .with_context(|| format!("failed to parse auto-start metadata {}", path.display()))?;
        Ok(Some(manifest))
    }
}

/// Snapshot describing the current auto-start status surfaced to users.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoStartStatus {
    /// Auto-start is installed; the unit is enabled and optionally active.
    Enabled {
        provider: AutoStartProvider,
        active: bool,
    },
    /// Auto-start metadata exists but appears disabled.
    Disabled { provider: AutoStartProvider },
    /// Auto-start is not available or not configured for this platform.
    Unsupported,
}

/// Wrapper around platform-specific installation routines.
#[derive(Debug)]
pub struct AutoStartManager {
    provider: AutoStartProvider,
    manifest_path: PathBuf,
}

impl AutoStartManager {
    /// Construct a manager for the supplied vault paths, returning `None` when
    /// auto-start is not supported by this platform.
    pub fn detect(manifest_path: PathBuf) -> Option<Self> {
        #[cfg(target_os = "macos")]
        {
            Some(Self {
                provider: AutoStartProvider::Launchd,
                manifest_path,
            })
        }

        #[cfg(target_os = "linux")]
        {
            Some(Self {
                provider: AutoStartProvider::SystemdUser,
                manifest_path,
            })
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = manifest_path;
            None
        }
    }

    /// Expose the underlying provider for status reporting.
    pub fn provider(&self) -> AutoStartProvider {
        self.provider
    }

    /// Load the current manifest if present.
    pub fn load_manifest(&self) -> Result<Option<AutoStartManifest>> {
        AutoStartManifest::load(&self.manifest_path)
    }

    /// Remove a manifest file from disk.
    pub fn remove_manifest(&self) -> Result<()> {
        if self.manifest_path.exists() {
            fs::remove_file(&self.manifest_path)
                .with_context(|| format!("failed to remove {}", self.manifest_path.display()))?;
        }
        Ok(())
    }

    /// Install auto-start support for the given vault and binary, returning the
    /// manifest describing the installed unit.
    pub fn install(
        &self,
        vault_path: &Path,
        daemon_binary: &Path,
        embedding_model: Option<&str>,
    ) -> Result<AutoStartManifest> {
        match self.provider {
            AutoStartProvider::Launchd => {
                self.install_launchd(vault_path, daemon_binary, embedding_model)
            }
            AutoStartProvider::SystemdUser => {
                self.install_systemd(vault_path, daemon_binary, embedding_model)
            }
        }
    }

    /// Ensure the installed unit is disabled and remove it from the system.
    pub fn uninstall(&self, manifest: &AutoStartManifest) -> Result<()> {
        match manifest.provider {
            AutoStartProvider::Launchd => self.uninstall_launchd(manifest),
            AutoStartProvider::SystemdUser => self.uninstall_systemd(manifest),
        }
    }

    /// Request that the service manager start the unit, returning the observed
    /// PID when obtainable.
    pub fn start_unit(&self, manifest: &AutoStartManifest) -> Result<Option<u32>> {
        match manifest.provider {
            AutoStartProvider::Launchd => self.start_launchd(manifest),
            AutoStartProvider::SystemdUser => self.start_systemd(manifest),
        }
    }

    /// Query whether the installed unit is enabled/active.
    pub fn query_status(&self, manifest: &AutoStartManifest) -> Result<AutoStartStatus> {
        match manifest.provider {
            AutoStartProvider::Launchd => self.query_launchd_status(manifest),
            AutoStartProvider::SystemdUser => self.query_systemd_status(manifest),
        }
    }

    fn install_launchd(
        &self,
        vault_path: &Path,
        daemon_binary: &Path,
        embedding_model: Option<&str>,
    ) -> Result<AutoStartManifest> {
        let home = UserDirs::new()
            .map(|dirs| dirs.home_dir().to_path_buf())
            .context("failed to determine user home directory")?;
        let agents_dir = home.join("Library/LaunchAgents");
        fs::create_dir_all(&agents_dir).with_context(|| {
            format!(
                "failed to create launch agents directory {}",
                agents_dir.display()
            )
        })?;

        let label = format!("com.arrowhead.daemon.{}", vault_slug(vault_path)?);
        let plist_path = agents_dir.join(format!("{label}.plist"));

        let plist = render_launchd_plist(&label, daemon_binary, vault_path, embedding_model);
        fs::write(&plist_path, plist)
            .with_context(|| format!("failed to write launchd plist {}", plist_path.display()))?;

        // Register the agent for this session (bootstrap) so it becomes active
        // immediately. Fall back to legacy `load` if bootstrap fails.
        #[cfg(target_os = "macos")]
        {
            let scope = format!("gui/{}", current_uid()?);
            let status = Command::new("launchctl")
                .arg("bootstrap")
                .arg(&scope)
                .arg(&plist_path)
                .status();
            match status {
                Ok(exit) if exit.success() => {}
                _ => {
                    let load_status = Command::new("launchctl")
                        .arg("load")
                        .arg(&plist_path)
                        .status()
                        .context("failed to invoke launchctl load")?;
                    if !load_status.success() {
                        bail!(
                            "launchctl failed to register agent {} (exit code {:?})",
                            plist_path.display(),
                            load_status.code()
                        );
                    }
                }
            }
        }

        let manifest = AutoStartManifest {
            provider: AutoStartProvider::Launchd,
            unit_name: label,
            unit_path: plist_path,
            installed_at: Utc::now(),
        };

        manifest.save(&self.manifest_path)?;
        Ok(manifest)
    }

    fn uninstall_launchd(&self, manifest: &AutoStartManifest) -> Result<()> {
        let scope = format!("gui/{}", current_uid()?);
        let full_label = format!("{}/{}", scope, manifest.unit_name);
        let status = Command::new("launchctl")
            .arg("bootout")
            .arg(&full_label)
            .status();

        if let Ok(exit) = status {
            if !exit.success() {
                // bootout returns exit code 36 when the service is not found; treat as success.
                if exit.code() != Some(36) {
                    bail!(
                        "failed to unload launchd agent {} (exit code {:?})",
                        full_label,
                        exit.code()
                    );
                }
            }
        }

        if manifest.unit_path.exists() {
            fs::remove_file(&manifest.unit_path).with_context(|| {
                format!(
                    "failed to remove launchd plist {}",
                    manifest.unit_path.display()
                )
            })?;
        }

        self.remove_manifest()?;
        Ok(())
    }

    fn start_launchd(&self, manifest: &AutoStartManifest) -> Result<Option<u32>> {
        let scope = format!("gui/{}", current_uid()?);
        let label = format!("{}/{}", scope, manifest.unit_name);

        let status = Command::new("launchctl")
            .arg("kickstart")
            .arg("-k")
            .arg(&label)
            .status()
            .context("failed to run launchctl kickstart")?;
        if !status.success() {
            bail!(
                "launchctl kickstart failed for {} (exit code {:?})",
                label,
                status.code()
            );
        }

        extract_launchd_pid(&manifest.unit_name)
    }

    fn query_launchd_status(&self, manifest: &AutoStartManifest) -> Result<AutoStartStatus> {
        let scope = format!("gui/{}", current_uid()?);
        let label = format!("{}/{}", scope, manifest.unit_name);
        let output = Command::new("launchctl")
            .arg("print")
            .arg(&label)
            .output()
            .context("failed to run launchctl print")?;

        if !output.status.success() {
            return Ok(AutoStartStatus::Disabled {
                provider: AutoStartProvider::Launchd,
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let active = stdout
            .lines()
            .any(|line| line.trim_start().starts_with("state = running"));

        Ok(AutoStartStatus::Enabled {
            provider: AutoStartProvider::Launchd,
            active,
        })
    }

    fn install_systemd(
        &self,
        vault_path: &Path,
        daemon_binary: &Path,
        embedding_model: Option<&str>,
    ) -> Result<AutoStartManifest> {
        let home = UserDirs::new()
            .map(|dirs| dirs.home_dir().to_path_buf())
            .context("failed to determine user home directory")?;
        let units_dir = home.join(".config/systemd/user");
        fs::create_dir_all(&units_dir).with_context(|| {
            format!(
                "failed to create systemd user unit directory {}",
                units_dir.display()
            )
        })?;

        let unit_name = format!("arrowheadd-{}.service", vault_slug(vault_path)?);
        let unit_path = units_dir.join(&unit_name);

        let unit = render_systemd_unit(daemon_binary, vault_path, embedding_model);
        fs::write(&unit_path, unit)
            .with_context(|| format!("failed to write systemd unit {}", unit_path.display()))?;

        Command::new("systemctl")
            .arg("--user")
            .arg("daemon-reload")
            .status()
            .context("failed to reload systemd user units")?;

        let enable = Command::new("systemctl")
            .arg("--user")
            .arg("enable")
            .arg(&unit_name)
            .status()
            .context("failed to enable systemd unit")?;
        if !enable.success() {
            bail!(
                "systemctl failed to enable {} (exit code {:?})",
                unit_name,
                enable.code()
            );
        }

        let manifest = AutoStartManifest {
            provider: AutoStartProvider::SystemdUser,
            unit_name,
            unit_path,
            installed_at: Utc::now(),
        };

        manifest.save(&self.manifest_path)?;
        Ok(manifest)
    }

    fn uninstall_systemd(&self, manifest: &AutoStartManifest) -> Result<()> {
        let disable = Command::new("systemctl")
            .arg("--user")
            .arg("disable")
            .arg("--now")
            .arg(&manifest.unit_name)
            .status();

        if let Ok(exit) = disable {
            if !exit.success() {
                bail!(
                    "systemctl disable failed for {} (exit code {:?})",
                    manifest.unit_name,
                    exit.code()
                );
            }
        }

        if manifest.unit_path.exists() {
            fs::remove_file(&manifest.unit_path).with_context(|| {
                format!(
                    "failed to remove systemd unit file {}",
                    manifest.unit_path.display()
                )
            })?;
        }

        self.remove_manifest()?;
        Ok(())
    }

    fn start_systemd(&self, manifest: &AutoStartManifest) -> Result<Option<u32>> {
        let start = Command::new("systemctl")
            .arg("--user")
            .arg("start")
            .arg(&manifest.unit_name)
            .status()
            .context("failed to start systemd unit")?;
        if !start.success() {
            bail!(
                "systemctl start failed for {} (exit code {:?})",
                manifest.unit_name,
                start.code()
            );
        }

        let output = Command::new("systemctl")
            .arg("--user")
            .arg("show")
            .arg(&manifest.unit_name)
            .arg("--property=MainPID")
            .output()
            .context("failed to query systemd unit PID")?;

        if !output.status.success() {
            return Ok(None);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if let Some(pid_str) = line.strip_prefix("MainPID=") {
                if let Ok(pid) = pid_str.trim().parse::<u32>() {
                    if pid != 0 {
                        return Ok(Some(pid));
                    }
                }
            }
        }

        Ok(None)
    }

    fn query_systemd_status(&self, manifest: &AutoStartManifest) -> Result<AutoStartStatus> {
        let enabled = Command::new("systemctl")
            .arg("--user")
            .arg("is-enabled")
            .arg(&manifest.unit_name)
            .status()
            .context("failed to check systemd enablement status")?;
        if !enabled.success() {
            return Ok(AutoStartStatus::Disabled {
                provider: AutoStartProvider::SystemdUser,
            });
        }

        let active = Command::new("systemctl")
            .arg("--user")
            .arg("is-active")
            .arg(&manifest.unit_name)
            .status()
            .context("failed to check systemd active status")?
            .success();

        Ok(AutoStartStatus::Enabled {
            provider: AutoStartProvider::SystemdUser,
            active,
        })
    }
}

/// Compute a stable slug for the vault path to use in unit identifiers.
fn vault_slug(vault_path: &Path) -> Result<String> {
    let canonical = vault_path
        .canonicalize()
        .unwrap_or_else(|_| vault_path.to_path_buf());
    let display = canonical
        .to_str()
        .ok_or_else(|| anyhow!("vault path contains invalid UTF-8: {}", canonical.display()))?;

    let mut hasher = Sha256::new();
    hasher.update(display.as_bytes());
    let digest = hasher.finalize();
    let short = &digest[..6];
    let mut hash = String::new();
    for byte in short {
        write!(&mut hash, "{:02x}", byte).expect("format hash");
    }

    let name = vault_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("vault");
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();

    Ok(format!("{}-{}", clean.trim_matches('-'), hash))
}

fn render_launchd_plist(
    label: &str,
    binary: &Path,
    vault: &Path,
    embedding_model: Option<&str>,
) -> String {
    let program = binary.display();
    let vault_path = vault.display();
    let log_path = vault.join(".arrowhead/logs/daemon.log");
    let log_display = log_path.display();
    let embedding = embedding_model.unwrap_or("none");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{program}</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>ARROWHEAD_VAULT</key>
        <string>{vault_path}</string>
        <key>ARROWHEAD_EMBEDDING_MODEL</key>
        <string>{embedding}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>StandardOutPath</key>
    <string>{log_display}</string>
    <key>StandardErrorPath</key>
    <string>{log_display}</string>
</dict>
</plist>
"#
    )
}

fn render_systemd_unit(binary: &Path, vault: &Path, embedding_model: Option<&str>) -> String {
    let program = binary.display();
    let vault_path = vault.display();
    let log_path = vault.join(".arrowhead/logs/daemon.log");
    let log_display = log_path.display();
    let embedding = embedding_model.unwrap_or("none");
    format!(
        r#"[Unit]
Description=Arrowhead daemon for {vault_path}
After=network.target

[Service]
Type=simple
ExecStart={program}
Environment="ARROWHEAD_VAULT={vault_path}"
Environment="ARROWHEAD_EMBEDDING_MODEL={embedding}"
StandardOutput=append:{log_display}
StandardError=append:{log_display}
Restart=on-failure

[Install]
WantedBy=default.target
"#
    )
}

fn current_uid() -> Result<u32> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("id")
            .arg("-u")
            .output()
            .context("failed to determine current user id")?;
        if !output.status.success() {
            bail!(
                "failed to determine uid (exit code {:?})",
                output.status.code()
            );
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.trim().parse::<u32>().context("failed to parse uid")
    }

    #[cfg(not(target_os = "macos"))]
    {
        bail!("launchd integration is not supported on this platform");
    }
}

fn extract_launchd_pid(label: &str) -> Result<Option<u32>> {
    #[cfg(target_os = "macos")]
    {
        let scope = format!("gui/{}/{}", current_uid()?, label);
        let output = Command::new("launchctl")
            .arg("print")
            .arg(&scope)
            .output()
            .context("failed to invoke launchctl print for pid extraction")?;

        if !output.status.success() {
            return Ok(None);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let trimmed = line.trim();
            if let Some(pid) = trimmed.strip_prefix("pid = ") {
                if let Ok(value) = pid.parse::<u32>() {
                    if value != 0 {
                        return Ok(Some(value));
                    }
                }
            }
        }

        Ok(None)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = label;
        Ok(None)
    }
}

/// Prompt the user with a yes/no question, returning `None` when stdin is not a
/// terminal (non-interactive).
pub fn prompt_yes_no(question: &str) -> Result<Option<bool>> {
    if !io::stdin().is_terminal() {
        return Ok(None);
    }

    print!("{question} [y/N]: ");
    io::stdout().flush().ok();

    let mut buffer = String::new();
    io::stdin()
        .read_line(&mut buffer)
        .context("failed to read user input")?;

    let response = buffer.trim().to_ascii_lowercase();
    if response.is_empty() {
        return Ok(Some(false));
    }

    match response.as_str() {
        "y" | "yes" => Ok(Some(true)),
        "n" | "no" => Ok(Some(false)),
        _ => Ok(Some(false)),
    }
}
