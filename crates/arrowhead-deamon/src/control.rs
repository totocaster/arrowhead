use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow};
use arrowhead_core::DeamonStatus;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, broadcast},
};
use tracing::{error, info};

/// Control plane request supported by the deamon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlRequest {
    /// Query the current status snapshot.
    Status,
    /// Request a graceful shutdown.
    Shutdown,
}

/// Response envelope emitted by the control plane.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlResponse {
    /// Successful status response.
    Status { status: DeamonStatus },
    /// Acknowledge shutdown request.
    ShutdownAck,
    /// Error handling the command.
    Error { message: String },
}

#[cfg(unix)]
pub async fn run_control_server(
    socket_path: PathBuf,
    status: Arc<Mutex<DeamonStatus>>,
    shutdown_tx: broadcast::Sender<()>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<()> {
    use tokio::net::UnixListener;

    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent).await.with_context(|| {
            format!(
                "failed to create control socket directory {}",
                parent.display()
            )
        })?;
    }
    if socket_path.exists() {
        tokio::fs::remove_file(&socket_path)
            .await
            .with_context(|| format!("failed to remove stale socket {}", socket_path.display()))?;
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind control socket {}", socket_path.display()))?;
    info!(
        socket = %socket_path.display(),
        "deamon control socket ready"
    );

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _addr)) => {
                        let status = Arc::clone(&status);
                        let shutdown = shutdown_tx.clone();
                        tokio::spawn(async move {
                            if let Err(err) = handle_connection(stream, status, shutdown).await {
                                error!(error = ?err, "control connection failed");
                            }
                        });
                    }
                    Err(err) => {
                        error!(error = %err, "failed to accept control connection");
                    }
                }
            }
            _ = shutdown_rx.recv() => {
                break;
            }
        }
    }

    if socket_path.exists() {
        tokio::fs::remove_file(&socket_path)
            .await
            .with_context(|| {
                format!("failed to remove control socket {}", socket_path.display())
            })?;
    }

    info!("control server shutdown complete");
    Ok(())
}

#[cfg(unix)]
async fn handle_connection(
    stream: tokio::net::UnixStream,
    status: Arc<Mutex<DeamonStatus>>,
    shutdown_tx: broadcast::Sender<()>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .await
        .context("failed to read control command")?;
    if n == 0 {
        return Ok(());
    }

    let request: ControlRequest =
        serde_json::from_str(line.trim()).context("failed to parse control request")?;

    let response = match request {
        ControlRequest::Status => {
            let snapshot = {
                let status = status.lock().await;
                status.clone()
            };
            ControlResponse::Status { status: snapshot }
        }
        ControlRequest::Shutdown => {
            let _ = shutdown_tx.send(());
            ControlResponse::ShutdownAck
        }
    };

    let payload = serde_json::to_vec(&response).context("failed to serialise response")?;
    writer
        .write_all(&payload)
        .await
        .context("failed to write control response")?;
    writer
        .write_all(b"\n")
        .await
        .context("failed to terminate control response")?;
    writer.flush().await.context("failed to flush response")?;
    Ok(())
}

#[cfg(not(unix))]
pub async fn run_control_server(
    _socket_path: PathBuf,
    _status: Arc<Mutex<DeamonStatus>>,
    _shutdown_tx: broadcast::Sender<()>,
    mut shutdown_rx: broadcast::Receiver<()>,
) -> Result<()> {
    debug!("control server disabled on non-unix platform");
    let _ = shutdown_rx.recv().await;
    Ok(())
}

/// Send a control request over the Unix socket.
#[cfg(unix)]
pub async fn send_control_request<P: AsRef<Path>>(
    socket_path: P,
    request: ControlRequest,
) -> Result<ControlResponse> {
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path.as_ref())
        .await
        .with_context(|| {
            format!(
                "failed to connect to control socket {}",
                socket_path.as_ref().display()
            )
        })?;

    let payload = serde_json::to_vec(&request).context("failed to serialise control request")?;
    stream
        .write_all(&payload)
        .await
        .context("failed to send control request")?;
    stream
        .write_all(b"\n")
        .await
        .context("failed to terminate control request")?;
    stream.flush().await.context("failed to flush request")?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .await
        .context("failed to read control response")?;
    if bytes == 0 {
        return Err(anyhow!("control socket closed without response"));
    }

    let response: ControlResponse =
        serde_json::from_str(line.trim()).context("failed to parse control response")?;
    Ok(response)
}

#[cfg(not(unix))]
pub async fn send_control_request<P: AsRef<Path>>(
    _socket_path: P,
    _request: ControlRequest,
) -> Result<ControlResponse> {
    Err(anyhow!("control socket is unavailable on this platform"))
}
