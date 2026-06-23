//! `arrowhead --mcp` implementation.

use std::{
    env, fs,
    io::{self, BufRead},
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use rand::{RngCore, rngs::OsRng};
use tokio::signal;
use tracing::{info, warn};

use arrowhead_mcp::{
    auth::{
        AuthConfig, AuthMode, IpAllowList, NetworkError, TokenDigest, TokenError, TokenSources,
    },
    handlers::HandlerRegistry,
    http::{DEFAULT_BIND_ADDRESS, HttpServer, HttpServerConfig},
    runtime::{McpRuntime, RuntimeOptions},
    stdio::StdioServer,
};

use super::CommandContext;
use crate::logging::{self, scoped_named_file_logging};

const MCP_HTTP_LOG: &str = "mcp-http.log";
const ENV_BIND: &str = "ARROWHEAD_MCP_BIND";
const ENV_TOKEN: &str = "ARROWHEAD_MCP_TOKEN";

/// Command-line flags accepted while running `--mcp-server`.
#[derive(Debug, Args, Default, Clone)]
pub struct McpServerCliArgs {
    /// Socket address to bind the HTTP server to (e.g. 127.0.0.1:3911).
    #[arg(long, value_name = "ADDR", requires = "mcp_server")]
    pub bind: Option<String>,
    /// Additional CIDR ranges granted access (may be repeated).
    #[arg(long, value_name = "CIDR", requires = "mcp_server")]
    pub allow: Vec<String>,
    /// File containing CIDR ranges (one per line) to allow.
    #[arg(long, value_name = "PATH", requires = "mcp_server")]
    pub allow_file: Vec<PathBuf>,
    /// Authentication mode (`bearer` or `link-token`).
    #[arg(long, value_name = "MODE", requires = "mcp_server")]
    pub auth_mode: Option<String>,
    /// Raw authentication token to accept (may be repeated).
    #[arg(long, value_name = "TOKEN", requires = "mcp_server")]
    pub token: Vec<String>,
    /// File containing raw authentication tokens (one per line).
    #[arg(long, value_name = "PATH", requires = "mcp_server")]
    pub token_file: Vec<PathBuf>,
    /// Pre-hashed token digests (hex-encoded SHA-256).
    #[arg(long, value_name = "HEX", requires = "mcp_server")]
    pub token_hash: Vec<String>,
    /// Only generate and persist a new token, then exit.
    #[arg(long, requires = "mcp_server")]
    pub generate_token: bool,
    /// Override the maximum concurrent in-flight requests.
    #[arg(long, value_name = "N", requires = "mcp_server")]
    pub max_concurrency: Option<usize>,
    /// Override the maximum accepted request body size in bytes.
    #[arg(long, value_name = "BYTES", requires = "mcp_server")]
    pub max_body_bytes: Option<usize>,
}

/// Run the MCP stdio server until EOF is observed on stdin.
pub async fn run_stdio(ctx: &mut CommandContext) -> Result<()> {
    let vault_path = vault_path(ctx)?;
    let runtime =
        Arc::new(McpRuntime::initialise(build_runtime_options(ctx, vault_path.clone())).await?);

    let logs_dir = runtime.vault().paths().logs_dir();
    let _logging_guard = logging::scoped_file_logging(&logs_dir, ctx.verbosity())?;

    info!(
        vault = %vault_path.display(),
        socket = %runtime.daemon().socket_path().display(),
        "starting MCP stdio server"
    );

    let handler = Arc::new(HandlerRegistry::new(Arc::clone(&runtime)));
    let server = StdioServer::new(handler);
    let metrics = server.metrics();

    server.run().await?;

    let snapshot = metrics.snapshot();
    info!(
        accepted = snapshot.accepted_requests,
        completed = snapshot.completed_requests,
        failed = snapshot.failed_requests,
        rejected = snapshot.rejected_requests,
        notifications = snapshot.notifications_received,
        notification_failures = snapshot.notifications_failed,
        parse_errors = snapshot.parse_errors,
        "MCP stdio server terminated"
    );

    Ok(())
}

/// Run the MCP HTTP server until interrupted.
pub async fn run_server(ctx: &mut CommandContext, cli: &McpServerCliArgs) -> Result<()> {
    if cli.generate_token {
        generate_and_store_token(ctx, cli)?;
        return Ok(());
    }

    let setup = prepare_http_server(ctx, cli)?;
    let vault_path = vault_path(ctx)?;
    let runtime =
        Arc::new(McpRuntime::initialise(build_runtime_options(ctx, vault_path.clone())).await?);

    let logs_dir = runtime.vault().paths().logs_dir();
    let _logging_guard = scoped_named_file_logging(&logs_dir, ctx.verbosity(), MCP_HTTP_LOG)?;

    let handler = Arc::new(HandlerRegistry::new(Arc::clone(&runtime)));
    let server = HttpServer::new(handler, setup.http_config)?;

    let base_url = format!("http://{}", setup.bind);
    info!(
        vault = %vault_path.display(),
        bind = %setup.bind,
        auth_mode = %setup.auth_mode,
        "starting MCP HTTP server"
    );

    println!(
        "MCP HTTP server listening on {base_url}/rpc (auth mode: {})",
        setup.auth_mode
    );
    if setup.auth_mode.accepts_link_tokens() {
        if let Some(token) = setup.display_tokens.first() {
            println!("Link token URL: {base_url}/rpc/{token}");
        } else {
            println!(
                "link-token mode active; generate a token with `arrowhead --mcp-server --generate-token` to obtain a shareable URL"
            );
        }
    }
    println!(
        "Reminder: run mcp.discovery.get_vault_conventions before creating, updating, or deleting notes."
    );

    let shutdown = async {
        if let Err(err) = signal::ctrl_c().await {
            warn!(error = %err, "failed to await shutdown signal");
        }
    };

    server.serve(shutdown).await?;
    info!("MCP HTTP server stopped");

    Ok(())
}

#[derive(Debug)]
struct ServerSetup {
    http_config: HttpServerConfig,
    bind: SocketAddr,
    auth_mode: AuthMode,
    display_tokens: Vec<String>,
}

fn prepare_http_server(ctx: &CommandContext, cli: &McpServerCliArgs) -> Result<ServerSetup> {
    let bind = resolve_bind_address(ctx, cli)?;
    let auth_mode = resolve_auth_mode(ctx, cli)?;
    let allow_list = build_allow_list(ctx, cli)?;
    let (token_store, display_tokens) = collect_tokens(ctx, cli)?;

    let mut http_config = HttpServerConfig {
        bind_address: bind,
        auth: AuthConfig::new(auth_mode, token_store),
        ip_allowlist: allow_list,
        ..HttpServerConfig::default()
    };
    if let Some(limit) = cli.max_concurrency.or(ctx.config.mcp.max_concurrency) {
        http_config.max_concurrency = limit;
    }
    if let Some(limit) = cli.max_body_bytes.or(ctx.config.mcp.max_body_bytes) {
        http_config.max_body_bytes = limit;
    }

    Ok(ServerSetup {
        http_config,
        bind,
        auth_mode,
        display_tokens,
    })
}

fn resolve_bind_address(ctx: &CommandContext, cli: &McpServerCliArgs) -> Result<SocketAddr> {
    if let Some(value) = cli.bind.as_deref() {
        return parse_socket_addr(value);
    }

    if let Ok(value) = env::var(ENV_BIND) {
        if !value.trim().is_empty() {
            return parse_socket_addr(value.trim());
        }
    }

    if let Some(value) = ctx.config.mcp.bind_address.as_deref() {
        return parse_socket_addr(value);
    }

    Ok(DEFAULT_BIND_ADDRESS)
}

fn parse_socket_addr(value: &str) -> Result<SocketAddr> {
    value
        .parse()
        .map_err(|err| anyhow!("invalid bind address '{value}': {err}"))
}

fn resolve_auth_mode(ctx: &CommandContext, cli: &McpServerCliArgs) -> Result<AuthMode> {
    if let Some(mode) = cli.auth_mode.as_deref() {
        return AuthMode::from_str(mode).map_err(|err| anyhow!(err));
    }

    Ok(ctx.config.mcp.auth_mode)
}

fn build_allow_list(ctx: &CommandContext, cli: &McpServerCliArgs) -> Result<IpAllowList> {
    let mut entries: Vec<String> = vec!["127.0.0.0/8".to_string(), "::1/128".to_string()];
    entries.extend(ctx.config.mcp.allowed_ips.clone());
    entries.extend(cli.allow.clone());
    for path in &cli.allow_file {
        entries.extend(read_lines_trimmed(path)?);
    }

    match IpAllowList::from_strings(entries) {
        Ok(list) => Ok(list),
        Err(NetworkError::InvalidCidr { input, source }) => {
            Err(anyhow!("invalid CIDR entry '{input}': {source}"))
        }
    }
}

fn collect_tokens(
    ctx: &CommandContext,
    cli: &McpServerCliArgs,
) -> Result<(arrowhead_mcp::auth::TokenStore, Vec<String>)> {
    let mut sources = TokenSources::new();
    let mut display = Vec::new();

    for digest in &ctx.config.mcp.tokens {
        sources.add_hashed_token(*digest);
    }

    for hash in &cli.token_hash {
        sources
            .add_hashed_token_hex(hash)
            .map_err(|err| match err {
                TokenError::InvalidDigest { input, source } => {
                    anyhow!("invalid token hash '{input}': {source}")
                }
                other => anyhow!(other),
            })?;
    }

    for token in &cli.token {
        let trimmed = token.trim();
        if trimmed.is_empty() {
            continue;
        }
        sources.add_raw_token(trimmed.to_owned());
        display.push(trimmed.to_owned());
    }

    for path in &cli.token_file {
        for token in read_lines_trimmed(path)? {
            if token.is_empty() {
                continue;
            }
            sources.add_raw_token(token.clone());
            display.push(token);
        }
    }

    if let Ok(value) = env::var(ENV_TOKEN) {
        for token in value.split([',', ' ']) {
            let trimmed = token.trim();
            if trimmed.is_empty() {
                continue;
            }
            sources.add_raw_token(trimmed.to_owned());
            display.push(trimmed.to_owned());
        }
    }

    match sources.load() {
        Ok(store) => Ok((store, display)),
        Err(TokenError::EmptyStore) => bail!(
            "no authentication tokens configured. Provide --token, --token-hash, or stored hashes in the config, or run with --generate-token"
        ),
        Err(error) => Err(error.into()),
    }
}

fn read_lines_trimmed(path: &Path) -> Result<Vec<String>> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to read token file {}", path.display()))?;
    let reader = io::BufReader::new(file);
    Ok(reader
        .lines()
        .map_while(|line| line.ok())
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect())
}

