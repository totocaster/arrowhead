//! Tool request handlers
//!
//! Implements request handlers for all MCP tools.

use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Error;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::task;
use tracing::debug;

use arrowhead_core::{
    MetadataMap, NoteRecord, Vault,
    status::{ActivityState, ActivityStatus, DeamonStatus},
};

use crate::{
    protocol::{ErrorCode, Notification, ProtocolError, Request},
    runtime::McpRuntime,
    stdio::MessageHandler,
    tools::{
        DaemonStatusPayload, GraphContextPayload, GraphLinksPayload, GraphNoteParams,
        GraphOrphansPayload, GraphUnresolvedPayload, InitializeParams, InitializePayload,
        LinkEdgePayload, NoteContentPayload, NoteCreateParams, NoteDeleteParams, NoteDeletePayload,
        NoteListItem, NoteMetadataParams, NoteMetadataPayload, NoteReadParams, NoteUpdateParams,
        NotesListParams, NotesListPayload, OrphanNotePayload, RelatedNotesParams, SearchParams,
        SearchResultPayload, SearchResultsPayload, ServerCapabilitiesPayload, ServerInfoPayload,
        ToolDescriptor, ToolExample, ToolsListPayload, VaultStatsParams,
    },
};

/// Dispatches MCP method calls to concrete implementations.
#[derive(Debug, Clone)]
pub struct HandlerRegistry {
    runtime: Arc<McpRuntime>,
}

impl HandlerRegistry {
    /// Create a new registry instance with the provided runtime.
    pub fn new(runtime: Arc<McpRuntime>) -> Self {
        Self { runtime }
    }

