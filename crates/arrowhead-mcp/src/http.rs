//! HTTP transport implementation
//!
//! JSON-RPC 2.0 over HTTP for remote MCP connections.

use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use anyhow::{Context, Result};
use axum::{
    Router,
    body::Bytes,
    extract::{Path, State, connect_info::ConnectInfo},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use serde_json::Value;
use tokio::{
    net::TcpListener,
    sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError},
};
use tower_http::{
    limit::RequestBodyLimitLayer,
    trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer},
};
use tracing::{Instrument, Level, debug, error, info, trace, warn};

use crate::{
    auth::{AuthConfig, AuthFailure, AuthMode, AuthSource, Authenticator, IpAllowList},
    protocol::{ErrorCode, Id, Incoming, Message, ProtocolError, Response as RpcResponse},
    transport::MessageHandler,
};

/// Default bind address for the HTTP server.
pub const DEFAULT_BIND_ADDRESS: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3911);
/// Default maximum number of concurrent in-flight requests.
const DEFAULT_MAX_CONCURRENCY: usize = 8;
/// Default maximum size of an incoming request body in bytes.
const DEFAULT_MAX_BODY_BYTES: usize = 512 * 1024;

/// Configuration for the MCP HTTP server.
#[derive(Debug, Clone)]
pub struct HttpServerConfig {
    /// Address the server should bind to.
    pub bind_address: SocketAddr,
    /// Maximum concurrent in-flight JSON-RPC requests.
    pub max_concurrency: usize,
    /// Maximum request body size in bytes.
    pub max_body_bytes: usize,
    /// Authentication configuration governing incoming requests.
    pub auth: AuthConfig,
    /// IP addresses permitted to communicate with the server.
    pub ip_allowlist: IpAllowList,
}

impl Default for HttpServerConfig {
    fn default() -> Self {
        Self {
            bind_address: DEFAULT_BIND_ADDRESS,
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            auth: AuthConfig::default(),
            ip_allowlist: IpAllowList::default(),
        }
    }
}

impl HttpServerConfig {
    fn normalised(self) -> Self {
        Self {
            bind_address: self.bind_address,
            max_concurrency: self.max_concurrency.max(1),
            max_body_bytes: self.max_body_bytes.max(1),
            auth: self.auth,
            ip_allowlist: self.ip_allowlist,
        }
    }
}

/// JSON-RPC HTTP server.
pub struct HttpServer<H>
where
    H: MessageHandler,
{
    handler: Arc<H>,
    config: HttpServerConfig,
    authenticator: Authenticator,
    ip_allowlist: IpAllowList,
}

impl<H> HttpServer<H>
where
    H: MessageHandler,
{
    /// Construct a new HTTP server using the provided configuration.
    pub fn new(handler: Arc<H>, config: HttpServerConfig) -> Result<Self> {
        let config = config.normalised();
        let authenticator = config
            .auth
            .build()
            .context("no authentication tokens configured")?;

        Ok(Self {
            handler,
            authenticator,
            ip_allowlist: config.ip_allowlist.clone(),
            config,
        })
    }

    /// Construct a new server using default transport limits and the supplied auth settings.
    pub fn with_auth(handler: Arc<H>, auth: AuthConfig) -> Result<Self> {
        let config = HttpServerConfig {
            auth,
            ..HttpServerConfig::default()
        };
        Self::new(handler, config)
    }

    /// Access the effective configuration.
    pub fn config(&self) -> &HttpServerConfig {
        &self.config
    }

    /// Start serving HTTP traffic until the provided shutdown future resolves.
    pub async fn serve(self, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<()> {
        let addr = self.config.bind_address;
        let app = self.router();

        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind HTTP listener on {addr}"))?;
        let actual_addr = listener
            .local_addr()
            .context("failed to read bound HTTP listener address")?;

        info!(%actual_addr, "starting MCP HTTP server");

        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .await
        .context("HTTP server failed")?;

        info!(%actual_addr, "MCP HTTP server stopped");

        Ok(())
    }

    /// Build an Axum router representing this server.
    pub fn router(&self) -> Router {
        let state = HttpServerState::new(
            Arc::clone(&self.handler),
            self.config.max_concurrency,
            self.authenticator.clone(),
            self.ip_allowlist.clone(),
        );

        let trace_layer = TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
            .on_response(
                DefaultOnResponse::new()
                    .level(Level::INFO)
                    .latency_unit(tower_http::LatencyUnit::Micros),
            );

        Router::new()
            .route("/health", get(handle_health))
            .route("/rpc", post(handle_rpc::<H>))
            .route("/rpc/{token}", post(handle_rpc_with_token::<H>))
            .with_state(state)
            .layer(trace_layer)
            .layer(RequestBodyLimitLayer::new(self.config.max_body_bytes))
    }
}

struct HttpServerState<H>
where
    H: MessageHandler,
{
    handler: Arc<H>,
    concurrency: Arc<Semaphore>,
    authenticator: Authenticator,
    ip_allowlist: IpAllowList,
}

impl<H> Clone for HttpServerState<H>
where
    H: MessageHandler,
{
    fn clone(&self) -> Self {
        Self {
            handler: Arc::clone(&self.handler),
            concurrency: Arc::clone(&self.concurrency),
            authenticator: self.authenticator.clone(),
            ip_allowlist: self.ip_allowlist.clone(),
        }
    }
}

impl<H> HttpServerState<H>
where
    H: MessageHandler,
{
    fn new(
        handler: Arc<H>,
        max_concurrency: usize,
        authenticator: Authenticator,
        ip_allowlist: IpAllowList,
    ) -> Self {
        let concurrency = Arc::new(Semaphore::new(max_concurrency.max(1)));
        Self {
            handler,
            concurrency,
            authenticator,
            ip_allowlist,
        }
    }

    fn handler(&self) -> Arc<H> {
        Arc::clone(&self.handler)
    }

    fn try_acquire(&self) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        self.concurrency.clone().try_acquire_owned()
    }

    fn authenticate(
        &self,
        bearer_header: Option<&str>,
        link_token: Option<&str>,
    ) -> Result<AuthSource, AuthFailure> {
        self.authenticator.authenticate(bearer_header, link_token)
    }

    fn allows_ip(&self, addr: IpAddr) -> bool {
        self.ip_allowlist.allows(addr)
    }

    fn mode(&self) -> AuthMode {
        self.authenticator.mode()
    }
}

