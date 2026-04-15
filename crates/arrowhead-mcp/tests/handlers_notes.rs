use std::{
    collections::HashSet,
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
};

use arrowhead_core::status::DaemonStatus;
use arrowhead_core::workspace::{WORKSPACE_CONFIG_FILE, WorkspaceFile, write_workspace_file};
use arrowhead_core::{
    LinkResolutionRecord, MetadataMap, NoteRecord, Vault,
    graph::LinkReason,
    metadata::{MetadataExtraction, MetadataExtractor},
    metrics::MetricsConfigFile,
    parse_metrics_reader,
    sqlite::IndexDatabase,
};
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
        metrics: None,
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

async fn build_handler_with_metrics(temp_dir: &TempDir) -> HandlerRegistry {
    let metrics_dir = temp_dir.path().join("Metrics");
    fs::create_dir_all(&metrics_dir).expect("create metrics dir");
    let relative_path = "Metrics/health.metrics.ndjson";
    let contents = concat!(
        r#"{"id":"01AAA","ts":"2026-04-14T08:30:00+00:00","key":"body.weight","value":105.6,"unit":"kg","source":"withings","note":"Morning weigh-in","tags":["health"]}"#,
        "\n",
        r#"{"id":"01AAB","ts":"2026-04-14T12:00:00+00:00","key":"nutrition.energy_intake","value":850,"unit":"kcal","source":"manual","note":"Steak dinner","tags":["food"]}"#
    );
    fs::write(
        metrics_dir.join("health.metrics.ndjson"),
        format!("{contents}\n"),
    )
    .expect("write metrics file");

    let vault_path = temp_dir.path().to_path_buf();
    let runtime = McpRuntime::initialise(
        RuntimeOptions::new(vault_path)
            .with_embedding_model(None)
            .with_daemon_socket(Some(temp_dir.path().join("control.sock")))
            .with_daemon_status(Some(temp_dir.path().join("status.json"))),
    )
    .await
    .expect("runtime initialises");

    let rows = parse_metrics_reader(Cursor::new(contents), Path::new(relative_path))
        .expect("parse metrics rows");
    runtime
        .database()
        .upsert_metrics_file(relative_path, chrono::Utc::now(), &rows, chrono::Utc::now())
        .expect("upsert indexed metrics");

    HandlerRegistry::new(Arc::new(runtime))
}

async fn build_handler_with_context(temp_dir: &TempDir) -> HandlerRegistry {
    let arrowhead_dir = temp_dir.path().join(".arrowhead");
    fs::create_dir_all(&arrowhead_dir).expect("create arrowhead dir");
    write_workspace_file(
        &arrowhead_dir.join(WORKSPACE_CONFIG_FILE),
        &WorkspaceFile {
            attachments_dir: None,
            ignored_folders: Vec::new(),
            daily_note_format: Some("YYYY-MM-DD".to_string()),
            link_style: Some("relative".to_string()),
            metrics: Some(MetricsConfigFile {
                root: Some("Metrics".to_string()),
                extensions: vec![".metrics.ndjson".to_string()],
                default_write_file: Some("Metrics/All.metrics.ndjson".to_string()),
                record_reference_prefix: Some("metric:".to_string()),
                week_start_day: None,
                day_start_hour: None,
            }),
        },
    )
    .expect("write workspace config");

    let vault_path = temp_dir.path().to_path_buf();
    let runtime = McpRuntime::initialise(
        RuntimeOptions::new(vault_path)
            .with_embedding_model(None)
            .with_daemon_socket(Some(temp_dir.path().join("control.sock")))
            .with_daemon_status(Some(temp_dir.path().join("status.json"))),
    )
    .await
    .expect("runtime initialises");

    seed_context_vault(runtime.vault().as_ref(), runtime.database().as_ref());

    HandlerRegistry::new(Arc::new(runtime))
}

