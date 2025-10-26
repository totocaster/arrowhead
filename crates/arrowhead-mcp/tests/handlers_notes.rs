use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use arrowhead_mcp::{
    handlers::HandlerRegistry,
    protocol::{Params, Request},
    runtime::{McpRuntime, RuntimeOptions},
    stdio::MessageHandler,
};
use serde_json::{Value, json};
use tempfile::TempDir;

fn fixture_vault_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("test-vault")
}

fn copy_fixture() -> TempDir {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let target = temp_dir.path();
    let source = fixture_vault_dir();
    copy_dir_recursive(&source, target).expect("copy fixture vault");
    temp_dir
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

async fn build_handler(temp_dir: &TempDir) -> HandlerRegistry {
    let vault_path = temp_dir.path().to_path_buf();
    let runtime = McpRuntime::initialise(
        RuntimeOptions::new(vault_path)
            .with_embedding_model(None)
            .with_daemon_socket(Some(temp_dir.path().join("control.sock")))
            .with_daemon_status(Some(temp_dir.path().join("status.json"))),
    )
    .await
    .expect("runtime initialises");

    HandlerRegistry::new(Arc::new(runtime))
}

#[tokio::test]
async fn notes_list_ids_only_returns_results() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let params = Params::new(json!({ "idsOnly": true })).expect("params");
    let request = Request::new(1, "mcp.notes.list", params);
    let value = handler
        .handle_request(request)
        .await
        .expect("list request succeeds");

    let notes = value
        .get("notes")
        .and_then(Value::as_array)
        .expect("notes array present");
    assert!(!notes.is_empty(), "notes array should not be empty");
    assert!(
        notes.iter().all(|item| item.get("noteId").is_some()),
        "each entry contains noteId"
    );
}

#[tokio::test]
async fn notes_read_returns_expected_fields() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let params = Params::new(json!({ "noteId": "Photography Equipment" })).expect("params");
    let request = Request::new(2, "mcp.notes.read", params);
    let value = handler
        .handle_request(request)
        .await
        .expect("read request succeeds");

    assert_eq!(
        value.get("noteId").and_then(Value::as_str),
        Some("Photography Equipment")
    );
    assert!(value.get("content").and_then(Value::as_str).is_some());
    assert!(value.get("metadata").is_some());
}

#[tokio::test]
async fn graph_find_orphans_reports_notes() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let request = Request::new(3, "mcp.graph.find_orphans", Params::default());
    let value = handler
        .handle_request(request)
        .await
        .expect("graph orphan request succeeds");

    let total = value.get("total").and_then(Value::as_u64).unwrap_or(0);
    let notes = value
        .get("notes")
        .and_then(Value::as_array)
        .expect("notes array present");
    assert_eq!(total as usize, notes.len());
}

#[tokio::test]
async fn graph_find_unresolved_lists_links() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let request = Request::new(4, "mcp.graph.find_unresolved", Params::default());
    let value = handler
        .handle_request(request)
        .await
        .expect("graph unresolved request succeeds");

    let links = value
        .get("links")
        .and_then(Value::as_array)
        .expect("links array present");
    assert_eq!(
        value.get("total").and_then(Value::as_u64).unwrap_or(0) as usize,
        links.len()
    );
}

#[tokio::test]
async fn notes_create_update_delete_round_trip() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    // Create a new note.
    let create_params = Params::new(json!({
        "title": "Projects/Test Plan",
        "content": "# Test Plan\n- item one",
        "metadata": { "tags": ["testing"] }
    }))
    .expect("create params");
    let create_request = Request::new(5, "mcp.notes.create", create_params);
    let created = handler
        .handle_request(create_request)
        .await
        .expect("create succeeds");
    assert_eq!(
        created.get("noteId").and_then(Value::as_str),
        Some("Projects/Test Plan")
    );

    // Update the created note.
    let update_params = Params::new(json!({
        "noteId": "Projects/Test Plan",
        "content": "Updated body",
        "metadata": { "status": "done" }
    }))
    .expect("update params");
    let update_request = Request::new(6, "mcp.notes.update", update_params);
    let updated = handler
        .handle_request(update_request)
        .await
        .expect("update succeeds");
    assert!(
        updated
            .get("metadata")
            .and_then(Value::as_object)
            .and_then(|map| map.get("status"))
            .and_then(Value::as_str)
            == Some("done")
    );

    // Delete the note.
    let delete_params =
        Params::new(json!({ "noteId": "Projects/Test Plan", "confirm": true })).expect("delete");
    let delete_request = Request::new(7, "mcp.notes.delete", delete_params);
    let deleted = handler
        .handle_request(delete_request)
        .await
        .expect("delete succeeds");
    assert_eq!(deleted.get("deleted").and_then(Value::as_bool), Some(true));
}

#[tokio::test]
async fn discovery_related_notes_returns_payload() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let params =
        Params::new(json!({ "noteId": "Photography Equipment", "limit": 3 })).expect("params");
    let request = Request::new(8, "mcp.discovery.get_related_notes", params);
    let value = handler
        .handle_request(request)
        .await
        .expect("related notes request succeeds");

    assert_eq!(
        value.get("noteId").and_then(Value::as_str),
        Some("Photography Equipment")
    );
    assert!(value.get("related").is_some());
}

#[tokio::test]
async fn discovery_vault_stats_returns_counts() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let params = Params::new(json!({ "recentLimit": 5 })).expect("stats params");
    let request = Request::new(9, "mcp.discovery.get_vault_stats", params);
    let value = handler
        .handle_request(request)
        .await
        .expect("vault stats request succeeds");

    let total = value
        .get("totalNotes")
        .or_else(|| value.get("total_notes")) // camelCase post-serialization
        .and_then(Value::as_u64)
        .expect("total notes present");
    assert!(total > 0);
}

#[tokio::test]
async fn discovery_vault_conventions_returns_patterns() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let request = Request::new(10, "mcp.discovery.get_vault_conventions", Params::default());
    let value = handler
        .handle_request(request)
        .await
        .expect("vault conventions succeeds");

    assert!(value.get("namingPatterns").is_some());
    assert!(value.get("metadataFields").is_some());
}

#[tokio::test]
async fn protocol_initialize_reports_capabilities() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let params = Params::new(json!({ "clientName": "Integration Test", "clientVersion": "1.0.0" }))
        .expect("init params");
    let request = Request::new(11, "mcp.protocol.initialize", params);
    let value = handler
        .handle_request(request)
        .await
        .expect("initialize succeeds");

    assert_eq!(
        value
            .get("serverInfo")
            .or_else(|| value.get("server_info"))
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str),
        Some("Arrowhead MCP")
    );
    assert!(
        value
            .get("capabilities")
            .or_else(|| value.get("capabilities"))
            .is_some(),
        "capabilities should be present"
    );
}

#[tokio::test]
async fn protocol_tools_list_contains_note_create() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let request = Request::new(12, "mcp.protocol.tools/list", Params::default());
    let value = handler
        .handle_request(request)
        .await
        .expect("tools list succeeds");

    let tools = value
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array present");
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some("mcp.notes.create")),
        "tool list should include mcp.notes.create"
    );
}
