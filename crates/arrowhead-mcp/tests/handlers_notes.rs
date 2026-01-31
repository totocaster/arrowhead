use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use arrowhead_core::status::DaemonStatus;
use arrowhead_core::workspace::{WORKSPACE_CONFIG_FILE, WorkspaceFile, write_workspace_file};
use arrowhead_mcp::{
    handlers::HandlerRegistry,
    protocol::{Params, ProtocolError, Request},
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

fn copy_fixture_generic() -> TempDir {
    let temp_dir = copy_fixture();
    let obsidian_dir = temp_dir.path().join(".obsidian");
    if obsidian_dir.exists() {
        fs::remove_dir_all(&obsidian_dir).expect("remove obsidian metadata");
    }
    let arrowhead_dir = temp_dir.path().join(".arrowhead");
    fs::create_dir_all(&arrowhead_dir).expect("create arrowhead dir");
    let file = WorkspaceFile {
        attachments_dir: Some("Attachments".to_string()),
        ignored_folders: vec!["Drafts".to_string()],
        daily_note_format: Some("YYYY-MM-DD".to_string()),
        link_style: Some("relative".to_string()),
    };
    write_workspace_file(&arrowhead_dir.join(WORKSPACE_CONFIG_FILE), &file)
        .expect("write workspace config");
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

async fn call_tool(handler: &HandlerRegistry, name: &str, arguments: Value) -> Value {
    let params = Params::new(json!({ "name": name, "arguments": arguments })).expect("tool params");
    let request = Request::new(0, "tools/call", params);
    handler
        .handle_request(request)
        .await
        .expect("tool call succeeds")
}

async fn call_tool_structured(
    handler: &HandlerRegistry,
    name: &str,
    arguments: Value,
) -> serde_json::Map<String, Value> {
    call_tool(handler, name, arguments)
        .await
        .get("structuredContent")
        .and_then(Value::as_object)
        .cloned()
        .expect("structured content present")
}

async fn call_tool_error(handler: &HandlerRegistry, name: &str, arguments: Value) -> ProtocolError {
    let params = Params::new(json!({ "name": name, "arguments": arguments })).expect("tool params");
    let request = Request::new(0, "tools/call", params);
    handler
        .handle_request(request)
        .await
        .expect_err("tool call should fail")
}

#[tokio::test]
async fn notes_list_ids_only_returns_results() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let structured = call_tool_structured(&handler, "notes_list", json!({ "idsOnly": true })).await;

    let notes = structured
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

    let structured = call_tool_structured(
        &handler,
        "notes_read",
        json!({ "noteId": "Photography Equipment" }),
    )
    .await;

    assert_eq!(
        structured.get("noteId").and_then(Value::as_str),
        Some("Photography Equipment")
    );
    assert!(structured.get("content").and_then(Value::as_str).is_some());
    assert!(structured.get("metadata").is_some());
}

#[tokio::test]
async fn vault_status_returns_cached_status_when_daemon_unreachable() {
    let temp_dir = copy_fixture();
    let status_path = temp_dir.path().join("status.json");
    let mut status = DaemonStatus::new(temp_dir.path().join("daemon.log"));
    status.indexed_notes = 42;
    status.error_notes = 1;
    status
        .save_to_path(&status_path)
        .expect("write status snapshot");

    let handler = build_handler(&temp_dir).await;

    let structured = call_tool_structured(&handler, "vault_status", json!({})).await;

    assert_eq!(
        structured
            .get("indexedNotes")
            .or_else(|| structured.get("indexed_notes"))
            .and_then(Value::as_u64),
        Some(42)
    );
    assert!(structured.get("summary").and_then(Value::as_str).is_some());

    let issues = structured
        .get("issues")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(issues.iter().any(|issue| {
        issue.get("code").and_then(Value::as_str) == Some("daemon_offline_cached_status")
    }));
}

#[tokio::test]
async fn graph_find_orphans_reports_notes() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let structured = call_tool_structured(&handler, "graph_find_orphans", json!({})).await;

    let total = structured.get("total").and_then(Value::as_u64).unwrap_or(0);
    let notes = structured
        .get("notes")
        .and_then(Value::as_array)
        .expect("notes array present");
    assert_eq!(total as usize, notes.len());
}

#[tokio::test]
async fn graph_find_unresolved_lists_links() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let structured = call_tool_structured(&handler, "graph_find_unresolved", json!({})).await;

    let links = structured
        .get("links")
        .and_then(Value::as_array)
        .expect("links array present");
    assert_eq!(
        structured.get("total").and_then(Value::as_u64).unwrap_or(0) as usize,
        links.len()
    );
}

#[tokio::test]
async fn notes_create_update_delete_round_trip() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    // Create a new note.
    let created = call_tool_structured(
        &handler,
        "notes_create",
        json!({
            "title": "Projects/Test Plan",
            "content": "# Test Plan\n- item one",
            "metadata": { "tags": ["testing"] }
        }),
    )
    .await;
    assert_eq!(
        created.get("noteId").and_then(Value::as_str),
        Some("Projects/Test Plan")
    );

    // Update the created note.
    let updated = call_tool_structured(
        &handler,
        "notes_update",
        json!({
            "noteId": "Projects/Test Plan",
            "content": "Updated body",
            "metadata": { "status": "done" }
        }),
    )
    .await;
    assert!(
        updated
            .get("metadata")
            .and_then(Value::as_object)
            .and_then(|map| map.get("status"))
            .and_then(Value::as_str)
            == Some("done")
    );

    // Delete the note.
    let deleted = call_tool_structured(
        &handler,
        "notes_delete",
        json!({ "noteId": "Projects/Test Plan", "confirm": true }),
    )
    .await;
    assert_eq!(deleted.get("deleted").and_then(Value::as_bool), Some(true));
}

