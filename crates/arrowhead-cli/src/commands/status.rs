//! `arrowhead status` command implementation.
//!
//! Streams live daemon status frames or prints a snapshot fallback.

use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
};

use anyhow::{Context, Result};
use arrowhead_core::{
    ActivityState, DeamonStatus, DownloadState, IssueSeverity, StatusFrame, Vault, VaultConfig,
};
use arrowhead_deamon::{StatusStream, status_stream};
use clap::Args;
use serde_json;
use tokio::signal;

use super::CommandContext;

/// CLI arguments for the `status` command.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct StatusCommand {
    /// Emit newline-delimited JSON frames instead of human output.
    #[arg(long)]
    pub json: bool,
}

/// Run the `status` command.
pub async fn run(ctx: &CommandContext, command: &StatusCommand) -> Result<()> {
    let vault_path = resolve_vault_path(ctx)?;
    let vault = Vault::new(VaultConfig::new(vault_path.clone()))?;
    vault.ensure_arrowhead_dirs()?;

    let paths = vault.paths();
    let socket_path = paths.arrowhead_dir.join("deamon/control.sock");
    let status_path = paths.arrowhead_dir.join("deamon/status.json");
    let stdout_is_tty = io::stdout().is_terminal();

    match status_stream(&socket_path).await {
        Ok(mut stream) => {
            if !command.json {
                println!(
                    "Streaming daemon status from {} (Ctrl+C to exit).\n",
                    socket_path.display()
                );
            }
            stream_frames(&mut stream, command.json, stdout_is_tty).await
        }
        Err(err) => {
            let snapshot = DeamonStatus::load_from_path(&status_path)?;
            if let Some(status) = snapshot {
                if command.json {
                    let frame = StatusFrame::new(status);
                    println!("{}", serde_json::to_string(&frame)?);
                } else {
                    println!(
                        "Daemon stream unavailable ({}). Showing latest snapshot.\n",
                        err
                    );
                    render_snapshot(&status, stdout_is_tty);
                }
                Ok(())
            } else {
                Err(err.context("failed to connect to daemon status stream"))
            }
        }
    }
}

fn resolve_vault_path(ctx: &CommandContext) -> Result<PathBuf> {
    ctx.config
        .vault
        .clone()
        .context("no vault configured. Provide --vault or run `arrowhead init` first")
}

async fn stream_frames(stream: &mut StatusStream, json: bool, tty: bool) -> Result<()> {
    loop {
        tokio::select! {
            biased;
            _ = signal::ctrl_c() => {
                if !json {
                    println!("\nReceived Ctrl+C. Stopping status stream.");
                }
                break;
            }
            frame = stream.next() => {
                match frame? {
                    Some(frame) => {
                        if json {
                            println!("{}", serde_json::to_string(&frame)?);
                        } else {
                            render_frame(&frame, tty)?;
                        }
                    }
                    None => {
                        if !json {
                            println!("Daemon closed the status stream.");
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
        // Clear the screen before rendering the latest frame to avoid an ever-growing log.
        print!("\u{001b}[2J\u{001b}[H");
    } else {
        println!();
        println!("[{}]", frame.emitted_at.to_rfc3339());
    }

    render_snapshot(&frame.status, tty);
    io::stdout().flush().ok();
    Ok(())
}

fn render_snapshot(status: &DeamonStatus, tty: bool) {
    if !tty {
        println!("Updated: {}", status.updated_at.to_rfc3339());
    } else {
        println!("arrowhead daemon status");
        println!("=======================");
        println!("Updated: {}", status.updated_at.to_rfc3339());
    }
    let activity_label = status
        .activity
        .description
        .as_deref()
        .unwrap_or_else(|| describe_activity(status.activity.state));
    println!("Activity: {}", activity_label);
    if let Some(note_id) = &status.activity.note_id {
        println!("  Note: {}", note_id);
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
                println!("    {}", detail);
            }
        }
    }

    println!("Log: {}", status.log_path.display());
    println!();
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

fn describe_download_state(state: DownloadState) -> &'static str {
    match state {
        DownloadState::Pending => "pending",
        DownloadState::InProgress => "in-progress",
        DownloadState::Completed => "completed",
        DownloadState::Failed => "failed",
    }
}

fn describe_issue_severity(severity: IssueSeverity) -> &'static str {
    match severity {
        IssueSeverity::Info => "info",
        IssueSeverity::Warning => "warning",
        IssueSeverity::Error => "error",
    }
}
