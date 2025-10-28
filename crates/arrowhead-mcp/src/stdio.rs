//! stdio transport implementation
//!
//! JSON-RPC 2.0 over stdin/stdout for local MCP connections.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, anyhow};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, BufWriter},
    sync::{Semaphore, mpsc, mpsc::error::TrySendError},
    task::JoinSet,
};
use tracing::{Instrument, debug, error, info, trace, warn};

use crate::protocol::{
    ErrorCode, Id, Incoming, Message, Notification, ProtocolError, Request, Response,
};

pub use crate::transport::MessageHandler;

/// Default size of the inbound request queue.
const DEFAULT_CHANNEL_CAPACITY: usize = 64;
/// Default number of concurrent request workers.
const DEFAULT_MAX_CONCURRENCY: usize = 4;

/// Configuration for the stdio server.
#[derive(Debug, Clone)]
pub struct StdioServerConfig {
    /// Bounded channel capacity for pending requests.
    pub channel_capacity: usize,
    /// Maximum number of concurrent request handler tasks.
    pub max_concurrency: usize,
}

impl Default for StdioServerConfig {
    fn default() -> Self {
        Self {
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
        }
    }
}

#[derive(Debug, Clone)]
/// Runtime metrics collected by the stdio server.
pub struct StdioMetrics {
    inner: Arc<StdioMetricsInner>,
}

#[derive(Debug, Default)]
struct StdioMetricsInner {
    accepted_requests: AtomicU64,
    completed_requests: AtomicU64,
    failed_requests: AtomicU64,
    rejected_requests: AtomicU64,
    active_requests: AtomicU64,
    notifications_received: AtomicU64,
    notifications_failed: AtomicU64,
    notifications_dropped: AtomicU64,
    parse_errors: AtomicU64,
}

impl Default for StdioMetrics {
    fn default() -> Self {
        Self {
            inner: Arc::new(StdioMetricsInner::default()),
        }
    }
}

/// Snapshot of the stdio server metrics.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdioMetricsSnapshot {
    /// Total number of requests accepted into the processing queue.
    pub accepted_requests: u64,
    /// Requests that completed successfully.
    pub completed_requests: u64,
    /// Requests that returned an error response.
    pub failed_requests: u64,
    /// Requests refused due to backpressure.
    pub rejected_requests: u64,
    /// Requests currently executing.
    pub active_requests: u64,
    /// Notifications passed to handlers.
    pub notifications_received: u64,
    /// Notifications whose handlers returned errors.
    pub notifications_failed: u64,
    /// Notifications dropped because the queue was saturated.
    pub notifications_dropped: u64,
    /// Frames rejected during JSON parsing.
    pub parse_errors: u64,
}

impl StdioMetrics {
    fn record_request_enqueued(&self) {
        self.inner.accepted_requests.fetch_add(1, Ordering::Relaxed);
    }

    fn record_request_started(&self) {
        self.inner.active_requests.fetch_add(1, Ordering::Relaxed);
    }

    fn record_request_completed(&self) {
        self.inner
            .completed_requests
            .fetch_add(1, Ordering::Relaxed);
        self.inner.active_requests.fetch_sub(1, Ordering::Relaxed);
    }

    fn record_request_failed(&self) {
        self.inner.failed_requests.fetch_add(1, Ordering::Relaxed);
        self.inner.active_requests.fetch_sub(1, Ordering::Relaxed);
    }

    fn record_request_rejected(&self) {
        self.inner.rejected_requests.fetch_add(1, Ordering::Relaxed);
    }