fn seed_context_vault(vault: &Vault, database: &IndexDatabase) {
    let notes = vec![
        build_context_note(
            vault,
            "Project Hub",
            Some("Project Hub"),
            "Track body.weight in [[2026-04-14]] and metric:01AAA from withings.",
        ),
        build_context_note(
            vault,
            "2026-04-14",
            Some("2026-04-14"),
            "Daily note for body.weight updates.",
        ),
        build_context_note(
            vault,
            "Related Note",
            Some("Related Note"),
            "See [[Project Hub]] for the latest withings import.",
        ),
    ];
    let note_ids = notes
        .iter()
        .map(|note| note.id.clone())
        .collect::<HashSet<_>>();

    for note in notes {
        let extraction = MetadataExtractor::new()
            .extract(&note)
            .expect("extract metadata");
        let resolved_links = make_resolved_links(&extraction, &note_ids);
        database
            .upsert_note(&note, &extraction, &resolved_links, chrono::Utc::now())
            .expect("upsert note");
    }

    let rows = parse_metrics_reader(
        Cursor::new(
            concat!(
                r#"{"id":"01AAA","ts":"2026-04-14T08:30:00+00:00","date":"2026-04-14","key":"body.weight","value":105.6,"unit":"kg","source":"withings","note":"Morning weigh-in"}"#,
                "\n",
                r#"{"id":"01AAB","ts":"2026-04-15T08:30:00+00:00","date":"2026-04-15","key":"body.weight","value":105.2,"unit":"kg","source":"withings","note":"Follow-up weigh-in"}"#
            ),
        ),
        Path::new("Metrics/All.metrics.ndjson"),
    )
    .expect("parse metrics rows");
    database
        .upsert_metrics_file(
            "Metrics/All.metrics.ndjson",
            chrono::Utc::now(),
            &rows,
            chrono::Utc::now(),
        )
        .expect("upsert context metrics");
}

fn build_context_note(vault: &Vault, note_id: &str, title: Option<&str>, body: &str) -> NoteRecord {
    let mut metadata = MetadataMap::default();
    if let Some(title) = title {
        metadata.insert("title".to_string(), Value::String(title.to_string()));
    }
    vault
        .write_note(note_id, &metadata, body)
        .expect("write note");
    vault.load_note(note_id).expect("load note")
}

fn make_resolved_links(
    extraction: &MetadataExtraction,
    note_ids: &HashSet<String>,
) -> Vec<LinkResolutionRecord> {
    extraction
        .wikilinks
        .iter()
        .map(|link| {
            let target = note_ids.contains(&link.target).then(|| link.target.clone());
            LinkResolutionRecord {
                raw: link.raw.clone(),
                target,
                display: link.display.clone(),
                heading: link.heading.clone(),
                reason: if note_ids.contains(&link.target) {
                    LinkReason::Direct
                } else {
                    LinkReason::Unresolved
                },
            }
        })
        .collect()
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
async fn metrics_list_files_returns_results() {
    let temp_dir = copy_fixture();
    let handler = build_handler_with_metrics(&temp_dir).await;

    let structured = call_tool_structured(&handler, "metrics_list_files", json!({})).await;

    let files = structured
        .get("files")
        .and_then(Value::as_array)
        .expect("files array present");
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].get("relativePath").and_then(Value::as_str),
        Some("Metrics/health.metrics.ndjson")
    );
}

#[tokio::test]
async fn metrics_read_returns_expected_fields() {
    let temp_dir = copy_fixture();
    let handler = build_handler_with_metrics(&temp_dir).await;

    let structured = call_tool_structured(
        &handler,
        "metrics_read",
        json!({ "metricId": "metric:01AAA" }),
    )
    .await;

    let record = structured
        .get("record")
        .and_then(Value::as_object)
        .expect("record payload present");
    assert_eq!(
        record
            .get("record")
            .and_then(Value::as_object)
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str),
        Some("01AAA")
    );
    assert_eq!(record.get("sourceLine").and_then(Value::as_u64), Some(1));
}

