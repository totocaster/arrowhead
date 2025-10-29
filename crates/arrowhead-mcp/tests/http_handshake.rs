use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Result as AnyResult;
use arrowhead_mcp::{
    auth::{AuthConfig, AuthMode, TokenStoreBuilder},
    handlers::HandlerRegistry,
    http::{HttpServer, HttpServerConfig},
    runtime::{McpRuntime, RuntimeOptions},
};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::{io, sync::oneshot, task::JoinHandle, time::sleep};

const TEST_TOKEN: &str = "test-token";

fn fixture_vault_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("test-vault")
}

fn copy_fixture() -> tempfile::TempDir {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    copy_dir_recursive(&fixture_vault_dir(), temp_dir.path()).expect("copy fixture vault");
    temp_dir
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else {
            fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

async fn start_http_server(
    handler: Arc<HandlerRegistry>,
) -> (SocketAddr, oneshot::Sender<()>, JoinHandle<AnyResult<()>>) {
    let mut token_builder = TokenStoreBuilder::new();
    token_builder
        .add_raw_token(TEST_TOKEN)
        .expect("add bearer token");
    let token_store = token_builder.build();

    let bind_port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("bind probe listener")
        .local_addr()
        .expect("read probe addr")
        .port();
    let bind_address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), bind_port);

    let server = HttpServer::new(
        handler,
        HttpServerConfig {
            bind_address,
            auth: AuthConfig::new(AuthMode::Bearer, token_store),
            ..HttpServerConfig::default()
        },
    )
    .expect("build http server");

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let shutdown_future = async move {
        let _ = shutdown_rx.await;
    };

    let server_handle = tokio::spawn(async move { server.serve(shutdown_future).await });

    (bind_address, shutdown_tx, server_handle)
}

async fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: Value,
) -> reqwest::Result<reqwest::Response> {
    client
        .post(url)
        .bearer_auth(TEST_TOKEN)
        .json(&body)
        .send()
        .await
}

#[tokio::test]
async fn http_server_handles_initialize_and_tools_list() {
    let temp_dir = copy_fixture();
    let runtime = McpRuntime::initialise(
        RuntimeOptions::new(temp_dir.path().to_path_buf())
            .with_embedding_model(None)
            .with_daemon_socket(Some(temp_dir.path().join("control.sock")))
            .with_daemon_status(Some(temp_dir.path().join("status.json"))),
    )
    .await
    .expect("runtime initialises");

    let handler = Arc::new(HandlerRegistry::new(Arc::new(runtime)));
    let (bind_address, shutdown_tx, server_handle) = start_http_server(handler).await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("build reqwest client");

    let base_url = format!("http://{bind_address}/rpc");

    let init_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "integration-test", "version": "1.0.0" }
        }
    });

    let init_response = loop {
        match post_json(&client, &base_url, init_body.clone()).await {
            Ok(response) => break response,
            Err(error) if error.is_connect() => {
                sleep(Duration::from_millis(25)).await;
                continue;
            }
            Err(error) => panic!("initialize request failed: {error:?}"),
        }
    };
    assert_eq!(init_response.status(), StatusCode::OK);
    let init_payload: Value = init_response.json().await.expect("parse init response");
    assert_eq!(init_payload.get("id").and_then(Value::as_i64), Some(1));
    let init_result = init_payload
        .get("result")
        .and_then(Value::as_object)
        .cloned()
        .expect("initialize result object");
    assert_eq!(
        init_result.get("protocolVersion").and_then(Value::as_str),
        Some("2025-06-18")
    );
    assert!(init_result.get("serverInfo").is_some());
    assert!(init_result.get("capabilities").is_some());

    let tools_body = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    let tools_response = post_json(&client, &base_url, tools_body)
        .await
        .expect("tools/list response");
    assert_eq!(tools_response.status(), StatusCode::OK);
    let tools_payload: Value = tools_response.json().await.expect("parse tools response");
    assert_eq!(tools_payload.get("id").and_then(Value::as_i64), Some(2));
    let tools_result = tools_payload
        .get("result")
        .and_then(Value::as_object)
        .cloned()
        .expect("tools result object");
    let tools = tools_result
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array present");
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some("notes_read")),
        "tool catalogue should include notes_read"
    );

    shutdown_tx.send(()).expect("signal shutdown");
    server_handle
        .await
        .expect("join server task")
        .expect("server terminated cleanly");
}