    fn record_notification_enqueued(&self) {
        self.inner
            .notifications_received
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_notification_failed(&self) {
        self.inner
            .notifications_failed
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_notification_dropped(&self) {
        self.inner
            .notifications_dropped
            .fetch_add(1, Ordering::Relaxed);
    }

    fn record_parse_error(&self) {
        self.inner.parse_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Capture a snapshot of the current metrics.
    pub fn snapshot(&self) -> StdioMetricsSnapshot {
        StdioMetricsSnapshot {
            accepted_requests: self.inner.accepted_requests.load(Ordering::Relaxed),
            completed_requests: self.inner.completed_requests.load(Ordering::Relaxed),
            failed_requests: self.inner.failed_requests.load(Ordering::Relaxed),
            rejected_requests: self.inner.rejected_requests.load(Ordering::Relaxed),
            active_requests: self.inner.active_requests.load(Ordering::Relaxed),
            notifications_received: self.inner.notifications_received.load(Ordering::Relaxed),
            notifications_failed: self.inner.notifications_failed.load(Ordering::Relaxed),
            notifications_dropped: self.inner.notifications_dropped.load(Ordering::Relaxed),
            parse_errors: self.inner.parse_errors.load(Ordering::Relaxed),
        }
    }
}

/// JSON-RPC server operating over stdin/stdout.
pub struct StdioServer<H>
where
    H: MessageHandler,
{
    handler: Arc<H>,
    config: StdioServerConfig,
    trace_seq: AtomicU64,
    metrics: StdioMetrics,
}

impl<H> StdioServer<H>
where
    H: MessageHandler,
{
    /// Construct a stdio server using default configuration.
    pub fn new(handler: Arc<H>) -> Self {
        Self::with_config(handler, StdioServerConfig::default())
    }

    /// Construct a stdio server with the supplied configuration.
    pub fn with_config(handler: Arc<H>, config: StdioServerConfig) -> Self {
        let max_concurrency = config.max_concurrency.max(1);
        let channel_capacity = config.channel_capacity.max(1);
        Self {
            handler,
            config: StdioServerConfig {
                max_concurrency,
                channel_capacity,
            },
            trace_seq: AtomicU64::new(1),
            metrics: StdioMetrics::default(),
        }
    }

    /// Access the runtime metrics collected by this server.
    #[must_use]
    pub fn metrics(&self) -> StdioMetrics {
        self.metrics.clone()
    }

    /// Determine whether the server is currently experiencing backpressure.
    #[must_use]
    pub fn is_saturated(&self) -> bool {
        let snapshot = self.metrics.snapshot();
        let finished = snapshot
            .completed_requests
            .saturating_add(snapshot.failed_requests);
        let outstanding = snapshot.accepted_requests.saturating_sub(finished);
        outstanding as usize >= self.config.channel_capacity
    }

    /// Check whether new requests are likely to be accepted without backpressure.
    #[must_use]
    pub fn is_accepting_requests(&self) -> bool {
        !self.is_saturated()
    }

    /// Run the stdio server until EOF or a fatal error occurs.
    pub async fn run(&self) -> Result<()> {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        self.run_with_io(stdin, stdout).await
    }

    /// Run the stdio server with injected reader/writer streams (useful for tests).
    pub async fn run_with_io<R, W>(&self, reader: R, writer: W) -> Result<()>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let reader = BufReader::new(reader);
        let writer = BufWriter::new(writer);
        self.run_with_buffers(reader, writer).await
    }

    fn next_trace_id(&self) -> u64 {
        self.trace_seq.fetch_add(1, Ordering::Relaxed)
    }

    async fn run_with_buffers<R, W>(&self, reader: BufReader<R>, writer: BufWriter<W>) -> Result<()>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (request_tx, request_rx) = mpsc::channel(self.config.channel_capacity);
        let (response_tx, response_rx) = mpsc::channel(self.config.channel_capacity);

        let dispatcher_handler = Arc::clone(&self.handler);
        let dispatcher_config = self.config.clone();
        let dispatcher_responses = response_tx.clone();
        let dispatcher_metrics = self.metrics.clone();
        let dispatcher = tokio::spawn(async move {
            dispatch_loop(
                dispatcher_handler,
                dispatcher_metrics,
                dispatcher_config.max_concurrency,
                request_rx,
                dispatcher_responses,
            )
            .await;
        });

        let writer_task =
            tokio::spawn(async move { response_writer_loop(writer, response_rx).await });

        let reader_result = self
            .reader_loop(reader, request_tx, response_tx.clone())
            .await
            .context("stdio reader loop failed");

        drop(response_tx);

        dispatcher
            .await
            .context("dispatcher task aborted unexpectedly")?;
        writer_task
            .await
            .context("writer task aborted unexpectedly")?
            .context("writer loop failed")?;

        reader_result
    }

    async fn reader_loop(
        &self,
        reader: BufReader<impl AsyncRead + Unpin + Send>,
        request_tx: mpsc::Sender<DispatchEnvelope>,
        response_tx: mpsc::Sender<ResponseEnvelope>,
    ) -> Result<()> {
        let mut reader = reader;
        let mut buffer = String::new();

        loop {
            buffer.clear();
            let bytes_read = reader
                .read_line(&mut buffer)
                .await
                .context("failed to read from stdin")?;
            if bytes_read == 0 {
                info!("stdin closed; shutting down stdio transport");
                break;
            }

            let frame = buffer.trim();
            if frame.is_empty() {
                continue;
            }

            match Incoming::parse_str(frame) {
                Ok(incoming) => {
                    self.dispatch_incoming(incoming, &request_tx, &response_tx)
                        .await?;
                }
                Err(error) => {
                    let trace_id = self.next_trace_id();
                    self.metrics.record_parse_error();
                    warn!(
                        trace_id,
                        error = %error,
                        "failed to parse JSON-RPC frame"
                    );
                    let rpc_error = error.into_rpc();
                    if let Err(send_err) = response_tx
                        .send(ResponseEnvelope {
                            trace_id,
                            response: Response::error(Id::Null, rpc_error),
                        })
                        .await
                    {
                        error!(
                            trace_id,
                            error = %send_err,
                            "failed to enqueue parse error response"
                        );
                    }
                }
            }
        }

        Ok(())
    }

    async fn dispatch_incoming(
        &self,
        incoming: Incoming,
        request_tx: &mpsc::Sender<DispatchEnvelope>,
        response_tx: &mpsc::Sender<ResponseEnvelope>,
    ) -> Result<()> {
        match incoming {
            Incoming::Single(message) => {
                self.dispatch_message(message, request_tx, response_tx)
                    .await?;
            }
            Incoming::Batch(messages) => {
                if messages.is_empty() {
                    let trace_id = self.next_trace_id();
                    let rpc_error =
                        ProtocolError::invalid_request("batch must contain at least one message")
                            .into_rpc();
                    response_tx
                        .send(ResponseEnvelope {
                            trace_id,
                            response: Response::error(Id::Null, rpc_error),
                        })
                        .await
                        .map_err(|err| anyhow!("response channel closed: {err}"))?;
                    return Ok(());
                }

                for message in messages {
                    self.dispatch_message(message, request_tx, response_tx)
                        .await?;
                }
            }
        }

        Ok(())
    }

    async fn dispatch_message(
        &self,
        message: Message,
        request_tx: &mpsc::Sender<DispatchEnvelope>,
        response_tx: &mpsc::Sender<ResponseEnvelope>,
    ) -> Result<()> {
        match message {
            Message::Request(request) => {
                let trace_id = self.next_trace_id();
                let method = request.method.clone();
                let id = request.id.clone();
                trace!(
                    trace_id,
                    method = %method,
                    id = %id,
                    "dispatching request"
                );
                let envelope = DispatchEnvelope {
                    trace_id,
                    message: DispatchMessage::Request(request),
                };
                match request_tx.try_send(envelope) {
                    Ok(()) => {
                        self.metrics.record_request_enqueued();
                    }
                    Err(TrySendError::Full(envelope)) => {
                        self.metrics.record_request_rejected();
                        let request = match envelope.message {
                            DispatchMessage::Request(req) => req,
                            DispatchMessage::Notification(_) => unreachable!(),
                        };
                        warn!(trace_id, method = %method, "dropping request due to saturated queue");
                        let rpc_error = ProtocolError::custom(
                            ErrorCode::RateLimited,
                            "too many pending requests. Slow down and retry.",
                            None,
                        )
                        .into_rpc();
                        response_tx
                            .send(ResponseEnvelope {
                                trace_id,
                                response: Response::error(request.id.clone(), rpc_error),
                            })
                            .await
                            .map_err(|err| anyhow!("response channel closed: {err}"))?;
                    }
                    Err(TrySendError::Closed(_)) => {
                        return Err(anyhow!("request channel closed"));
                    }
                }
            }
            Message::Notification(notification) => {
                let trace_id = self.next_trace_id();
                let method = notification.method.clone();
                trace!(
                    trace_id,
                    method = %method,
                    "dispatching notification"
                );
                let envelope = DispatchEnvelope {
                    trace_id,
                    message: DispatchMessage::Notification(notification),
                };
                match request_tx.try_send(envelope) {
                    Ok(()) => {
                        self.metrics.record_notification_enqueued();
                    }
                    Err(TrySendError::Full(_)) => {
                        self.metrics.record_notification_dropped();
                        warn!(
                            trace_id,
                            method = %method,
                            "dropping notification due to saturated queue"
                        );
                    }
                    Err(TrySendError::Closed(_)) => {
                        return Err(anyhow!("request channel closed"));
                    }
                }
            }
            Message::Response(_) => {
                let trace_id = self.next_trace_id();
                warn!(trace_id, "unexpected response received by server; ignoring");
                let rpc_error =
                    ProtocolError::invalid_request("received response message from client")
                        .into_rpc();
                response_tx
                    .send(ResponseEnvelope {
                        trace_id,
                        response: Response::error(Id::Null, rpc_error),
                    })
                    .await
                    .map_err(|err| anyhow!("response channel closed: {err}"))?;
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
struct DispatchEnvelope {
    trace_id: u64,
    message: DispatchMessage,
}

#[derive(Debug)]
enum DispatchMessage {
    Request(Request),
    Notification(Notification),
}

#[derive(Debug)]
struct ResponseEnvelope {
    trace_id: u64,
    response: Response,
}

async fn dispatch_loop<H>(
    handler: Arc<H>,
    metrics: StdioMetrics,
    max_concurrency: usize,
    mut request_rx: mpsc::Receiver<DispatchEnvelope>,
    response_tx: mpsc::Sender<ResponseEnvelope>,
) where
    H: MessageHandler,
{
    let concurrency = max_concurrency.max(1);
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let mut join_set = JoinSet::new();

    while let Some(envelope) = request_rx.recv().await {
        let permit = match semaphore.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(error) => {
                error!(%error, "failed to acquire worker permit");
                break;
            }
        };

        let handler = Arc::clone(&handler);
        let responses = response_tx.clone();
        let worker_metrics = metrics.clone();

        join_set.spawn(async move {
            let _permit = permit;
            match envelope.message {
                DispatchMessage::Request(request) => {
                    worker_metrics.record_request_started();
                    let span = tracing::info_span!(
                        "mcp_request",
                        trace_id = envelope.trace_id,
                        method = %request.method,
                        id = %request.id
                    );
                    async move {
                        let request_id = request.id.clone();
                        match handler.handle_request(request).await {
                            Ok(result) => {
                                if let Err(err) = responses
                                    .send(ResponseEnvelope {
                                        trace_id: envelope.trace_id,
                                        response: Response::success(request_id, result),
                                    })
                                    .await
                                {
                                    error!(
                                        trace_id = envelope.trace_id,
                                        error = %err,
                                        "failed to enqueue success response"
                                    );
                                    worker_metrics.record_request_failed();
                                } else {
                                    trace!(
                                        trace_id = envelope.trace_id,
                                        "request processing completed"
                                    );
                                    worker_metrics.record_request_completed();
                                }
                            }
                            Err(error) => {
                                let rpc_error = error.into_rpc();
                                if let Err(err) = responses
                                    .send(ResponseEnvelope {
                                        trace_id: envelope.trace_id,
                                        response: Response::error(request_id, rpc_error),
                                    })
                                    .await
                                {
                                    error!(
                                        trace_id = envelope.trace_id,
                                        error = %err,
                                        "failed to enqueue error response"
                                    );
                                    worker_metrics.record_request_failed();
                                } else {
                                    debug!(
                                        trace_id = envelope.trace_id,
                                        "request completed with error response"
                                    );
                                    worker_metrics.record_request_failed();
                                }
                            }
                        }
                    }
                    .instrument(span)
                    .await;
                }
                DispatchMessage::Notification(notification) => {
                    let span = tracing::debug_span!(
                        "mcp_notification",
                        trace_id = envelope.trace_id,
                        method = %notification.method
                    );
                    async move {
                        if let Err(error) = handler.handle_notification(notification).await {
                            warn!(
                                trace_id = envelope.trace_id,
                                error = %error,
                                "notification handler returned error"
                            );
                            worker_metrics.record_notification_failed();
                        }
                    }
                    .instrument(span)
                    .await;
                }
            }
        });

        while let Some(result) = join_set.try_join_next() {
            if let Err(join_error) = result {
                error!(error = %join_error, "request worker panicked");
            }
        }
    }

    while let Some(result) = join_set.join_next().await {
        if let Err(join_error) = result {
            error!(error = %join_error, "request worker panicked");
        }
    }
}

async fn response_writer_loop<W>(
    mut writer: BufWriter<W>,
    mut response_rx: mpsc::Receiver<ResponseEnvelope>,
) -> Result<()>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    while let Some(envelope) = response_rx.recv().await {
        let span = tracing::info_span!("mcp_response", trace_id = envelope.trace_id);
        async {
            let payload = serde_json::to_string(&envelope.response)
                .context("failed to serialise response payload")?;
            writer
                .write_all(payload.as_bytes())
                .await
                .context("failed to write response payload")?;
            writer
                .write_all(b"\n")
                .await
                .context("failed to write newline terminator")?;
            writer.flush().await.context("failed to flush writer")?;
            Ok::<(), anyhow::Error>(())
        }
        .instrument(span)
        .await?;
    }

    writer.flush().await.context("failed to flush writer")?;
    Ok(())
}
