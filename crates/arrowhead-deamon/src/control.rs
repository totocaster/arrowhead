use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, anyhow};
use arrowhead_core::{DeamonStatus, StatusFrame};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter},
    sync::{Mutex, broadcast},
};
use tracing::{error, info};

/// Control plane request supported by the deamon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlRequest {
    /// Query the current status snapshot.
    StatusSnapshot,
    /// Subscribe to a real-time stream of status frames.
    StatusSubscribe,
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
    frames: broadcast::Sender<StatusFrame>,
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
                        let frames = frames.clone();
                        let shutdown = shutdown_tx.clone();
                        tokio::spawn(async move {
                            if let Err(err) =
                                handle_connection(stream, status, frames, shutdown).await
                            {
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
    frames: broadcast::Sender<StatusFrame>,
    shutdown_tx: broadcast::Sender<()>,
) -> Result<()> {
    let (reader, writer) = stream.into_split();
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

    match request {
        ControlRequest::StatusSnapshot => {
            let snapshot = {
                let status = status.lock().await;
                status.clone()
            };
            let response = ControlResponse::Status { status: snapshot };
            let mut writer = BufWriter::new(writer);
            write_response(&mut writer, &response).await?;
            return Ok(());
        }
        ControlRequest::StatusSubscribe => {
            let writer = BufWriter::new(writer);
            stream_status(writer, status, frames).await?;
            return Ok(());
        }
        ControlRequest::Shutdown => {
            let _ = shutdown_tx.send(());
            let mut writer = BufWriter::new(writer);
            let response = ControlResponse::ShutdownAck;
            write_response(&mut writer, &response).await?;
            return Ok(());
        }
    }
}

async fn write_response<W, T>(writer: &mut BufWriter<W>, value: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(value).context("failed to serialise control response")?;
    writer
        .write_all(&payload)
        .await
        .context("failed to write control response")?;
    writer
        .write_all(b"\n")
        .await
        .context("failed to terminate control response")?;
    writer.flush().await.context("failed to flush response")
}

async fn stream_status(
    mut writer: BufWriter<tokio::net::unix::OwnedWriteHalf>,
    status: Arc<Mutex<DeamonStatus>>,
    frames: broadcast::Sender<StatusFrame>,
) -> Result<()> {
    let initial = {
        let snapshot = status.lock().await.clone();
        StatusFrame::new(snapshot)
    };
    write_response(&mut writer, &initial).await?;

    let mut receiver = frames.subscribe();
    loop {
        match receiver.recv().await {
            Ok(frame) => write_response(&mut writer, &frame).await?,
            Err(broadcast::error::RecvError::Closed) => break,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let snapshot = status.lock().await.clone();
                let frame = StatusFrame::new(snapshot);
                write_response(&mut writer, &frame).await?;
            }
        }
    }

    Ok(())
}

#[cfg(not(unix))]
pub async fn run_control_server(
    _socket_path: PathBuf,
    _status: Arc<Mutex<DeamonStatus>>,
    _frames: broadcast::Sender<StatusFrame>,
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
    if matches!(request, ControlRequest::StatusSubscribe) {
        return Err(anyhow!(
            "status subscribe requires `status_stream` API instead of `send_control_request`"
        ));
    }

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

/// Subscribe to the live status stream over the control socket.
#[cfg(unix)]
pub async fn status_stream<P: AsRef<Path>>(socket_path: P) -> Result<StatusStream> {
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path.as_ref())
        .await
        .with_context(|| {
            format!(
                "failed to connect to control socket {}",
                socket_path.as_ref().display()
            )
        })?;

    let payload = serde_json::to_vec(&ControlRequest::StatusSubscribe)
        .context("failed to serialise status subscribe request")?;
    stream
        .write_all(&payload)
        .await
        .context("failed to send status subscribe request")?;
    stream
        .write_all(b"\n")
        .await
        .context("failed to terminate status subscribe request")?;
    stream.flush().await.context("failed to flush request")?;

    Ok(StatusStream {
        reader: BufReader::new(stream),
    })
}

/// Streaming handle that yields status frames until the connection closes.
#[cfg(unix)]
pub struct StatusStream {
    reader: BufReader<tokio::net::UnixStream>,
}

#[cfg(unix)]
impl StatusStream {
    /// Read the next frame from the stream, returning `None` when the daemon closes the socket.
    pub async fn next(&mut self) -> Result<Option<StatusFrame>> {
        let mut line = String::new();
        let bytes = self
            .reader
            .read_line(&mut line)
            .await
            .context("failed to read status frame")?;
        if bytes == 0 {
            return Ok(None);
        }
        let frame: StatusFrame =
            serde_json::from_str(line.trim()).context("failed to parse status frame")?;
        Ok(Some(frame))
    }
}

#[cfg(not(unix))]
pub async fn send_control_request<P: AsRef<Path>>(
    _socket_path: P,
    request: ControlRequest,
) -> Result<ControlResponse> {
    if matches!(request, ControlRequest::StatusSubscribe) {
        return Err(anyhow!(
            "status subscribe requires `status_stream` API instead of `send_control_request`"
        ));
    }
    Err(anyhow!("control socket is unavailable on this platform"))
}

/// Subscribe to the live status stream over the control socket (unsupported on non-Unix).
#[cfg(not(unix))]
pub async fn status_stream<P: AsRef<Path>>(_socket_path: P) -> Result<StatusStream> {
    Err(anyhow!("status streaming is unavailable on this platform"))
}

/// Dummy status stream placeholder for non-Unix builds.
#[cfg(not(unix))]
pub struct StatusStream;

#[cfg(not(unix))]
impl StatusStream {
    /// Streaming is unsupported on non-Unix platforms.
    pub async fn next(&mut self) -> Result<Option<StatusFrame>> {
        Err(anyhow!("status streaming is unavailable on this platform"))
    }
}