    async fn handle_graph_context(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: GraphNoteParams = request.params.deserialize()?;
        self.ensure_note_indexed(&params.note_id).await?;
        let context = self
            .runtime
            .graph_service()
            .context(&params.note_id)
            .await
            .map_err(|err| {
                ProtocolError::internal(format!("failed to load graph context: {err}"))
            })?;

        let payload = GraphContextPayload {
            note_id: params.note_id.clone(),
            backlinks: context
                .backlinks
                .iter()
                .map(LinkEdgePayload::from_edge)
                .collect(),
            forward_links: context
                .forward_links
                .iter()
                .map(LinkEdgePayload::from_edge)
                .collect(),
        };

        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise graph context: {err}"))
        })
    }

    async fn handle_graph_links(
        &self,
        request: Request,
        direction: GraphDirection,
    ) -> Result<Value, ProtocolError> {
        let params: GraphNoteParams = request.params.deserialize()?;
        self.ensure_note_indexed(&params.note_id).await?;
        let service = self.runtime.graph_service();
        let links = match direction {
            GraphDirection::Back => service.backlinks(&params.note_id).await.map_err(|err| {
                ProtocolError::internal(format!("failed to load backlinks: {err}"))
            })?,
            GraphDirection::Forward => {
                service
                    .forward_links(&params.note_id)
                    .await
                    .map_err(|err| {
                        ProtocolError::internal(format!("failed to load forward links: {err}"))
                    })?
            }
        };

        let payload = GraphLinksPayload {
            note_id: params.note_id.clone(),
            links: links.iter().map(LinkEdgePayload::from_edge).collect(),
        };

        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise graph links: {err}"))
        })
    }

    async fn handle_graph_orphans(&self) -> Result<Value, ProtocolError> {
        let service = self.runtime.graph_service();
        let orphan_ids = service.orphans().await.map_err(|err| {
            ProtocolError::internal(format!("failed to list orphan notes: {err}"))
        })?;

        if orphan_ids.is_empty() {
            let payload = GraphOrphansPayload {
                total: 0,
                notes: Vec::new(),
            };
            return serde_json::to_value(payload).map_err(|err| {
                ProtocolError::internal(format!("failed to serialise orphan payload: {err}"))
            });
        }

        let snapshot = self.runtime.inventory_snapshot().await.map_err(|err| {
            ProtocolError::internal(format!("failed to load vault inventory: {err}"))
        })?;

        let titles = {
            let db = Arc::clone(self.runtime.database());
            let ids = orphan_ids.clone();
            task::spawn_blocking(move || db.titles_for_notes(&ids))
                .await
                .map_err(|err| ProtocolError::internal(format!("titles task aborted: {err}")))?
                .map_err(|err| {
                    ProtocolError::internal(format!("failed to load orphan note titles: {err}"))
                })?
        };

        let notes = orphan_ids
            .into_iter()
            .map(|note_id| {
                let relative_path = snapshot
                    .get_by_id(&note_id)
                    .map(|entry| entry.relative_path.clone());
                OrphanNotePayload {
                    note_id: note_id.clone(),
                    title: titles.get(&note_id).cloned().unwrap_or(None),
                    relative_path,
                }
            })
            .collect::<Vec<_>>();

        let payload = GraphOrphansPayload {
            total: notes.len(),
            notes,
        };

        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise orphan notes: {err}"))
        })
    }

    async fn handle_graph_unresolved(&self) -> Result<Value, ProtocolError> {
        let links = self
            .runtime
            .graph_service()
            .unresolved_links()
            .await
            .map_err(|err| {
                ProtocolError::internal(format!("failed to list unresolved links: {err}"))
            })?;

        let payload = GraphUnresolvedPayload {
            total: links.len(),
            links: links.iter().map(LinkEdgePayload::from_edge).collect(),
        };

        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise unresolved links: {err}"))
        })
    }

    async fn handle_search_fts(&self, request: Request) -> Result<Value, ProtocolError> {
        self.ensure_daemon_ready().await?;
        let params: SearchParams = request.params.deserialize()?;
        let service = self.runtime.search_service().clone();
        let results = service
            .search_fts(&params.query, params.limit)
            .await
            .map_err(map_search_error)?;
        let payload = SearchResultsPayload {
            total: results.len(),
            results: results
                .iter()
                .map(SearchResultPayload::from_result)
                .collect(),
        };
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise search results: {err}"))
        })
    }

    #[cfg(feature = "vector-lancedb")]
    async fn handle_search_semantic(&self, request: Request) -> Result<Value, ProtocolError> {
        if !self.runtime.semantic_search_enabled() {
            return Err(ProtocolError::custom(
                ErrorCode::ToolDisabled,
                "semantic search is disabled. Enable the `vector-lancedb` feature and reindex the vault.",
                None,
            ));
        }

        self.ensure_daemon_ready().await?;
        let params: SearchParams = request.params.deserialize()?;
        let service = self.runtime.search_service().clone();
        let results = service
            .search_semantic(&params.query, params.limit)
            .await
            .map_err(map_search_error)?;
        let payload = SearchResultsPayload {
            total: results.len(),
            results: results
                .iter()
                .map(SearchResultPayload::from_result)
                .collect(),
        };
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise search results: {err}"))
        })
    }

    #[cfg(not(feature = "vector-lancedb"))]
    async fn handle_search_semantic(&self, _request: Request) -> Result<Value, ProtocolError> {
        Err(ProtocolError::custom(
            ErrorCode::ToolDisabled,
            "semantic search requires Arrowhead to be built with the `vector-lancedb` feature.",
            None,
        ))
    }

    #[cfg(feature = "vector-lancedb")]
    async fn handle_search_hybrid(&self, request: Request) -> Result<Value, ProtocolError> {
        if !self.runtime.semantic_search_enabled() {
            return Err(ProtocolError::custom(
                ErrorCode::ToolDisabled,
                "hybrid search is disabled. Enable the `vector-lancedb` feature and reindex the vault.",
                None,
            ));
        }

        self.ensure_daemon_ready().await?;
        let params: SearchParams = request.params.deserialize()?;
        let service = self.runtime.search_service().clone();
        let results = service
            .search_hybrid(&params.query, params.limit)
            .await
            .map_err(map_search_error)?;
        let payload = SearchResultsPayload {
            total: results.len(),
            results: results
                .iter()
                .map(SearchResultPayload::from_result)
                .collect(),
        };
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise search results: {err}"))
        })
    }

    #[cfg(not(feature = "vector-lancedb"))]
    async fn handle_search_hybrid(&self, _request: Request) -> Result<Value, ProtocolError> {
        Err(ProtocolError::custom(
            ErrorCode::ToolDisabled,
            "hybrid search requires Arrowhead to be built with the `vector-lancedb` feature.",
            None,
        ))
    }

    async fn handle_vault_status(&self) -> Result<Value, ProtocolError> {
        let status = self.daemon_status().await?;
        serde_json::to_value(status).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise vault status: {err}"))
        })
    }

    async fn handle_note_read(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: NoteReadParams = request.params.deserialize()?;
        let runtime = Arc::clone(&self.runtime);
        let note_id = params.note_id.clone();
        let (record, raw) = task::spawn_blocking(move || {
            let vault = runtime.vault();
            let note = vault
                .load_note(&note_id)
                .map_err(|err| map_note_load_error(err, &note_id))?;
            let raw_path = vault.note_file_path(&note_id).map_err(|err| {
                ProtocolError::internal(format!("failed to resolve note path {}: {err}", note_id))
            })?;
            let raw_content = std::fs::read_to_string(&raw_path).map_err(|err| {
                ProtocolError::internal(format!(
                    "failed to read note file {}: {err}",
                    raw_path.display()
                ))
            })?;
            Ok::<_, ProtocolError>((note, raw_content))
        })
        .await
        .map_err(|err| ProtocolError::internal(format!("note read task aborted: {err}")))??;

        let payload = NoteContentPayload::from_record(&record, Some(raw));
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise note content: {err}"))
        })
    }

    async fn handle_note_list(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: NotesListParams = request.params.deserialize()?;
        let runtime = Arc::clone(&self.runtime);
        let snapshot = task::spawn_blocking(move || {
            runtime.vault().inventory_snapshot().map_err(|err| {
                ProtocolError::internal(format!("failed to build vault inventory: {err}"))
            })
        })
        .await
        .map_err(|err| ProtocolError::internal(format!("inventory task aborted: {err}")))??;

        let mut entries: Vec<_> = snapshot.entries().to_vec();
        entries.sort_by(|a, b| a.id.cmp(&b.id));

        if let Some(limit) = params.limit {
            if entries.len() > limit {
                entries.truncate(limit);
            }
        }

        let note_ids: Vec<String> = entries.iter().map(|entry| entry.id.clone()).collect();
        let titles = {
            let database = Arc::clone(self.runtime.database());
            task::spawn_blocking(move || database.titles_for_notes(&note_ids))
                .await
                .map_err(|err| ProtocolError::internal(format!("titles task aborted: {err}")))?
                .map_err(|err| {
                    ProtocolError::internal(format!("failed to load note titles: {err}"))
                })?
        };

        let notes = entries
            .into_iter()
            .map(|entry| {
                if params.ids_only {
                    NoteListItem {
                        note_id: entry.id,
                        title: None,
                        relative_path: None,
                        file_modified_at: None,
                        created_at: None,
                    }
                } else {
                    NoteListItem {
                        title: titles.get(&entry.id).cloned().unwrap_or(None),
                        relative_path: Some(entry.relative_path.clone()),
                        file_modified_at: Some(entry.file_modified_at),
                        created_at: entry.created_at,
                        note_id: entry.id,
                    }
                }
            })
            .collect();

        let payload = NotesListPayload { notes };
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise notes list: {err}"))
        })
    }

    async fn handle_note_metadata(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: NoteMetadataParams = request.params.deserialize()?;
        let runtime = Arc::clone(&self.runtime);
        let note_id = params.note_id.clone();
        let record = task::spawn_blocking(move || {
            runtime
                .vault()
                .load_note(&note_id)
                .map_err(|err| map_note_load_error(err, &note_id))
        })
        .await
        .map_err(|err| ProtocolError::internal(format!("note metadata task aborted: {err}")))??;

        let payload = NoteMetadataPayload {
            note_id: record.id.clone(),
            title: record.title.clone(),
            metadata: record.metadata.clone(),
        };

        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise note metadata: {err}"))
        })
    }

    async fn handle_note_create(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: NoteCreateParams = request.params.deserialize()?;
        let note_id = resolve_note_id(&params)?;
        let vault = Arc::clone(self.runtime.vault());
        let record = task::spawn_blocking(move || create_note_in_vault(vault, note_id, params))
            .await
            .map_err(|err| ProtocolError::internal(format!("note create task aborted: {err}")))??;

        let payload = NoteContentPayload::from_record(&record, None);
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise created note: {err}"))
        })
    }

    async fn handle_note_update(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: NoteUpdateParams = request.params.deserialize()?;
        let vault = Arc::clone(self.runtime.vault());
        let record = task::spawn_blocking(move || update_note_in_vault(vault, params))
            .await
            .map_err(|err| ProtocolError::internal(format!("note update task aborted: {err}")))??;

        let payload = NoteContentPayload::from_record(&record, None);
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise updated note: {err}"))
        })
    }

    async fn handle_note_delete(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: NoteDeleteParams = request.params.deserialize()?;
        if !params.confirm {
            return Err(ProtocolError::invalid_params(
                "set `confirm: true` to delete a note",
            ));
        }

        let vault = Arc::clone(self.runtime.vault());
        let payload = task::spawn_blocking(move || delete_note_in_vault(vault, params))
            .await
            .map_err(|err| ProtocolError::internal(format!("note delete task aborted: {err}")))??;

        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise delete response: {err}"))
        })
    }

    async fn handle_discovery_related_notes(
        &self,
        request: Request,
    ) -> Result<Value, ProtocolError> {
        let params: RelatedNotesParams = request.params.deserialize()?;
        let payload = self
            .runtime
            .compute_related_notes(&params.note_id, params.limit, params.strategy)
            .await
            .map_err(|err| {
                ProtocolError::internal(format!("failed to compute related notes: {err}"))
            })?;

        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise related notes: {err}"))
        })
    }

    async fn handle_discovery_vault_stats(&self, request: Request) -> Result<Value, ProtocolError> {
        let params = if request.params.raw().is_null() {
            VaultStatsParams::default()
        } else {
            request.params.deserialize::<VaultStatsParams>()?
        };

        let recent_limit = params.recent_limit.unwrap_or(10).max(1);
        let payload = self
            .runtime
            .compute_vault_stats(recent_limit)
            .await
            .map_err(|err| {
                ProtocolError::internal(format!("failed to compute vault stats: {err}"))
            })?;

        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise vault stats: {err}"))
        })
    }

    async fn handle_discovery_vault_conventions(&self) -> Result<Value, ProtocolError> {
        let payload = self
            .runtime
            .compute_vault_conventions()
            .await
            .map_err(|err| {
                ProtocolError::internal(format!("failed to compute vault conventions: {err}"))
            })?;

        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise vault conventions: {err}"))
        })
    }

    async fn handle_protocol_initialize(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: InitializeParams = request.params.deserialize()?;
        let capabilities = ServerCapabilitiesPayload {
            semantic_search: self.runtime.semantic_search_enabled(),
            note_writes: true,
            discovery_tools: true,
            graph_tools: true,
        };

        let daemon_status = self
            .runtime
            .cached_daemon_status()
            .await
            .map(build_daemon_status_payload);

        let payload = InitializePayload {
            server_info: ServerInfoPayload {
                name: "Arrowhead MCP".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: Some(format!(
                    "Arrowhead MCP stdio server (client: {})",
                    params.client_name
                )),
            },
            capabilities: Some(capabilities),
            daemon_status,
        };

        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise initialize payload: {err}"))
        })
    }

    async fn handle_protocol_tools_list(&self) -> Result<Value, ProtocolError> {
        let payload = ToolsListPayload {
            tools: self.build_tool_descriptors(),
        };

        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise tools list: {err}"))
        })
    }

    fn build_tool_descriptors(&self) -> Vec<ToolDescriptor> {
        let semantic_flag = Some("vector-lancedb".to_string());
        let mut tools = vec![
            ToolDescriptor {
                name: "mcp.graph.get_context".to_string(),
                category: "graph".to_string(),
                description: "Return backlinks and forward links for a note.".to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Context graph",
                    "Graph context for Photography Equipment.",
                    json!({ "noteId": "Photography Equipment" }),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.graph.get_backlinks".to_string(),
                category: "graph".to_string(),
                description: "List notes linking to the supplied note.".to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Backlinks",
                    "All backlinks to Daily/2024-01-15.",
                    json!({ "noteId": "Daily/2024-01-15" }),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.graph.get_forward_links".to_string(),
                category: "graph".to_string(),
                description: "List outbound links from the supplied note.".to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Forward links",
                    "Forward links from Photography Equipment.",
                    json!({ "noteId": "Photography Equipment" }),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.graph.find_orphans".to_string(),
                category: "graph".to_string(),
                description: "Identify notes without inbound or outbound links.".to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Graph orphans",
                    "List orphaned notes.",
                    json!({}),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.graph.find_unresolved".to_string(),
                category: "graph".to_string(),
                description: "List unresolved WikiLinks requiring manual attention.".to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Unresolved links",
                    "List unresolved links across the vault.",
                    json!({}),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.search.fts".to_string(),
                category: "search".to_string(),
                description: "Execute a full-text search query.".to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "FTS search",
                    "Search for camera related notes.",
                    json!({ "query": "camera" }),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.search.semantic".to_string(),
                category: "search".to_string(),
                description: "Execute a semantic similarity search (requires LanceDB vectors)."
                    .to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Semantic search",
                    "Find notes similar to a query sentence.",
                    json!({ "query": "suggest travel packing lists" }),
                )],
                feature_flag: semantic_flag.clone(),
            },
            ToolDescriptor {
                name: "mcp.search.hybrid".to_string(),
                category: "search".to_string(),
                description: "Execute a hybrid FTS + semantic search (requires LanceDB vectors)."
                    .to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Hybrid search",
                    "Blend keyword and semantic results.",
                    json!({ "query": "portrait lens recommendations" }),
                )],
                feature_flag: semantic_flag.clone(),
            },
            ToolDescriptor {
                name: "mcp.vault.status".to_string(),
                category: "vault".to_string(),
                description: "Fetch the current Arrowhead daemon status snapshot.".to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Vault status",
                    "Daemon health summary.",
                    json!({}),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.notes.read".to_string(),
                category: "notes".to_string(),
                description: "Read the full content of a note.".to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Read note",
                    "Load the Photography Equipment note.",
                    json!({ "noteId": "Photography Equipment" }),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.notes.list".to_string(),
                category: "notes".to_string(),
                description: "List notes in the vault with optional metadata.".to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "List notes",
                    "List note identifiers only.",
                    json!({ "idsOnly": true, "limit": 20 }),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.notes.metadata".to_string(),
                category: "notes".to_string(),
                description: "Fetch metadata for the supplied note without content.".to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Note metadata",
                    "Metadata for GTD/Inbox.",
                    json!({ "noteId": "GTD/Inbox" }),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.notes.create".to_string(),
                category: "notes".to_string(),
                description: "Create a new note with optional metadata.".to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Create note",
                    "Create a project brief with inline content.",
                    json!({
                        "title": "Projects/New Initiative",
                        "content": "# Objectives\nDraft action items."
                    }),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.notes.update".to_string(),
                category: "notes".to_string(),
                description: "Update note content or metadata.".to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Update note",
                    "Update the Photography Equipment note body.",
                    json!({
                        "noteId": "Photography Equipment",
                        "content": "Updated packing list."
                    }),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.notes.delete".to_string(),
                category: "notes".to_string(),
                description: "Delete a note after explicit confirmation.".to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Delete note",
                    "Delete an obsolete scratch note.",
                    json!({ "noteId": "Scratch/Sandbox", "confirm": true }),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.discovery.get_related_notes".to_string(),
                category: "discovery".to_string(),
                description: "Find notes related to the supplied anchor.".to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Related notes",
                    "Related notes for Photography Equipment.",
                    json!({ "noteId": "Photography Equipment", "limit": 5 }),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.discovery.get_vault_stats".to_string(),
                category: "discovery".to_string(),
                description: "Aggregate vault statistics such as counts and recent notes."
                    .to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Vault stats",
                    "Generate statistics including recent notes.",
                    json!({ "recentLimit": 5 }),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.discovery.get_vault_conventions".to_string(),
                category: "discovery".to_string(),
                description: "Summarise naming patterns, metadata usage, and conventions."
                    .to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Vault conventions",
                    "Summarise conventions for the active vault.",
                    json!({}),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.protocol.initialize".to_string(),
                category: "protocol".to_string(),
                description: "Perform the MCP handshake and receive server capabilities."
                    .to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "Initialize",
                    "Initialize the Arrowhead MCP session.",
                    json!({
                        "clientName": "Example Client",
                        "clientVersion": "1.0.0"
                    }),
                )],
                feature_flag: None,
            },
            ToolDescriptor {
                name: "mcp.protocol.tools/list".to_string(),
                category: "protocol".to_string(),
                description: "List all tools exposed by the Arrowhead MCP server.".to_string(),
                input_schema: None,
                output_schema: None,
                examples: vec![make_tool_example(
                    "List tools",
                    "Enumerate the MCP tool surface.",
                    json!({}),
                )],
                feature_flag: None,
            },
        ];

        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    async fn ensure_daemon_ready(&self) -> Result<(), ProtocolError> {
        let _ = self.daemon_status().await?;
        Ok(())
    }

    async fn daemon_status(&self) -> Result<DeamonStatus, ProtocolError> {
        self.runtime.daemon().status().await.map_err(|err| {
            ProtocolError::custom(ErrorCode::ServiceUnavailable, err.to_string(), None)
        })
    }

    async fn ensure_note_indexed(&self, note_id: &str) -> Result<(), ProtocolError> {
        let database = Arc::clone(self.runtime.database());
        let note_id_owned = note_id.to_string();
        let state = task::spawn_blocking(move || database.note_state(&note_id_owned))
            .await
            .map_err(|err| ProtocolError::internal(format!("note state task aborted: {err}")))?
            .map_err(|err| ProtocolError::internal(format!("failed to query note state: {err}")))?;

        if state.is_some() {
            Ok(())
        } else {
            Err(ProtocolError::invalid_params(format!(
                "note {note_id} is not indexed. Run `arrowhead vault start` to refresh the index."
            )))
        }
    }
}