fn generate_and_store_token(ctx: &mut CommandContext, cli: &McpServerCliArgs) -> Result<()> {
    let auth_mode = resolve_auth_mode(ctx, cli)?;
    let bind = resolve_bind_address(ctx, cli)?;
    let token = generate_token_value();
    let digest = TokenDigest::hash(&token);

    if !ctx
        .config
        .mcp
        .tokens
        .iter()
        .any(|existing| existing == &digest)
    {
        ctx.config.mcp.tokens.push(digest);
    }

    ctx.persist()?;

    print_generated_token(&token, digest, bind, auth_mode);
    Ok(())
}

fn generate_token_value() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn print_generated_token(token: &str, digest: TokenDigest, bind: SocketAddr, mode: AuthMode) {
    let base = format!("http://{bind}");
    println!("Generated MCP token\n");
    println!("  Token: {token}");
    println!("  SHA-256: {}", digest.to_hex());
    println!("\nThe digest has been stored in your configuration file.");
    println!("Authorization header example: curl -H \"Authorization: Bearer {token}\" {base}/rpc");
    if mode.accepts_link_tokens() {
        println!("Link token URL: {base}/rpc/{token}");
    }
}

fn vault_path(ctx: &CommandContext) -> Result<PathBuf> {
    ctx.config
        .vault
        .clone()
        .context("no vault configured. Provide --vault or run `arrowhead init`.")
}

fn build_runtime_options(ctx: &CommandContext, vault_path: PathBuf) -> RuntimeOptions {
    RuntimeOptions::new(vault_path)
        .with_embedding_model(ctx.config.embedding_model.clone())
        .with_daemon_socket(ctx.config.daemon.socket_path.clone())
        .with_daemon_status(ctx.config.daemon.status_path.clone())
}