#[tokio::test]
async fn metrics_search_returns_matches() {
    let temp_dir = copy_fixture();
    let handler = build_handler_with_metrics(&temp_dir).await;

    let structured = call_tool_structured(
        &handler,
        "metrics_search",
        json!({ "query": "\"steak dinner\"", "limit": 5 }),
    )
    .await;

    assert_eq!(structured.get("total").and_then(Value::as_u64), Some(1));
    let results = structured
        .get("results")
        .and_then(Value::as_array)
        .expect("results array present");
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0]
            .get("record")
            .and_then(Value::as_object)
            .and_then(|record| record.get("id"))
            .and_then(Value::as_str),
        Some("01AAB")
    );
}

#[tokio::test]
async fn metrics_create_returns_indexed_record() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let handler = build_handler(&temp_dir).await;

    let structured = call_tool_structured(
        &handler,
        "metrics_create",
        json!({
            "id": "01MCPCREATE00000000000000",
            "ts": "2026-04-15T08:30:00+04:00",
            "key": "body.weight",
            "value": 105.6,
            "unit": "kg",
            "source": "withings",
            "tags": ["health"]
        }),
    )
    .await;

    let record = structured
        .get("record")
        .and_then(Value::as_object)
        .expect("record payload present");
    assert_eq!(
        record
            .get("record")
            .and_then(Value::as_object)
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str),
        Some("01MCPCREATE00000000000000")
    );
    assert_eq!(
        record.get("sourceFile").and_then(Value::as_str),
        Some("Metrics/All.metrics.ndjson")
    );
}

#[tokio::test]
async fn metrics_update_mutates_existing_record() {
    let temp_dir = copy_fixture();
    let handler = build_handler_with_metrics(&temp_dir).await;

    let structured = call_tool_structured(
        &handler,
        "metrics_update",
        json!({
            "metricId": "01AAA",
            "value": 104.9,
            "note": "Updated by MCP"
        }),
    )
    .await;

    let record = structured
        .get("record")
        .and_then(Value::as_object)
        .expect("record payload present");
    let payload = record
        .get("record")
        .and_then(Value::as_object)
        .expect("metric record present");
    assert_eq!(payload.get("value").and_then(Value::as_f64), Some(104.9));
    assert_eq!(
        payload.get("note").and_then(Value::as_str),
        Some("Updated by MCP")
    );
}

#[tokio::test]
async fn metrics_delete_requires_confirmation() {
    let temp_dir = copy_fixture();
    let handler = build_handler_with_metrics(&temp_dir).await;

    let err = call_tool_error(&handler, "metrics_delete", json!({ "metricId": "01AAA" })).await;
    assert!(matches!(err, ProtocolError::InvalidParams { .. }));
}

#[tokio::test]
async fn metrics_delete_removes_record() {
    let temp_dir = copy_fixture();
    let handler = build_handler_with_metrics(&temp_dir).await;

    let structured = call_tool_structured(
        &handler,
        "metrics_delete",
        json!({ "metricId": "01AAA", "confirm": true }),
    )
    .await;

    let deleted = structured
        .get("deleted")
        .and_then(Value::as_object)
        .expect("delete payload present");
    assert_eq!(
        deleted.get("metricId").and_then(Value::as_str),
        Some("01AAA")
    );

    let err = call_tool_error(&handler, "metrics_read", json!({ "metricId": "01AAA" })).await;
    assert!(matches!(err, ProtocolError::InvalidParams { .. }));
}

#[tokio::test]
async fn metrics_create_file_creates_indexed_file() {
    let temp_dir = copy_fixture();
    let handler = build_handler(&temp_dir).await;

    let structured = call_tool_structured(
        &handler,
        "metrics_create_file",
        json!({ "path": "Metrics/new.metrics.ndjson" }),
    )
    .await;

    let file = structured
        .get("file")
        .and_then(Value::as_object)
        .expect("file payload present");
    assert_eq!(
        file.get("relativePath").and_then(Value::as_str),
        Some("Metrics/new.metrics.ndjson")
    );
    assert!(
        temp_dir.path().join("Metrics/new.metrics.ndjson").exists(),
        "created file should exist on disk"
    );
}