#[async_trait]
impl MessageHandler for HandlerRegistry {
    #[allow(clippy::too_many_lines)]
    async fn handle_request(&self, request: Request) -> Result<Value, ProtocolError> {
        let method = request.method.clone();
        match method.as_str() {
            "mcp.graph.get_context" => self.handle_graph_context(request).await,
            "mcp.graph.get_backlinks" => {
                self.handle_graph_links(request, GraphDirection::Back).await
            }
            "mcp.graph.get_forward_links" => {
                self.handle_graph_links(request, GraphDirection::Forward)
                    .await
            }
            "mcp.graph.find_orphans" => self.handle_graph_orphans().await,
            "mcp.graph.find_unresolved" => self.handle_graph_unresolved().await,
            "mcp.search.fts" => self.handle_search_fts(request).await,
            "mcp.search.semantic" => self.handle_search_semantic(request).await,
            "mcp.search.hybrid" => self.handle_search_hybrid(request).await,
            "mcp.vault.status" => self.handle_vault_status().await,
            "mcp.notes.read" => self.handle_note_read(request).await,
            "mcp.notes.list" => self.handle_note_list(request).await,
            "mcp.notes.metadata" => self.handle_note_metadata(request).await,
            "mcp.notes.create" => self.handle_note_create(request).await,
            "mcp.notes.update" => self.handle_note_update(request).await,
            "mcp.notes.delete" => self.handle_note_delete(request).await,
            "mcp.discovery.get_related_notes" => self.handle_discovery_related_notes(request).await,
            "mcp.discovery.get_vault_stats" => self.handle_discovery_vault_stats(request).await,
            "mcp.discovery.get_vault_conventions" => {
                let _ = request;
                self.handle_discovery_vault_conventions().await
            }
            "mcp.protocol.initialize" => self.handle_protocol_initialize(request).await,
            "mcp.protocol.tools/list" => {
                let _ = request;
                self.handle_protocol_tools_list().await
            }
            _ => Err(ProtocolError::MethodNotFound {
                method: method.as_str().to_owned(),
            }),
        }
    }

