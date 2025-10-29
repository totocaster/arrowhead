use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use arrowhead_mcp::{
    handlers::HandlerRegistry,
    runtime::{McpRuntime, RuntimeOptions},
    stdio::StdioServer,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
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

#[tokio::test]
async fn stdio_server_handles_initialize_and_tools_list() {
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
    let server = StdioServer::new(Arc::clone(&handler));

    let (server_stream, client_stream) = tokio::io::duplex(4096);
    let (server_reader, server_writer) = tokio::io::split(server_stream);
    let (client_reader, mut client_writer) = tokio::io::split(client_stream);
    let mut client_reader = BufReader::new(client_reader);

    let server_task = tokio::spawn(async move {
        server
            .run_with_io(server_reader, server_writer)
            .await
            .expect("stdio server should exit cleanly");
    });

    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "integration-test", "version": "1.0.0" }
        }
    })
    .to_string();
    client_writer
        .write_all(init_request.as_bytes())
        .await
        .expect("write initialize");
    client_writer.write_all(b"\n").await.expect("newline");
    client_writer.flush().await.expect("flush initialize");

    let mut line = String::new();
    let read = client_reader
        .read_line(&mut line)
        .await
        .expect("read initialize response");
    assert!(read > 0, "initialize response should be present");

    let response: Value =
        serde_json::from_str(line.trim()).expect("parse initialize response as json");
    assert_eq!(response.get("id").and_then(Value::as_i64), Some(1));
    let init_result = response
        .get("result")
        .and_then(Value::as_object)
        .cloned()
        .expect("initialize result object");
    assert_eq!(
        init_result.get("protocolVersion").and_then(Value::as_str),
        Some("2025-06-18")
    );
    assert!(
        init_result.get("serverInfo").is_some(),
        "initialize should return server info"
    );
    assert!(
        init_result.get("capabilities").is_some(),
        "initialize should return capabilities"
    );

    let tools_request = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    })
    .to_string();
    client_writer
        .write_all(tools_request.as_bytes())
        .await
        .expect("write tools/list");
    client_writer.write_all(b"\n").await.expect("newline");
    client_writer.flush().await.expect("flush tools/list");

    line.clear();
    let read = client_reader
        .read_line(&mut line)
        .await
        .expect("read tools/list response");
    assert!(read > 0, "tools/list response should be present");

    let tools_response: Value =
        serde_json::from_str(line.trim()).expect("parse tools/list response");
    assert_eq!(tools_response.get("id").and_then(Value::as_i64), Some(2));
    let tools_result = tools_response
        .get("result")
        .and_then(Value::as_object)
        .cloned()
        .expect("tools/list result object");
    let tools = tools_result
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array present");
    assert!(
        tools
            .iter()
            .any(|tool| { tool.get("name").and_then(Value::as_str) == Some("notes_read") }),
        "tool catalogue should include notes_read"
    );

    drop(client_writer);
    drop(client_reader);

    server_task.await.expect("join stdio server task");
}