#[tokio::test]
async fn metrics_rename_file_moves_indexed_file() {
    let temp_dir = copy_fixture();
    let handler = build_handler_with_metrics(&temp_dir).await;

    let structured = call_tool_structured(
        &handler,
        "metrics_rename_file",
        json!({
            "sourcePath": "Metrics/health.metrics.ndjson",
            "destinationPath": "Metrics/body.metrics.ndjson"
        }),
    )
    .await;

    let file = structured
        .get("file")
        .and_then(Value::as_object)
        .expect("file payload present");
    assert_eq!(
        file.get("sourcePath").and_then(Value::as_str),
        Some("Metrics/health.metrics.ndjson")
    );
    assert_eq!(
        file.get("destinationPath").and_then(Value::as_str),
        Some("Metrics/body.metrics.ndjson")
    );
    assert!(
        !temp_dir
            .path()
            .join("Metrics/health.metrics.ndjson")
            .exists(),
        "source file should be removed"
    );
    assert!(
        temp_dir.path().join("Metrics/body.metrics.ndjson").exists(),
        "destination file should exist"
    );
}

#[tokio::test]
async fn metrics_delete_file_requires_confirmation() {
    let temp_dir = copy_fixture();
    let handler = build_handler_with_metrics(&temp_dir).await;

    let err = call_tool_error(
        &handler,
        "metrics_delete_file",
        json!({ "path": "Metrics/health.metrics.ndjson" }),
    )
    .await;
    assert!(matches!(err, ProtocolError::InvalidParams { .. }));
}

#[tokio::test]
async fn metrics_delete_file_removes_path() {
    let temp_dir = copy_fixture();
    let handler = build_handler_with_metrics(&temp_dir).await;

    let structured = call_tool_structured(
        &handler,
        "metrics_delete_file",
        json!({ "path": "Metrics/health.metrics.ndjson", "confirm": true }),
    )
    .await;

    let file = structured
        .get("file")
        .and_then(Value::as_object)
        .expect("file payload present");
    assert_eq!(
        file.get("relativePath").and_then(Value::as_str),
        Some("Metrics/health.metrics.ndjson")
    );
    assert!(
        !temp_dir
            .path()
            .join("Metrics/health.metrics.ndjson")
            .exists(),
        "deleted file should be removed"
    );
}

#[tokio::test]
async fn context_get_note_returns_context_sections() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let handler = build_handler_with_context(&temp_dir).await;

    let structured = call_tool_structured(
        &handler,
        "context_get_note",
        json!({ "noteId": "Project Hub" }),
    )
    .await;

    assert_eq!(
        structured
            .get("summary")
            .and_then(Value::as_object)
            .and_then(|summary| summary.get("kind"))
            .and_then(Value::as_str),
        Some("note")
    );
    assert!(
        structured
            .get("activity")
            .and_then(Value::as_object)
            .and_then(|activity| activity.get("metrics"))
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty()),
        "expected note context metric activity"
    );
}

#[tokio::test]
async fn context_get_metric_accepts_metric_key() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let handler = build_handler_with_context(&temp_dir).await;

    let structured = call_tool_structured(
        &handler,
        "context_get_metric",
        json!({ "metric": "body.weight" }),
    )
    .await;

    assert_eq!(
        structured
            .get("summary")
            .and_then(Value::as_object)
            .and_then(|summary| summary.get("kind"))
            .and_then(Value::as_str),
        Some("metric")
    );
    assert!(
        structured
            .get("related")
            .and_then(Value::as_object)
            .and_then(|related| related.get("sources"))
            .and_then(Value::as_array)
            .is_some_and(|sources| sources
                .iter()
                .any(|source| source.as_str() == Some("withings"))),
        "expected metric context sources"
    );
}