    async fn handle_notification(&self, notification: Notification) -> Result<(), ProtocolError> {
        debug!(
            method = %notification.method,
            "dropping unhandled notification"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum GraphDirection {
    Back,
    Forward,
}

fn map_search_error(err: Error) -> ProtocolError {
    let message = err.to_string();
    if message.contains("empty search query") {
        ProtocolError::invalid_params("search query must not be empty")
    } else if message.contains("requires Arrowhead to be built with the `vector-lancedb` feature") {
        ProtocolError::custom(ErrorCode::ToolDisabled, message, None)
    } else {
        ProtocolError::internal(message)
    }
}

fn map_note_load_error(err: Error, note_id: &str) -> ProtocolError {
    if err
        .chain()
        .any(|cause| cause.to_string().contains("not found"))
    {
        ProtocolError::invalid_params(format!("note {note_id} not found in vault"))
    } else {
        ProtocolError::internal(err.to_string())
    }
}

fn map_note_write_error(err: Error, note_id: &str) -> ProtocolError {
    if err
        .chain()
        .any(|cause| cause.to_string().contains("invalid note id"))
    {
        ProtocolError::invalid_params(format!("invalid note id {note_id}"))
    } else {
        ProtocolError::internal(err.to_string())
    }
}

fn resolve_note_id(params: &NoteCreateParams) -> Result<String, ProtocolError> {
    if let Some(id) = &params.note_id {
        clean_note_id(id)
    } else if let Some(title) = &params.title {
        clean_note_id(title)
    } else {
        Err(ProtocolError::invalid_params(
            "noteId or title is required to create a note",
        ))
    }
}

fn clean_note_id(raw: &str) -> Result<String, ProtocolError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ProtocolError::invalid_params("note id must not be empty"));
    }

