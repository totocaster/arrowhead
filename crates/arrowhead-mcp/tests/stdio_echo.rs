use std::sync::Arc;

use arrowhead_mcp::stdio::{MessageHandler, StdioServer};
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    time::{Duration, sleep},
};

struct EchoHandler;

#[async_trait::async_trait]
impl MessageHandler for EchoHandler {
    async fn handle_request(
        &self,
        request: arrowhead_mcp::protocol::Request,
    ) -> Result<serde_json::Value, arrowhead_mcp::protocol::ProtocolError> {
        Ok(request.params.raw().clone())
    }
}

#[tokio::test]
async fn stdio_server_echoes_request() {
    let handler = Arc::new(EchoHandler);
    let server = StdioServer::new(handler);
    let (server_stream, client_stream) = tokio::io::duplex(1024);
    let (server_reader, server_writer) = tokio::io::split(server_stream);
    let (client_reader, mut client_writer) = tokio::io::split(client_stream);

    let server_handle = tokio::spawn(async move {
        server
            .run_with_io(server_reader, server_writer)
            .await
            .expect("server should exit cleanly");
    });

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "mcp.echo",
        "params": {
            "message": "hello from client"
        }
    })
    .to_string();

    client_writer
        .write_all(request.as_bytes())
        .await
        .expect("write request");
    client_writer.write_all(b"\n").await.expect("write newline");
    client_writer.flush().await.expect("flush writer");
    drop(client_writer);

    let mut reader = BufReader::new(client_reader);
    let mut line = String::new();
    let read = reader.read_line(&mut line).await.expect("read response");
    assert!(read > 0, "response should be present");

    let value: serde_json::Value = serde_json::from_str(line.trim()).expect("valid JSON response");
    assert_eq!(
        value,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "message": "hello from client"
            }
        })
    );

    // Allow the server loop to observe EOF.
    sleep(Duration::from_millis(10)).await;
    drop(reader);
    server_handle.await.expect("join server task");
}