async fn handle_health() -> impl IntoResponse {
    #[derive(Serialize)]
    struct HealthResponse<'a> {
        status: &'a str,
        uptime: u64,
    }

    let body = HealthResponse {
        status: "ok",
        uptime: current_unix_timestamp(),
    };

    (StatusCode::OK, axum::Json(body))
}

async fn handle_rpc<H>(
    State(state): State<HttpServerState<H>>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse
where
    H: MessageHandler,
{
    let bearer = extract_authorization(&headers);
    process_rpc::<H>(state, client_addr, bearer, None, body).await
}

async fn handle_rpc_with_token<H>(
    Path(token): Path<String>,
    State(state): State<HttpServerState<H>>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse
where
    H: MessageHandler,
{
    let bearer = extract_authorization(&headers);
    process_rpc::<H>(state, client_addr, bearer, Some(token), body).await
}

async fn process_rpc<H>(
    state: HttpServerState<H>,
    client_addr: SocketAddr,
    bearer_header: Option<String>,
    token_path: Option<String>,
    body: Bytes,
) -> HttpResponse
where
    H: MessageHandler,
{
    trace!(remote = %client_addr, ?token_path, "received HTTP RPC request");

    if !state.allows_ip(client_addr.ip()) {
        warn!(remote = %client_addr, "rejecting request from disallowed IP address");
        return HttpResponse::auth_error(StatusCode::FORBIDDEN, "client address not allowed");
    }

    let auth_result = state.authenticate(bearer_header.as_deref(), token_path.as_deref());
    match auth_result {
        Ok(source) => {
            trace!(remote = %client_addr, ?source, "client authenticated");
        }
        Err(failure) => {
            warn!(remote = %client_addr, failure = ?failure, "authentication failed");
            return HttpResponse::from_auth_failure(failure, state.mode());
        }
    }

    let permit = match state.try_acquire() {
        Ok(permit) => permit,
        Err(TryAcquireError::NoPermits) => {
            warn!("rejecting request due to saturated concurrency limit");
            let error = ProtocolError::custom(
                ErrorCode::RateLimited,
                "too many concurrent requests. Retry shortly.",
                None,
            )
            .into_rpc();
            let payload = RpcResponse::error(Id::Null, error);
            return HttpResponse::json(StatusCode::TOO_MANY_REQUESTS, payload);
        }
        Err(TryAcquireError::Closed) => {
            error!("request concurrency limiter is closed");
            return HttpResponse::status(StatusCode::SERVICE_UNAVAILABLE);
        }
    };

    let span = tracing::debug_span!("http_rpc");
    let response = async move {
        match std::str::from_utf8(&body) {
            Ok(raw) => match Incoming::parse_str(raw) {
                Ok(incoming) => dispatch_incoming(state.handler(), incoming).await,
                Err(error) => {
                    warn!(error = %error, "failed to parse JSON-RPC payload");
                    let status = match error {
                        ProtocolError::ParseError { .. } => StatusCode::BAD_REQUEST,
                        _ => StatusCode::OK,
                    };
                    let rpc_error = error.into_rpc();
                    let payload = RpcResponse::error(Id::Null, rpc_error);
                    Ok(HttpResponse::json(status, payload))
                }
            },
            Err(error) => {
                warn!(%error, "failed to decode request body as UTF-8");
                let rpc_error =
                    ProtocolError::parse_error("request body must be valid UTF-8").into_rpc();
                let payload = RpcResponse::error(Id::Null, rpc_error);
                Ok(HttpResponse::json(StatusCode::BAD_REQUEST, payload))
            }
        }
    }
    .instrument(span)
    .await;

    drop(permit);

    match response {
        Ok(response) => response,
        Err(error) => {
            error!(error = %error, "failed to process JSON-RPC request");
            HttpResponse::status(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

fn extract_authorization(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

async fn dispatch_incoming<H>(
    handler: Arc<H>,
    incoming: Incoming,
) -> Result<HttpResponse, anyhow::Error>
where
    H: MessageHandler,
{
    match incoming {
        Incoming::Single(message) => {
            let mut responses = Vec::with_capacity(1);
            if let Some(response) = dispatch_message(handler, message).await {
                responses.push(response);
            }
            Ok(build_response(responses)?)
        }
        Incoming::Batch(messages) => {
            if messages.is_empty() {
                let rpc_error = ProtocolError::invalid_request(
                    "batch must contain at least one JSON-RPC message",
                )
                .into_rpc();
                let payload = RpcResponse::error(Id::Null, rpc_error);
                return Ok(HttpResponse::json(StatusCode::OK, vec![payload]));
            }

            let mut responses = Vec::new();
            for message in messages {
                if let Some(response) = dispatch_message(Arc::clone(&handler), message).await {
                    responses.push(response);
                }
            }
            Ok(build_response(responses)?)
        }
    }
}

async fn dispatch_message<H>(handler: Arc<H>, message: Message) -> Option<RpcResponse>
where
    H: MessageHandler,
{
    match message {
        Message::Request(request) => {
            trace!(method = %request.method, "handling JSON-RPC request");
            let method = request.method.clone();
            let id = request.id.clone();
            match handler.handle_request(request).await {
                Ok(result) => Some(RpcResponse::success(id.clone(), result)),
                Err(error) => {
                    debug!(%method, request_id = %id, err = %error, "request handler returned protocol error");
                    Some(RpcResponse::error(id, error.into_rpc()))
                }
            }
        }
        Message::Notification(notification) => {
            trace!(method = %notification.method, "handling JSON-RPC notification");
            if let Err(error) = handler.handle_notification(notification).await {
                warn!(error = %error, "notification handler returned error");
            }
            None
        }
        Message::Response(_) => {
            warn!("received unexpected JSON-RPC response message from client");
            let rpc_error = ProtocolError::invalid_request(
                "received response message; servers only accept requests or notifications",
            )
            .into_rpc();
            Some(RpcResponse::error(Id::Null, rpc_error))
        }
    }
}

fn build_response(payloads: Vec<RpcResponse>) -> Result<HttpResponse, anyhow::Error> {
    if payloads.is_empty() {
        return Ok(HttpResponse::status(StatusCode::NO_CONTENT));
    }

    if payloads.len() == 1 {
        let value = serde_json::to_value(&payloads[0]).context("failed to encode JSON response")?;
        return Ok(HttpResponse::json(StatusCode::OK, value));
    }

    let mut values = Vec::with_capacity(payloads.len());
    for payload in payloads {
        let value = serde_json::to_value(&payload).context("failed to encode JSON response")?;
        values.push(value);
    }

    Ok(HttpResponse::json(StatusCode::OK, Value::Array(values)))
}

#[derive(Debug)]
struct HttpResponse {
    status: StatusCode,
    body: Option<Value>,
    headers: HeaderMap,
}

impl HttpResponse {
    fn status(status: StatusCode) -> Self {
        Self {
            status,
            body: None,
            headers: HeaderMap::new(),
        }
    }

    fn json<T>(status: StatusCode, payload: T) -> Self
    where
        T: Serialize,
    {
        let value = match serde_json::to_value(payload) {
            Ok(value) => value,
            Err(error) => {
                error!(%error, "failed to serialise JSON response payload");
                return Self::status(StatusCode::INTERNAL_SERVER_ERROR);
            }
        };

        Self {
            status,
            body: Some(value),
            headers: HeaderMap::new(),
        }
    }

    fn with_header(mut self, name: header::HeaderName, value: HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }

    fn auth_error(status: StatusCode, message: &str) -> Self {
        let rpc_error = ProtocolError::custom(ErrorCode::ServerError, message, None).into_rpc();
        let payload = RpcResponse::error(Id::Null, rpc_error);
        Self::json(status, payload)
    }

    fn from_auth_failure(failure: AuthFailure, mode: AuthMode) -> Self {
        match failure {
            AuthFailure::MissingToken => {
                let response =
                    Self::auth_error(StatusCode::UNAUTHORIZED, "authentication required");
                response.with_header(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"))
            }
            AuthFailure::InvalidToken => {
                Self::auth_error(StatusCode::FORBIDDEN, "authentication token rejected")
            }
            AuthFailure::NotConfigured => {
                let mut response = Self::auth_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server authentication has not been configured correctly",
                );
                if mode.accepts_link_tokens() {
                    response = response
                        .with_header(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                }
                response
            }
        }
    }
}

impl IntoResponse for HttpResponse {
    fn into_response(self) -> Response {
        let mut response = match self.body {
            Some(body) => (self.status, axum::Json(body)).into_response(),
            None => self.status.into_response(),
        };

        for (name, value) in self.headers.iter() {
            response.headers_mut().insert(name.clone(), value.clone());
        }

        response
    }
}

fn current_unix_timestamp() -> u64 {
    // Placeholder for a more meaningful uptime/health metric. Tracks seconds since process start.
    static START: once_cell::sync::Lazy<std::time::Instant> =
        once_cell::sync::Lazy::new(std::time::Instant::now);
    START.elapsed().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{self, Body},
        extract::connect_info::MockConnectInfo,
        http::{Request, StatusCode},
    };
    use serde_json::json;
    use tokio::time::{Duration, sleep};
    use tower::ServiceExt;

    use crate::{
        auth::{AuthConfig, AuthMode, IpAllowList, TokenStoreBuilder},
        protocol::Request as RpcRequest,
        transport::MessageHandler,
    };

    const TEST_TOKEN: &str = "test-token";
    const ALT_TOKEN: &str = "alt-token";

    #[derive(Debug)]
    struct EchoHandler;

    #[async_trait::async_trait]
    impl MessageHandler for EchoHandler {
        async fn handle_request(
            &self,
            request: RpcRequest,
        ) -> std::result::Result<Value, ProtocolError> {
            Ok(json!({
                "method": request.method,
                "params": request.params,
            }))
        }
    }

    fn build_auth_config(mode: AuthMode) -> AuthConfig {
        let mut builder = TokenStoreBuilder::new();
        builder.add_raw_token(TEST_TOKEN).unwrap();
        if mode.accepts_link_tokens() {
            builder.add_raw_token(ALT_TOKEN).unwrap();
        }
        AuthConfig::new(mode, builder.build())
    }

    fn build_server<H>(
        handler: Arc<H>,
        auth: AuthConfig,
        ip_allowlist: IpAllowList,
    ) -> HttpServer<H>
    where
        H: MessageHandler,
    {
        let config = HttpServerConfig {
            auth,
            ip_allowlist,
            ..HttpServerConfig::default()
        };
        HttpServer::new(handler, config).expect("build http server")
    }

    fn router_with_client<H>(server: HttpServer<H>, addr: SocketAddr) -> Router
    where
        H: MessageHandler,
    {
        server.router().layer(MockConnectInfo(addr))
    }

    fn bearer_header() -> String {
        format!("Bearer {TEST_TOKEN}")
    }

    #[tokio::test]
    async fn handles_single_request() {
        let handler = Arc::new(EchoHandler);
        let auth = build_auth_config(AuthMode::Bearer);
        let server = build_server(handler, auth, IpAllowList::default());
        let app = router_with_client(server, SocketAddr::from(([127, 0, 0, 1], 4000)));

        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "mcp.echo",
            "params": {"message": "hi"},
        });

        let request = Request::builder()
            .method("POST")
            .uri("/rpc")
            .header("content-type", "application/json")
            .header("authorization", bearer_header())
            .body(Body::from(body.to_string()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "method": "mcp.echo",
                    "params": {"message": "hi"}
                }
            })
        );
    }

    #[tokio::test]
    async fn returns_no_content_for_notifications() {
        let handler = Arc::new(EchoHandler);
        let auth = build_auth_config(AuthMode::Bearer);
        let server = build_server(handler, auth, IpAllowList::default());
        let app = router_with_client(server, SocketAddr::from(([127, 0, 0, 1], 4001)));

        let body = json!({
            "jsonrpc": "2.0",
            "method": "mcp.notify",
            "params": {"note_id": "n1"},
        });

        let request = Request::builder()
            .method("POST")
            .uri("/rpc")
            .header("content-type", "application/json")
            .header("authorization", bearer_header())
            .body(Body::from(body.to_string()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn rejects_malformed_json() {
        let handler = Arc::new(EchoHandler);
        let auth = build_auth_config(AuthMode::Bearer);
        let server = build_server(handler, auth, IpAllowList::default());
        let app = router_with_client(server, SocketAddr::from(([127, 0, 0, 1], 4002)));

        let request = Request::builder()
            .method("POST")
            .uri("/rpc")
            .header("content-type", "application/json")
            .header("authorization", bearer_header())
            .body(Body::from("{\"jsonrpc\": \"2.0\", \"id\": 1,"))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body = body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["code"], ErrorCode::ParseError.code());
    }

    #[derive(Debug)]
    struct SlowHandler;

    #[async_trait::async_trait]
    impl MessageHandler for SlowHandler {
        async fn handle_request(
            &self,
            _request: RpcRequest,
        ) -> std::result::Result<Value, ProtocolError> {
            sleep(Duration::from_millis(100)).await;
            Ok(json!({"ok": true}))
        }
    }

    #[tokio::test]
    async fn enforces_concurrency_limit() {
        let handler = Arc::new(SlowHandler);
        let mut builder = TokenStoreBuilder::new();
        builder.add_raw_token(TEST_TOKEN).unwrap();
        let config = HttpServerConfig {
            max_concurrency: 1,
            auth: AuthConfig::new(AuthMode::Bearer, builder.build()),
            ..HttpServerConfig::default()
        };
        let server = HttpServer::new(handler, config).expect("http server");
        let app = router_with_client(server, SocketAddr::from(([127, 0, 0, 1], 4003)));

        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "mcp.echo"
        })
        .to_string();

        let request1 = Request::builder()
            .method("POST")
            .uri("/rpc")
            .header("content-type", "application/json")
            .header("authorization", bearer_header())
            .body(Body::from(body.clone()))
            .unwrap();

        let request2 = Request::builder()
            .method("POST")
            .uri("/rpc")
            .header("content-type", "application/json")
            .header("authorization", bearer_header())
            .body(Body::from(body))
            .unwrap();

        let app_clone = app.clone();
        let handle = tokio::spawn(async move { app_clone.oneshot(request1).await.unwrap() });

        // Ensure the first request acquires the semaphore permit.
        sleep(Duration::from_millis(10)).await;

        let response2 = app.oneshot(request2).await.unwrap();
        assert_eq!(response2.status(), StatusCode::TOO_MANY_REQUESTS);

        let response1 = handle.await.unwrap();
        assert_eq!(response1.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn rejects_missing_authentication() {
        let handler = Arc::new(EchoHandler);
        let auth = build_auth_config(AuthMode::Bearer);
        let server = build_server(handler, auth, IpAllowList::default());
        let app = router_with_client(server, SocketAddr::from(([127, 0, 0, 1], 4004)));

        let body = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "mcp.echo",
            "params": {},
        });

        let request = Request::builder()
            .method("POST")
            .uri("/rpc")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn accepts_link_token_path() {
        let handler = Arc::new(EchoHandler);
        let auth = build_auth_config(AuthMode::LinkToken);
        let server = build_server(handler, auth, IpAllowList::default());
        let app = router_with_client(server, SocketAddr::from(([127, 0, 0, 1], 4005)));

        let body = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "mcp.echo",
            "params": {},
        });

        let request = Request::builder()
            .method("POST")
            .uri(format!("/rpc/{ALT_TOKEN}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn rejects_disallowed_ip() {
        let handler = Arc::new(EchoHandler);
        let auth = build_auth_config(AuthMode::Bearer);
        // Allow only IPv6 loopback to force rejection of IPv4 address below.
        let ip_allowlist = IpAllowList::from_strings(["::1/128"]).unwrap();
        let server = build_server(handler, auth, ip_allowlist);
        let app = router_with_client(server, SocketAddr::from(([10, 0, 0, 5], 4006)));

        let body = json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "mcp.echo",
            "params": {},
        });

        let request = Request::builder()
            .method("POST")
            .uri("/rpc")
            .header("content-type", "application/json")
            .header("authorization", bearer_header())
            .body(Body::from(body.to_string()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