    let without_ext = trimmed.strip_suffix(".md").unwrap_or(trimmed);
    let candidate = without_ext.trim_matches(|c| c == '/' || c == '\\').trim();

    if candidate.is_empty() {
        return Err(ProtocolError::invalid_params("note id must not be empty"));
    }

    Ok(candidate.replace('\\', "/"))
}

fn merge_metadata_map(metadata: &mut MetadataMap, updates: Option<MetadataMap>) {
    if let Some(map) = updates {
        for (key, value) in map {
            if value.is_null() {
                metadata.remove(&key);
            } else {
                metadata.insert(key, value);
            }
        }
    }
}

fn create_note_in_vault(
    vault: Arc<Vault>,
    note_id: String,
    params: NoteCreateParams,
) -> Result<NoteRecord, ProtocolError> {
    let NoteCreateParams {
        note_id: _,
        title,
        category,
        content,
        metadata,
    } = params;

    let path = vault
        .note_file_path(&note_id)
        .map_err(|err| ProtocolError::invalid_params(err.to_string()))?;
    if path.exists() {
        return Err(ProtocolError::invalid_params(format!(
            "note {note_id} already exists"
        )));
    }

    let mut metadata_map = metadata.unwrap_or_default();
    if let Some(title_value) = &title {
        metadata_map.insert("title".to_string(), Value::String(title_value.clone()));
    }
    if let Some(category_value) = &category {
        metadata_map.insert(
            "category".to_string(),
            Value::String(category_value.clone()),
        );
    }
    if !metadata_map.contains_key("title") {
        let fallback = title.clone().unwrap_or_else(|| note_id.clone());
        metadata_map.insert("title".to_string(), Value::String(fallback));
    }

    let body = content.unwrap_or_default();
    vault
        .write_note(&note_id, &metadata_map, &body)
        .map_err(|err| map_note_write_error(err, &note_id))?;
    vault
        .load_note(&note_id)
        .map_err(|err| map_note_load_error(err, &note_id))
}