#[tokio::test]
async fn notes_delete_requires_confirmation_flag() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let error = call_tool_error(
        &handler,
        "notes_delete",
        json!({ "noteId": "Photography Equipment" }),
    )
    .await;

    match error {
        ProtocolError::InvalidParams { message } => {
            assert!(message.contains("confirm"), "unexpected message: {message}");
        }
        other => panic!("expected invalid params error, got {:?}", other),
    }
}

#[tokio::test]
async fn discovery_related_notes_returns_payload() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let structured = call_tool_structured(
        &handler,
        "discovery_get_related_notes",
        json!({ "noteId": "Photography Equipment", "limit": 3 }),
    )
    .await;

    assert_eq!(
        structured.get("noteId").and_then(Value::as_str),
        Some("Photography Equipment")
    );
    assert!(structured.get("related").is_some());
}

#[tokio::test]
async fn discovery_related_notes_accepts_query() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let structured = call_tool_structured(
        &handler,
        "discovery_get_related_notes",
        json!({ "query": "photography", "limit": 3 }),
    )
    .await;

    assert_eq!(
        structured.get("query").and_then(Value::as_str),
        Some("photography")
    );
    assert!(structured.get("related").is_some());
}

#[tokio::test]
async fn discovery_vault_stats_returns_counts() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let structured = call_tool_structured(
        &handler,
        "discovery_get_vault_stats",
        json!({ "recentLimit": 5 }),
    )
    .await;

    let total = structured
        .get("totalNotes")
        .or_else(|| structured.get("total_notes"))
        .and_then(Value::as_u64)
        .expect("total notes present");
    assert!(total > 0);
}

#[tokio::test]
async fn discovery_vault_conventions_returns_patterns() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let structured =
        call_tool_structured(&handler, "discovery_get_vault_conventions", json!({})).await;

    assert!(structured.get("namingPatterns").is_some());
    assert!(structured.get("metadataFields").is_some());
    let style = structured
        .get("styleGuide")
        .and_then(Value::as_object)
        .expect("style guide present");
    let relative_path = style
        .get("relativePath")
        .and_then(Value::as_str)
        .expect("style guide relative path");
    assert!(
        relative_path.ends_with("ARROWHEAD.md"),
        "expected Arrowhead guide path, got {relative_path}"
    );
    let content = style
        .get("content")
        .and_then(Value::as_str)
        .expect("style guide content");
    assert!(
        content.contains("Arrowhead Guide"),
        "expected Arrowhead guide content"
    );

    let agents = structured
        .get("agentsPlaybook")
        .and_then(Value::as_object)
        .expect("agents playbook present");
    let agents_path = agents
        .get("relativePath")
        .and_then(Value::as_str)
        .expect("agents playbook path");
    assert_eq!(agents_path, "AGENTS.md");
    let agents_content = agents
        .get("content")
        .and_then(Value::as_str)
        .expect("agents playbook content");
    assert!(
        agents_content.contains("Arrowhead Coding Agent Playbook"),
        "expected agents playbook content"
    );

    let workspace = structured
        .get("workspace")
        .and_then(Value::as_object)
        .expect("workspace payload present");
    assert_eq!(
        workspace.get("kind").and_then(Value::as_str),
        Some("obsidian")
    );
    assert!(
        structured.get("obsidian").is_some(),
        "legacy obsidian payload should remain for compatibility"
    );
}

#[tokio::test]
async fn discovery_vault_conventions_reports_generic_workspace_metadata() {
    let temp_dir = copy_fixture_generic();
    let handler = build_handler(&temp_dir).await;

    let structured =
        call_tool_structured(&handler, "discovery_get_vault_conventions", json!({})).await;

    assert!(
        structured.get("obsidian").is_none(),
        "generic workspaces should omit obsidian payload"
    );
    let workspace = structured
        .get("workspace")
        .and_then(Value::as_object)
        .expect("workspace payload present");
    assert_eq!(
        workspace.get("kind").and_then(Value::as_str),
        Some("generic")
    );
    let ignored = workspace
        .get("ignoredFolders")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(ignored.iter().any(|value| value == "Drafts"));
}

#[tokio::test]
async fn protocol_initialize_reports_capabilities() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let params = Params::new(json!({
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "clientInfo": { "name": "integration-test", "version": "1.0.0" }
    }))
    .expect("init params");
    let request = Request::new(11, "initialize", params);
    let value = handler
        .handle_request(request)
        .await
        .expect("initialize succeeds");

    assert_eq!(
        value.get("protocolVersion").and_then(Value::as_str),
        Some("2025-06-18")
    );
    assert_eq!(
        value
            .get("serverInfo")
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str),
        Some("arrowhead-mcp")
    );
    assert!(
        value.get("capabilities").is_some(),
        "capabilities should be present"
    );
}

#[tokio::test]
async fn protocol_tools_list_contains_note_create() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let request = Request::new(12, "tools/list", Params::default());
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
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some("notes_create")),
        "tool list should include notes_create"
    );
}