#[tokio::test]
async fn context_get_source_returns_metric_keys() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let handler = build_handler_with_context(&temp_dir).await;

    let structured = call_tool_structured(
        &handler,
        "context_get_source",
        json!({ "source": "withings" }),
    )
    .await;

    assert_eq!(
        structured
            .get("summary")
            .and_then(Value::as_object)
            .and_then(|summary| summary.get("kind"))
            .and_then(Value::as_str),
        Some("source")
    );
    assert!(
        structured
            .get("related")
            .and_then(Value::as_object)
            .and_then(|related| related.get("metricKeys"))
            .and_then(Value::as_array)
            .is_some_and(|keys| keys.iter().any(|key| key.as_str() == Some("body.weight"))),
        "expected source context metric keys"
    );
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
    let metrics = structured
        .get("metrics")
        .and_then(Value::as_object)
        .expect("metrics payload present");
    assert_eq!(
        metrics.get("source").and_then(Value::as_str),
        Some("default")
    );
    let files = metrics
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        files.is_empty(),
        "fixture vault should not include metrics files"
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
async fn discovery_vault_conventions_reports_metrics_overrides_and_files() {
    let temp_dir = copy_fixture_generic();
    fs::create_dir_all(temp_dir.path().join("Health")).expect("create metrics dir");
    fs::write(
        temp_dir.path().join("Health").join("daily.health.ndjson"),
        "{}\n",
    )
    .expect("write metrics file");

    write_workspace_file(
        &temp_dir
            .path()
            .join(".arrowhead")
            .join(WORKSPACE_CONFIG_FILE),
        &WorkspaceFile {
            attachments_dir: Some("Attachments".to_string()),
            ignored_folders: vec!["Drafts".to_string()],
            daily_note_format: Some("YYYY-MM-DD".to_string()),
            link_style: Some("relative".to_string()),
            metrics: Some(MetricsConfigFile {
                root: Some("Health".to_string()),
                extensions: vec![".health.ndjson".to_string()],
                default_write_file: Some("Health/Inbox.health.ndjson".to_string()),
                record_reference_prefix: Some("health:".to_string()),
                week_start_day: Some("sunday".to_string()),
                day_start_hour: Some(5),
            }),
        },
    )
    .expect("rewrite workspace config");

    let handler = build_handler(&temp_dir).await;
    let structured =
        call_tool_structured(&handler, "discovery_get_vault_conventions", json!({})).await;

    let metrics = structured
        .get("metrics")
        .and_then(Value::as_object)
        .expect("metrics payload present");
    assert_eq!(
        metrics.get("source").and_then(Value::as_str),
        Some("arrowhead-workspace")
    );
    assert_eq!(metrics.get("root").and_then(Value::as_str), Some("Health"));
    assert_eq!(
        metrics.get("defaultWriteFile").and_then(Value::as_str),
        Some("Health/Inbox.health.ndjson")
    );
    assert_eq!(
        metrics.get("recordReferencePrefix").and_then(Value::as_str),
        Some("health:")
    );
    assert_eq!(metrics.get("dayStartHour").and_then(Value::as_u64), Some(5));
    let files = metrics
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    assert!(
        files
            .iter()
            .any(|value| value == "Health/daily.health.ndjson")
    );
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
async fn protocol_tools_list_contains_metrics_and_note_tools() {
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
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some("metrics_search")),
        "tool list should include metrics_search"
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some("metrics_create")),
        "tool list should include metrics_create"
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some("metrics_update")),
        "tool list should include metrics_update"
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some("metrics_delete")),
        "tool list should include metrics_delete"
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some("metrics_create_file")),
        "tool list should include metrics_create_file"
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some("metrics_rename_file")),
        "tool list should include metrics_rename_file"
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some("metrics_delete_file")),
        "tool list should include metrics_delete_file"
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some("context_get_note")),
        "tool list should include context_get_note"
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some("context_get_metric")),
        "tool list should include context_get_metric"
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name").and_then(Value::as_str) == Some("context_get_source")),
        "tool list should include context_get_source"
    );
}