fn update_note_in_vault(
    vault: Arc<Vault>,
    params: NoteUpdateParams,
) -> Result<NoteRecord, ProtocolError> {
    let NoteUpdateParams {
        note_id,
        title,
        content,
        metadata,
    } = params;

    let record = vault
        .load_note(&note_id)
        .map_err(|err| map_note_load_error(err, &note_id))?;
    let mut metadata_map = record.metadata.clone();

    if let Some(title_value) = title {
        if title_value.trim().is_empty() {
            metadata_map.remove("title");
        } else {
            metadata_map.insert("title".to_string(), Value::String(title_value));
        }
    }

    merge_metadata_map(&mut metadata_map, metadata);

    let body = content.unwrap_or(record.content);
    vault
        .write_note(&note_id, &metadata_map, &body)
        .map_err(|err| map_note_write_error(err, &note_id))?;
    vault
        .load_note(&note_id)
        .map_err(|err| map_note_load_error(err, &note_id))
}

fn delete_note_in_vault(
    vault: Arc<Vault>,
    params: NoteDeleteParams,
) -> Result<NoteDeletePayload, ProtocolError> {
    let NoteDeleteParams {
        note_id,
        confirm: _,
    } = params;
    let path = vault
        .note_file_path(&note_id)
        .map_err(|err| ProtocolError::invalid_params(err.to_string()))?;

    if !path.exists() {
        return Err(ProtocolError::invalid_params(format!(
            "note {note_id} does not exist"
        )));
    }

    fs::remove_file(&path).map_err(|err| {
        if err.kind() == ErrorKind::NotFound {
            ProtocolError::invalid_params(format!("note {note_id} does not exist"))
        } else {
            ProtocolError::internal(format!(
                "failed to delete note file {}: {err}",
                path.display()
            ))
        }
    })?;

    let root = vault.paths().root.clone();
    let pruned = prune_empty_directories(&root, path.parent())?;

    Ok(NoteDeletePayload {
        note_id,
        deleted: true,
        pruned_directories: pruned,
    })
}

fn prune_empty_directories(
    root: &Path,
    start: Option<&Path>,
) -> Result<Vec<PathBuf>, ProtocolError> {
    let mut pruned = Vec::new();
    let mut current = match start {
        Some(path) => path.to_path_buf(),
        None => return Ok(pruned),
    };

    while current.starts_with(root) && current != root {
        match fs::read_dir(&current) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    break;
                }
            }
            Err(err) => {
                return Err(ProtocolError::internal(format!(
                    "failed to inspect directory {}: {err}",
                    current.display()
                )));
            }
        }

        fs::remove_dir(&current).map_err(|err| {
            ProtocolError::internal(format!(
                "failed to remove directory {}: {err}",
                current.display()
            ))
        })?;

        let relative = current.strip_prefix(root).unwrap_or(&current).to_path_buf();
        pruned.push(relative);

        if let Some(parent) = current.parent() {
            current = parent.to_path_buf();
        } else {
            break;
        }
    }

    Ok(pruned)
}

fn build_daemon_status_payload(status: DeamonStatus) -> DaemonStatusPayload {
    let activity = summarise_activity(&status.activity);
    let queued_jobs = (status.activity.queued_jobs > 0).then_some(status.activity.queued_jobs);

    DaemonStatusPayload {
        updated_at: status.updated_at,
        indexed_notes: status.indexed_notes,
        error_notes: status.error_notes,
        activity,
        queued_jobs,
    }
}

fn summarise_activity(activity: &ActivityStatus) -> Option<String> {
    if let Some(description) = &activity.description {
        if !description.is_empty() {
            return Some(description.clone());
        }
    }

    match activity.state {
        ActivityState::Idle => Some("idle".to_string()),
        ActivityState::Indexing => activity
            .note_id
            .as_ref()
            .map(|id| format!("indexing {id}"))
            .or_else(|| Some("indexing".to_string())),
        ActivityState::Removing => Some("removing stale entries".to_string()),
        ActivityState::Downloading => Some("downloading assets".to_string()),
        ActivityState::Faulted => Some("daemon faulted".to_string()),
    }
}

fn make_tool_example(name: &str, description: &str, request: Value) -> ToolExample {
    ToolExample {
        name: name.to_string(),
        description: Some(description.to_string()),
        request,
    }
}
