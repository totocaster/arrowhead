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
use chrono::{DateTime, FixedOffset, NaiveDate};
use serde_json::{Value, json};
use tokio::task;
use tracing::debug;

use arrowhead_core::{
    MetadataMap, MetricCreateRequest, MetricUpdateRequest, MonthContextSelector, NoteRecord,
    PatchValue, Vault, WeekContextSelector,
    status::{ActivityState, ActivityStatus, DaemonStatus},
};

use crate::{
    protocol::{ErrorCode, Notification, Params, ProtocolError, Request},
    runtime::McpRuntime,
    tools::{
        CallToolParams, CallToolResultPayload, ContextChangedParams, ContextDayParams,
        ContextMetricParams, ContextMonthParams, ContextNoteParams, ContextSourceParams,
        ContextWeekParams, DaemonStatusPayload, GraphContextPayload, GraphLinksPayload,
        GraphNoteParams, GraphOrphansPayload, GraphUnresolvedPayload, ImplementationDescriptor,
        InitializeParams, InitializeResultPayload, LinkEdgePayload, MetricCreateParams,
        MetricDeleteParams, MetricDeletePayload, MetricFileCreateParams, MetricFileCreatePayload,
        MetricFileDeleteParams, MetricFileDeletePayload, MetricFileRenameParams,
        MetricFileRenamePayload, MetricReadParams, MetricReadPayload, MetricUpdateParams,
        MetricsFilesPayload, MetricsSearchResultsPayload, NoteContentPayload, NoteCreateParams,
        NoteDeleteParams, NoteDeletePayload, NoteListItem, NoteMetadataParams, NoteMetadataPayload,
        NoteReadParams, NoteUpdateParams, NotesListParams, NotesListPayload, OrphanNotePayload,
        RelatedNotesParams, SearchParams, SearchResultPayload, SearchResultsPayload,
        ServerCapabilitiesPayload, ToolCapabilityPayload, ToolDescriptor, ToolsListPayload,
        VaultStatsParams,
    },
    transport::MessageHandler,
};

/// Dispatches MCP method calls to concrete implementations.
#[derive(Debug, Clone)]
pub struct HandlerRegistry {
    runtime: Arc<McpRuntime>,
}

const SUPPORTED_PROTOCOL_VERSION: &str = "2025-06-18";

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

    async fn handle_context_note(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: ContextNoteParams = request.params.deserialize()?;
        let payload = self
            .runtime
            .context_service()
            .note(&params.note_id, params.note_limit, params.metric_limit)
            .await
            .map_err(map_context_error)?;
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise context payload: {err}"))
        })
    }

    async fn handle_context_day(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: ContextDayParams = request.params.deserialize()?;
        let payload = self
            .runtime
            .context_service()
            .day(&params.day, params.note_limit, params.metric_limit)
            .await
            .map_err(map_context_error)?;
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise context payload: {err}"))
        })
    }

    async fn handle_context_week(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: ContextWeekParams = request.params.deserialize()?;
        let payload = self
            .runtime
            .context_service()
            .week(
                resolve_week_selector(&params)?,
                params.note_limit,
                params.metric_limit,
            )
            .await
            .map_err(map_context_error)?;
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise context payload: {err}"))
        })
    }

    async fn handle_context_month(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: ContextMonthParams = request.params.deserialize()?;
        let payload = self
            .runtime
            .context_service()
            .month(
                resolve_month_selector(&params)?,
                params.note_limit,
                params.metric_limit,
            )
            .await
            .map_err(map_context_error)?;
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise context payload: {err}"))
        })
    }

    async fn handle_context_changed(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: ContextChangedParams = request.params.deserialize()?;
        let payload = self
            .runtime
            .context_service()
            .changed(
                params.days.unwrap_or(7),
                params.note_limit,
                params.metric_limit,
            )
            .await
            .map_err(map_context_error)?;
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise context payload: {err}"))
        })
    }

    async fn handle_context_metric(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: ContextMetricParams = request.params.deserialize()?;
        let payload = self
            .runtime
            .context_service()
            .metric(
                &params.metric,
                params.range.as_deref(),
                params.note_limit,
                params.metric_limit,
            )
            .await
            .map_err(map_context_error)?;
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise context payload: {err}"))
        })
    }

    async fn handle_context_source(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: ContextSourceParams = request.params.deserialize()?;
        let payload = self
            .runtime
            .context_service()
            .source(
                &params.source,
                params.range.as_deref(),
                params.note_limit,
                params.metric_limit,
            )
            .await
            .map_err(map_context_error)?;
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise context payload: {err}"))
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

    async fn handle_search_semantic(&self, request: Request) -> Result<Value, ProtocolError> {
        if !self.runtime.semantic_search_enabled() {
            return Err(ProtocolError::custom(
                ErrorCode::ToolDisabled,
                "semantic search is disabled because embeddings are not initialised.",
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

    async fn handle_search_hybrid(&self, request: Request) -> Result<Value, ProtocolError> {
        if !self.runtime.semantic_search_enabled() {
            return Err(ProtocolError::custom(
                ErrorCode::ToolDisabled,
                "hybrid search is disabled because embeddings are not initialised.",
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

    async fn handle_metrics_list_files(&self) -> Result<Value, ProtocolError> {
        let service = self.runtime.metrics_service().clone();
        let files = service.list_files().await.map_err(|err| {
            ProtocolError::internal(format!("failed to list indexed metrics files: {err}"))
        })?;
        let payload = MetricsFilesPayload { files };
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise metrics files: {err}"))
        })
    }

    async fn handle_metrics_read(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: MetricReadParams = request.params.deserialize()?;
        let service = self.runtime.metrics_service().clone();
        let metric_id = params.metric_id.clone();
        let record = service.read_record(&metric_id).await.map_err(|err| {
            ProtocolError::internal(format!("failed to load metric record {metric_id}: {err}"))
        })?;
        let Some(record) = record else {
            return Err(ProtocolError::invalid_params(format!(
                "metric {metric_id} was not found in the index. Run `arrowhead index start` to refresh metrics data."
            )));
        };

        let payload = MetricReadPayload { record };
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise metric record: {err}"))
        })
    }

    async fn handle_metrics_search(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: SearchParams = request.params.deserialize()?;
        let service = self.runtime.metrics_service().clone();
        let results = service
            .search(&params.query, params.limit)
            .await
            .map_err(map_metrics_search_error)?;
        let payload = MetricsSearchResultsPayload {
            total: results.len(),
            results,
        };
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise metrics search results: {err}"))
        })
    }

    async fn handle_metrics_create(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: MetricCreateParams = request.params.deserialize()?;
        let service = self.runtime.metrics_mutation_service().clone();
        let record = service
            .create(build_metric_create_request(params)?)
            .await
            .map_err(map_metrics_mutation_error)?;
        let payload = MetricReadPayload { record };
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise created metric record: {err}"))
        })
    }

    async fn handle_metrics_update(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: MetricUpdateParams = request.params.deserialize()?;
        let service = self.runtime.metrics_mutation_service().clone();
        let record = service
            .update(build_metric_update_request(params)?)
            .await
            .map_err(map_metrics_mutation_error)?;
        let payload = MetricReadPayload { record };
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise updated metric record: {err}"))
        })
    }

    async fn handle_metrics_delete(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: MetricDeleteParams = request.params.deserialize()?;
        if !params.confirm {
            return Err(ProtocolError::invalid_params(
                "set `confirm: true` to delete a metric record",
            ));
        }

        let service = self.runtime.metrics_mutation_service().clone();
        let deleted = service
            .delete(&params.metric_id)
            .await
            .map_err(map_metrics_mutation_error)?;
        let payload = MetricDeletePayload { deleted };
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise deleted metric record: {err}"))
        })
    }

    async fn handle_metrics_create_file(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: MetricFileCreateParams = request.params.deserialize()?;
        let service = self.runtime.metrics_mutation_service().clone();
        let file = service
            .create_file(&params.path)
            .await
            .map_err(map_metrics_mutation_error)?;
        let payload = MetricFileCreatePayload { file };
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise created metrics file: {err}"))
        })
    }

    async fn handle_metrics_rename_file(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: MetricFileRenameParams = request.params.deserialize()?;
        let service = self.runtime.metrics_mutation_service().clone();
        let file = service
            .rename_file(&params.source_path, &params.destination_path)
            .await
            .map_err(map_metrics_mutation_error)?;
        let payload = MetricFileRenamePayload { file };
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise renamed metrics file: {err}"))
        })
    }

    async fn handle_metrics_delete_file(&self, request: Request) -> Result<Value, ProtocolError> {
        let params: MetricFileDeleteParams = request.params.deserialize()?;
        if !params.confirm {
            return Err(ProtocolError::invalid_params(
                "set `confirm: true` to delete a metrics file",
            ));
        }

        let service = self.runtime.metrics_mutation_service().clone();
        let file = service
            .delete_file(&params.path)
            .await
            .map_err(map_metrics_mutation_error)?;
        let payload = MetricFileDeletePayload { file };
        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise deleted metrics file: {err}"))
        })
    }

    async fn handle_vault_status(&self) -> Result<Value, ProtocolError> {
        let status = self.daemon_status().await?;
        let mut value = serde_json::to_value(&status).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise vault status: {err}"))
        })?;

        if let Value::Object(ref mut map) = value {
            let activity =
                summarise_activity(&status.activity).unwrap_or_else(|| "idle".to_string());
            let message = format!(
                "Daemon activity: {activity}. Indexed {} notes ({} errors).",
                status.indexed_notes, status.error_notes
            );
            map.insert("summary".to_string(), json!(message));
        }

        Ok(value)
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
        let limit = params.limit;
        let strategy = params.strategy;

        let payload = if let Some(note_id) = params.note_id.as_deref() {
            let trimmed = note_id.trim();
            if trimmed.is_empty() {
                return Err(ProtocolError::invalid_params(
                    "noteId must not be empty when provided",
                ));
            }
            self.runtime
                .compute_related_notes(trimmed, limit, strategy)
                .await
        } else if let Some(query) = params.query.as_deref() {
            let trimmed = query.trim();
            if trimmed.is_empty() {
                return Err(ProtocolError::invalid_params(
                    "query must not be empty when provided",
                ));
            }
            self.runtime
                .compute_related_notes_for_query(trimmed, limit, strategy)
                .await
        } else {
            return Err(ProtocolError::invalid_params(
                "provide either noteId or query to compute related notes",
            ));
        }
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

    async fn handle_initialize(&self, request: Request) -> Result<Value, ProtocolError> {
        let InitializeParams {
            protocol_version: requested_version,
            capabilities: _client_capabilities,
            client_info,
        } = request.params.deserialize()?;
        let tool_capabilities = ToolCapabilityPayload {
            list_changed: Some(false),
        };

        let mut arrowhead_caps = json!({
            "requiresVaultConventions": true
        });
        if self.runtime.semantic_search_enabled() {
            if let Some(object) = arrowhead_caps.as_object_mut() {
                object.insert("semanticSearch".to_string(), Value::Bool(true));
            }
        }
        let experimental = Some(json!({
            "arrowhead": arrowhead_caps
        }));

        let capabilities = ServerCapabilitiesPayload {
            tools: Some(tool_capabilities),
            experimental,
            ..ServerCapabilitiesPayload::default()
        };

        let negotiated_version = if requested_version == SUPPORTED_PROTOCOL_VERSION {
            requested_version
        } else {
            SUPPORTED_PROTOCOL_VERSION.to_string()
        };

        let client_label = client_info.title.as_deref().unwrap_or(&client_info.name);

        let daemon_status = self
            .runtime
            .cached_daemon_status()
            .await
            .map(build_daemon_status_payload);

        let payload = InitializeResultPayload {
            protocol_version: negotiated_version,
            capabilities,
            server_info: ImplementationDescriptor {
                name: "arrowhead-mcp".to_string(),
                title: Some("Arrowhead MCP".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            instructions: Some(format!(
                "Arrowhead indexes your Obsidian vault. Call mcp.discovery.get_vault_conventions \
                 before creating or editing notes so agents honour local naming rules. Use \
                 tools/list to discover search, graph, and note-management tools. Ensure the \
                 arrowhead daemon is running for up-to-date data. Client: {client_label}."
            )),
            daemon_status,
        };

        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise initialize payload: {err}"))
        })
    }

    async fn handle_tools_list(&self, _request: Request) -> Result<Value, ProtocolError> {
        let payload = ToolsListPayload {
            tools: self.build_tool_descriptors(),
            next_cursor: None,
        };

        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise tools list: {err}"))
        })
    }

    async fn handle_tools_call(&self, request: Request) -> Result<Value, ProtocolError> {
        let CallToolParams {
            name, arguments, ..
        } = request.params.deserialize()?;
        let method = resolve_tool_method(&name)
            .map(str::to_owned)
            .or_else(|| {
                if name.contains('.') {
                    Some(name.clone())
                } else {
                    None
                }
            })
            .ok_or_else(|| ProtocolError::MethodNotFound {
                method: name.clone(),
            })?;

        let tool_arguments = Value::Object(arguments);
        let tool_params = Params::new(tool_arguments)
            .map_err(|err| ProtocolError::invalid_params(err.to_string()))?;
        let tool_request = Request::new(request.id.clone(), method, tool_params);
        let result = self.handle_named_tool(tool_request).await?;
        let payload = match name.as_str() {
            "notes_delete" => {
                let message = result.as_object().and_then(|map| {
                    if map.get("deleted").and_then(Value::as_bool) == Some(true) {
                        let note_id = map.get("noteId").and_then(Value::as_str).unwrap_or("note");
                        Some(format!("Deleted note {note_id}."))
                    } else {
                        None
                    }
                });
                CallToolResultPayload::from_value_with_message(result, message)
            }
            "metrics_delete" => {
                let message = result
                    .get("deleted")
                    .and_then(Value::as_object)
                    .and_then(|deleted| deleted.get("metricId"))
                    .and_then(Value::as_str)
                    .map(|metric_id| format!("Deleted metric {metric_id}."));
                CallToolResultPayload::from_value_with_message(result, message)
            }
            "metrics_create_file" => {
                let message = result
                    .get("file")
                    .and_then(Value::as_object)
                    .and_then(|file| file.get("relativePath"))
                    .and_then(Value::as_str)
                    .map(|path| format!("Created metrics file {path}."));
                CallToolResultPayload::from_value_with_message(result, message)
            }
            "metrics_rename_file" => {
                let message = result
                    .get("file")
                    .and_then(Value::as_object)
                    .and_then(|file| {
                        let source = file.get("sourcePath")?.as_str()?;
                        let destination = file.get("destinationPath")?.as_str()?;
                        Some(format!("Renamed metrics file {source} -> {destination}."))
                    });
                CallToolResultPayload::from_value_with_message(result, message)
            }
            "metrics_delete_file" => {
                let message = result
                    .get("file")
                    .and_then(Value::as_object)
                    .and_then(|file| file.get("relativePath"))
                    .and_then(Value::as_str)
                    .map(|path| format!("Deleted metrics file {path}."));
                CallToolResultPayload::from_value_with_message(result, message)
            }
            "context_get_day"
            | "context_get_week"
            | "context_get_month"
            | "context_get_changed"
            | "context_get_note"
            | "context_get_metric"
            | "context_get_source" => {
                let message =
                    result
                        .get("summary")
                        .and_then(Value::as_object)
                        .and_then(|summary| {
                            let kind = summary.get("kind")?.as_str()?;
                            let target = summary.get("target")?.as_str()?;
                            Some(format!("Loaded {kind} context for {target}."))
                        });
                CallToolResultPayload::from_value_with_message(result, message)
            }
            "vault_status" => {
                let message = result
                    .get("summary")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string());
                CallToolResultPayload::from_value_with_message(result, message)
            }
            _ => CallToolResultPayload::from_value(result),
        };

        serde_json::to_value(payload).map_err(|err| {
            ProtocolError::internal(format!("failed to serialise tool result: {err}"))
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn handle_named_tool(&self, request: Request) -> Result<Value, ProtocolError> {
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
            "mcp.context.get_day" => self.handle_context_day(request).await,
            "mcp.context.get_week" => self.handle_context_week(request).await,
            "mcp.context.get_month" => self.handle_context_month(request).await,
            "mcp.context.get_changed" => self.handle_context_changed(request).await,
            "mcp.context.get_note" => self.handle_context_note(request).await,
            "mcp.context.get_metric" => self.handle_context_metric(request).await,
            "mcp.context.get_source" => self.handle_context_source(request).await,
            "mcp.search.fts" => self.handle_search_fts(request).await,
            "mcp.search.semantic" => self.handle_search_semantic(request).await,
            "mcp.search.hybrid" => self.handle_search_hybrid(request).await,
            "mcp.metrics.list_files" => self.handle_metrics_list_files().await,
            "mcp.metrics.read" => self.handle_metrics_read(request).await,
            "mcp.metrics.search" => self.handle_metrics_search(request).await,
            "mcp.metrics.create" => self.handle_metrics_create(request).await,
            "mcp.metrics.update" => self.handle_metrics_update(request).await,
            "mcp.metrics.delete" => self.handle_metrics_delete(request).await,
            "mcp.metrics.create_file" => self.handle_metrics_create_file(request).await,
            "mcp.metrics.rename_file" => self.handle_metrics_rename_file(request).await,
            "mcp.metrics.delete_file" => self.handle_metrics_delete_file(request).await,
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
                self.handle_discovery_vault_conventions().await
            }
            _ => Err(ProtocolError::MethodNotFound {
                method: method.as_str().to_owned(),
            }),
        }
    }

    fn build_tool_descriptors(&self) -> Vec<ToolDescriptor> {
        let empty_schema = || {
            json!({
                "type": "object",
                "description": "This tool does not accept any parameters.",
                "properties": {},
                "additionalProperties": false
            })
        };

        let note_id_examples = json!(["Projects/Test Plan", "2025-10-26-F130208"]);
        let metadata_examples = json!([
            {
                "category": "project",
                "status": "active",
                "tags": ["ai", "tools"]
            }
        ]);
        let query_examples = json!(["project status:active", "\"exact phrase\"", "tags:ai"]);

        let metadata_map_schema = || {
            json!({
                "type": "object",
                "description": "Frontmatter metadata as key-value pairs.",
                "additionalProperties": true,
                "examples": metadata_examples.clone()
            })
        };
        let date_time_schema = |description: &str| {
            json!({
                "type": "string",
                "format": "date-time",
                "description": description
            })
        };
        let path_schema = |description: &str| {
            json!({
                "type": "string",
                "description": description
            })
        };

        let note_id_field_schema = json!({
            "type": "string",
            "description": "Vault-relative note identifier without the .md extension.",
            "examples": note_id_examples
        });
        let metric_id_field_schema = json!({
            "type": "string",
            "description": "Stable metric id or `metric:<id>` reference.",
            "examples": ["01JV7RK8Q4X60M0E2N0A6QK61V", "metric:01JV7RK8Q4X60M0E2N0A6QK61V"]
        });

        let note_id_schema = json!({
            "type": "object",
            "description": "Parameters that identify a single note.",
            "additionalProperties": false,
            "properties": {
                "noteId": note_id_field_schema.clone()
            },
            "required": ["noteId"]
        });
        let search_schema = json!({
            "type": "object",
            "description": "Parameters for Arrowhead search tools.",
            "additionalProperties": false,
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Query string to evaluate. Supports field:value metadata filters and quoted phrases for exact matches.",
                    "examples": query_examples
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 10,
                    "description": "Maximum number of results to return. Defaults to 10 when omitted.",
                    "examples": [10]
                }
            },
            "required": ["query"]
        });
        let metric_id_schema = json!({
            "type": "object",
            "description": "Parameters that identify a single metric record.",
            "additionalProperties": false,
            "properties": {
                "metricId": metric_id_field_schema.clone()
            },
            "required": ["metricId"]
        });
        let metric_create_schema = json!({
            "type": "object",
            "description": "Parameters for creating a metric record in a vault metrics file.",
            "additionalProperties": false,
            "properties": {
                "filePath": path_schema("Optional target metrics file relative to the vault root. Defaults to the resolved metrics write file."),
                "id": {
                    "type": "string",
                    "description": "Optional stable metric id. Arrowhead generates one when omitted."
                },
                "ts": {
                    "type": "string",
                    "format": "date-time",
                    "description": "RFC 3339 timestamp recorded for the metric event."
                },
                "key": {
                    "type": "string",
                    "description": "Metric key such as `body.weight`."
                },
                "value": {
                    "type": "number",
                    "description": "Numeric metric value."
                },
                "source": {
                    "type": "string",
                    "description": "Source that produced the metric."
                },
                "date": {
                    "type": "string",
                    "description": "Optional YYYY-MM-DD date bucket."
                },
                "unit": {
                    "type": "string",
                    "description": "Optional unit string."
                },
                "originId": {
                    "type": "string",
                    "description": "Optional provenance id."
                },
                "note": {
                    "type": "string",
                    "description": "Optional human-authored note."
                },
                "context": {
                    "type": "object",
                    "description": "Optional structured context object.",
                    "additionalProperties": true
                },
                "tags": {
                    "type": "array",
                    "description": "Optional tags attached to the metric row.",
                    "items": { "type": "string" }
                }
            },
            "required": ["ts", "key", "value", "source"]
        });
        let metric_update_schema = json!({
            "type": "object",
            "description": "Parameters for updating a metric record by stable id.",
            "additionalProperties": false,
            "properties": {
                "metricId": metric_id_field_schema.clone(),
                "ts": {
                    "type": "string",
                    "format": "date-time",
                    "description": "Optional replacement RFC 3339 timestamp."
                },
                "key": {
                    "type": "string",
                    "description": "Optional replacement metric key."
                },
                "value": {
                    "type": "number",
                    "description": "Optional replacement numeric value."
                },
                "source": {
                    "type": "string",
                    "description": "Optional replacement source."
                },
                "date": {
                    "type": "string",
                    "description": "Optional replacement YYYY-MM-DD date."
                },
                "clearDate": {
                    "type": "boolean",
                    "default": false,
                    "description": "Clear the `date` field."
                },
                "unit": {
                    "type": "string",
                    "description": "Optional replacement unit."
                },
                "clearUnit": {
                    "type": "boolean",
                    "default": false,
                    "description": "Clear the `unit` field."
                },
                "originId": {
                    "type": "string",
                    "description": "Optional replacement provenance id."
                },
                "clearOriginId": {
                    "type": "boolean",
                    "default": false,
                    "description": "Clear the `originId` field."
                },
                "note": {
                    "type": "string",
                    "description": "Optional replacement note."
                },
                "clearNote": {
                    "type": "boolean",
                    "default": false,
                    "description": "Clear the `note` field."
                },
                "context": {
                    "type": "object",
                    "description": "Optional replacement context object.",
                    "additionalProperties": true
                },
                "clearContext": {
                    "type": "boolean",
                    "default": false,
                    "description": "Clear the `context` field."
                },
                "tags": {
                    "type": "array",
                    "description": "Replace the tag list with these values.",
                    "items": { "type": "string" }
                },
                "clearTags": {
                    "type": "boolean",
                    "default": false,
                    "description": "Clear the tag list entirely."
                }
            },
            "required": ["metricId"]
        });
        let metric_delete_schema = json!({
            "type": "object",
            "description": "Parameters for deleting a metric record.",
            "additionalProperties": false,
            "properties": {
                "metricId": metric_id_field_schema.clone(),
                "confirm": {
                    "type": "boolean",
                    "default": false,
                    "description": "Safety confirmation flag; must be true to delete."
                }
            },
            "required": ["metricId"]
        });
        let metric_file_create_schema = json!({
            "type": "object",
            "description": "Parameters for creating an empty metrics file.",
            "additionalProperties": false,
            "properties": {
                "path": path_schema("Target metrics file relative to the vault root.")
            },
            "required": ["path"]
        });
        let metric_file_rename_schema = json!({
            "type": "object",
            "description": "Parameters for renaming a metrics file.",
            "additionalProperties": false,
            "properties": {
                "sourcePath": path_schema("Existing metrics file relative to the vault root."),
                "destinationPath": path_schema("New metrics file relative to the vault root.")
            },
            "required": ["sourcePath", "destinationPath"]
        });
        let metric_file_delete_schema = json!({
            "type": "object",
            "description": "Parameters for deleting a metrics file.",
            "additionalProperties": false,
            "properties": {
                "path": path_schema("Metrics file relative to the vault root."),
                "confirm": {
                    "type": "boolean",
                    "default": false,
                    "description": "Safety confirmation flag; must be true to delete."
                }
            },
            "required": ["path"]
        });
        let context_day_schema = json!({
            "type": "object",
            "description": "Parameters for building day context.",
            "additionalProperties": false,
            "properties": {
                "day": {
                    "type": "string",
                    "description": "Day to inspect in YYYY-MM-DD format."
                },
                "noteLimit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional limit for related notes."
                },
                "metricLimit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional limit for metric records."
                }
            },
            "required": ["day"]
        });
        let context_week_schema = json!({
            "type": "object",
            "description": "Parameters for building week context.",
            "additionalProperties": false,
            "properties": {
                "day": {
                    "type": "string",
                    "description": "Optional day inside the requested week in YYYY-MM-DD format."
                },
                "this": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true, inspect the current week."
                },
                "last": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true, inspect the previous week."
                },
                "noteLimit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional limit for related notes."
                },
                "metricLimit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional limit for metric records."
                }
            }
        });
        let context_month_schema = json!({
            "type": "object",
            "description": "Parameters for building month context.",
            "additionalProperties": false,
            "properties": {
                "day": {
                    "type": "string",
                    "description": "Optional day inside the requested month in YYYY-MM-DD format."
                },
                "this": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true, inspect the current month."
                },
                "last": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true, inspect the previous month."
                },
                "noteLimit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional limit for related notes."
                },
                "metricLimit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional limit for metric records."
                }
            }
        });
        let context_changed_schema = json!({
            "type": "object",
            "description": "Parameters for building recently changed context.",
            "additionalProperties": false,
            "properties": {
                "days": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Trailing number of days to inspect. Defaults to 7."
                },
                "noteLimit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional limit for related notes."
                },
                "metricLimit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional limit for metric records."
                }
            }
        });
        let context_note_schema = json!({
            "type": "object",
            "description": "Parameters for building note context.",
            "additionalProperties": false,
            "properties": {
                "noteId": note_id_field_schema.clone(),
                "noteLimit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional limit for related notes."
                },
                "metricLimit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional limit for metric records."
                }
            },
            "required": ["noteId"]
        });
        let context_metric_schema = json!({
            "type": "object",
            "description": "Parameters for building metric context from a metric id or key.",
            "additionalProperties": false,
            "properties": {
                "metric": {
                    "type": "string",
                    "description": "Metric id (`metric:<id>` or raw id) or metric key."
                },
                "range": {
                    "type": "string",
                    "description": "Optional metrics date range such as `past30d` or `2026-04-01..2026-04-15`."
                },
                "noteLimit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional limit for related notes."
                },
                "metricLimit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional limit for metric records."
                }
            },
            "required": ["metric"]
        });
        let context_source_schema = json!({
            "type": "object",
            "description": "Parameters for building source context.",
            "additionalProperties": false,
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Metrics source identifier."
                },
                "range": {
                    "type": "string",
                    "description": "Optional metrics date range such as `past30d` or `2026-04-01..2026-04-15`."
                },
                "noteLimit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional limit for related notes."
                },
                "metricLimit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional limit for metric records."
                }
            },
            "required": ["source"]
        });
        let notes_list_schema = json!({
            "type": "object",
            "description": "Optional filters when listing notes from the vault.",
            "additionalProperties": false,
            "properties": {
                "idsOnly": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true, return only note identifiers.",
                    "examples": [true]
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional maximum number of notes to include.",
                    "examples": [25]
                }
            }
        });
        let note_create_schema = json!({
            "type": "object",
            "description": "Parameters for creating a new note. Arrowhead requires either noteId or title, and falls back to title-based IDs when noteId is omitted.",
            "additionalProperties": false,
            "properties": {
                "noteId": note_id_field_schema.clone(),
                "title": {
                    "type": "string",
                    "description": "Optional display title written into frontmatter.",
                    "examples": ["Arrowhead CLI Roadmap"]
                },
                "category": {
                    "type": "string",
                    "description": "Optional helper that prefixes the note ID with the provided folder.",
                    "examples": ["Projects"]
                },
                "content": {
                    "type": "string",
                    "description": "Markdown body for the note. Defaults to an empty document.",
                    "default": "",
                    "examples": ["# Arrowhead CLI\n\n- [ ] Ship MCP tooling"]
                },
                "metadata": metadata_map_schema()
            }
        });
        let note_update_schema = json!({
            "type": "object",
            "description": "Partial update for an existing note. Omitted fields are left unchanged.",
            "additionalProperties": false,
            "properties": {
                "noteId": note_id_field_schema.clone(),
                "title": {
                    "type": "string",
                    "description": "Replacement title written into frontmatter.",
                    "examples": ["Updated Arrowhead Roadmap"]
                },
                "content": {
                    "type": "string",
                    "description": "Replacement Markdown body.",
                    "examples": ["# Updated Plan\n\nContent goes here."]
                },
                "metadata": metadata_map_schema()
            },
            "required": ["noteId"]
        });
        let note_delete_schema = json!({
            "type": "object",
            "description": "Parameters required to delete a note. Deletion is refused unless confirm is true.",
            "additionalProperties": false,
            "properties": {
                "noteId": note_id_field_schema.clone(),
                "confirm": {
                    "type": "boolean",
                    "const": true,
                    "description": "Set to true to confirm permanent deletion.",
                    "examples": [true]
                }
            },
            "required": ["noteId", "confirm"]
        });
        let related_notes_schema = json!({
            "type": "object",
            "description": "Return notes related to an anchor note or free-form query. Provide either noteId or query; Arrowhead rejects requests that omit both.",
            "additionalProperties": false,
            "properties": {
                "noteId": note_id_field_schema.clone(),
                "query": {
                    "type": "string",
                    "description": "Natural language prompt when no anchor note is supplied.",
                    "examples": ["project kickoff checklist"]
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "default": 5,
                    "description": "Maximum number of related notes to return. Defaults to 5.",
                    "examples": [5]
                },
                "strategy": {
                    "type": "string",
                    "enum": ["auto", "semantic", "graph", "hybrid"],
                    "default": "auto",
                    "description": "Strategy hint controlling which signals to prioritise."
                }
            }
        });

        let link_edge_schema = json!({
            "type": "object",
            "description": "WikiLink edge describing a relationship between notes.",
            "required": ["source", "raw", "reason"],
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Identifier of the note containing the outbound link.",
                    "examples": ["Projects/Test Plan"]
                },
                "target": {
                    "type": "string",
                    "description": "Resolved identifier of the linked note when available."
                },
                "raw": {
                    "type": "string",
                    "description": "Raw link text as written in the source note."
                },
                "displayText": {
                    "type": "string",
                    "description": "Optional alias captured from [[target|alias]] syntax."
                },
                "heading": {
                    "type": "string",
                    "description": "Optional heading fragment captured from [[target#Heading]]."
                },
                "reason": {
                    "type": "string",
                    "description": "Explanation of how the link target was resolved."
                }
            },
            "additionalProperties": false
        });
        let metric_issue_schema = json!({
            "type": "object",
            "description": "Validation issue attached to a metric record.",
            "required": ["severity", "code", "message"],
            "properties": {
                "severity": {
                    "type": "string",
                    "enum": ["warning", "error"],
                    "description": "Issue severity."
                },
                "code": {
                    "type": "string",
                    "description": "Stable validation issue code."
                },
                "field": {
                    "type": "string",
                    "description": "Field associated with the issue, when known."
                },
                "message": {
                    "type": "string",
                    "description": "Human-readable validation message."
                }
            },
            "additionalProperties": false
        });
        let metric_record_entry_schema = json!({
            "type": "object",
            "description": "Indexed metric record with validation metadata.",
            "required": ["sourceFile", "sourceLine", "record", "rawLine", "validationStatus", "issues"],
            "properties": {
                "sourceFile": path_schema("Vault-relative metrics file containing the record."),
                "sourceLine": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based line number in the metrics file."
                },
                "record": {
                    "type": "object",
                    "description": "Parsed metric record fields.",
                    "additionalProperties": true
                },
                "rawLine": {
                    "type": "string",
                    "description": "Raw NDJSON line as stored in the source file."
                },
                "validationStatus": {
                    "type": "string",
                    "enum": ["valid", "warning", "invalid"],
                    "description": "Aggregate validation state for the record."
                },
                "issues": {
                    "type": "array",
                    "description": "Validation issues associated with the record.",
                    "items": metric_issue_schema.clone()
                }
            },
            "additionalProperties": false
        });
        let metric_file_summary_schema = json!({
            "type": "object",
            "description": "Indexed metrics file summary.",
            "required": ["relativePath", "fileModifiedAt", "indexedAt", "rowCount", "recordCount", "warningCount", "errorCount"],
            "properties": {
                "relativePath": path_schema("Vault-relative metrics file path."),
                "fileModifiedAt": date_time_schema("Filesystem modification timestamp stored for the file."),
                "indexedAt": date_time_schema("Timestamp when Arrowhead last indexed the file."),
                "rowCount": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Number of non-empty NDJSON rows encountered in the file."
                },
                "recordCount": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Number of rows promoted into indexed metric records."
                },
                "warningCount": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Number of warning-level validation issues in the file."
                },
                "errorCount": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Number of error-level validation issues in the file."
                }
            },
            "additionalProperties": false
        });
        let metrics_files_payload_schema = json!({
            "type": "object",
            "description": "Indexed metrics file summaries.",
            "required": ["files"],
            "properties": {
                "files": {
                    "type": "array",
                    "items": metric_file_summary_schema.clone()
                }
            },
            "additionalProperties": false
        });
        let metric_read_payload_schema = json!({
            "type": "object",
            "description": "Single indexed metric record.",
            "required": ["record"],
            "properties": {
                "record": metric_record_entry_schema.clone()
            },
            "additionalProperties": false
        });
        let metrics_search_results_payload_schema = json!({
            "type": "object",
            "description": "Metrics search results.",
            "required": ["total", "results"],
            "properties": {
                "total": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Number of metric records returned."
                },
                "results": {
                    "type": "array",
                    "items": metric_record_entry_schema.clone()
                }
            },
            "additionalProperties": false
        });
        let metric_delete_payload_schema = json!({
            "type": "object",
            "description": "Confirmation that a metric record was deleted.",
            "required": ["deleted"],
            "properties": {
                "deleted": {
                    "type": "object",
                    "required": ["metricId", "sourceFile", "sourceLine"],
                    "properties": {
                        "metricId": {
                            "type": "string",
                            "description": "Stable id of the deleted metric."
                        },
                        "sourceFile": path_schema("Vault-relative metrics file that contained the deleted record."),
                        "sourceLine": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "1-based line number of the deleted record before removal."
                        }
                    },
                    "additionalProperties": false
                }
            },
            "additionalProperties": false
        });
        let metric_file_create_payload_schema = json!({
            "type": "object",
            "description": "Confirmation that a metrics file was created.",
            "required": ["file"],
            "properties": {
                "file": {
                    "type": "object",
                    "required": ["relativePath"],
                    "properties": {
                        "relativePath": path_schema("Vault-relative metrics file path.")
                    },
                    "additionalProperties": false
                }
            },
            "additionalProperties": false
        });
        let metric_file_rename_payload_schema = json!({
            "type": "object",
            "description": "Confirmation that a metrics file was renamed.",
            "required": ["file"],
            "properties": {
                "file": {
                    "type": "object",
                    "required": ["sourcePath", "destinationPath"],
                    "properties": {
                        "sourcePath": path_schema("Previous vault-relative metrics file path."),
                        "destinationPath": path_schema("New vault-relative metrics file path.")
                    },
                    "additionalProperties": false
                }
            },
            "additionalProperties": false
        });
        let metric_file_delete_payload_schema = json!({
            "type": "object",
            "description": "Confirmation that a metrics file was deleted.",
            "required": ["file"],
            "properties": {
                "file": {
                    "type": "object",
                    "required": ["relativePath", "rowCount"],
                    "properties": {
                        "relativePath": path_schema("Deleted vault-relative metrics file path."),
                        "rowCount": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Number of non-empty rows that existed before deletion."
                        }
                    },
                    "additionalProperties": false
                }
            },
            "additionalProperties": false
        });
        let context_note_item_schema = json!({
            "type": "object",
            "description": "Lightweight note summary surfaced by context.",
            "required": ["noteId"],
            "properties": {
                "noteId": note_id_field_schema.clone(),
                "title": { "type": "string" },
                "relativePath": path_schema("Optional vault-relative path for the note."),
                "fileModifiedAt": date_time_schema("Optional note file modification timestamp."),
                "createdAt": date_time_schema("Optional note file creation timestamp."),
                "preview": { "type": "string" },
                "reason": { "type": "string" },
                "evidenceKind": {
                    "type": "string",
                    "enum": ["explicit", "structural", "inferred"]
                },
                "confidence": { "type": "number" }
            },
            "additionalProperties": false
        });
        let context_metric_item_schema = json!({
            "type": "object",
            "description": "Lightweight metric lead surfaced by context.",
            "required": ["metricId", "key", "value", "source", "ts"],
            "properties": {
                "metricId": metric_id_field_schema.clone(),
                "key": { "type": "string" },
                "value": { "type": "number" },
                "unit": { "type": "string" },
                "source": { "type": "string" },
                "date": {
                    "type": "string",
                    "description": "Optional metric date in YYYY-MM-DD format."
                },
                "ts": date_time_schema("Metric timestamp."),
                "reason": { "type": "string" },
                "evidenceKind": {
                    "type": "string",
                    "enum": ["explicit", "structural", "inferred"]
                },
                "confidence": { "type": "number" }
            },
            "additionalProperties": false
        });
        let context_link_schema = json!({
            "type": "object",
            "description": "Relationship surfaced by context aggregation.",
            "required": ["kind", "from", "to", "reason"],
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["explicit", "structural", "related"]
                },
                "from": { "type": "string" },
                "to": { "type": "string" },
                "reason": { "type": "string" },
                "confidence": { "type": "number" }
            },
            "additionalProperties": false
        });
        let context_attention_item_schema = json!({
            "type": "object",
            "description": "Attention item returned by context aggregation.",
            "required": ["kind", "message"],
            "properties": {
                "kind": { "type": "string" },
                "message": { "type": "string" },
                "noteId": note_id_field_schema.clone(),
                "metricId": metric_id_field_schema.clone(),
                "sourceFile": path_schema("Optional related metrics file path."),
                "sourceLine": {
                    "type": "integer",
                    "minimum": 1
                }
            },
            "additionalProperties": false
        });
        let context_pivot_schema = json!({
            "type": "object",
            "description": "Suggested next command or read for continuing exploration.",
            "required": ["kind", "target", "command", "reason"],
            "properties": {
                "kind": { "type": "string" },
                "target": { "type": "string" },
                "command": { "type": "string" },
                "reason": { "type": "string" },
                "evidenceKind": {
                    "type": "string",
                    "enum": ["explicit", "structural", "inferred"]
                },
                "confidence": { "type": "number" }
            },
            "additionalProperties": false
        });
        let context_payload_schema = json!({
            "type": "object",
            "description": "Stable context payload shared by CLI JSON and MCP tools.",
            "required": ["summary", "history", "activity", "links", "attention", "related"],
            "properties": {
                "summary": {
                    "type": "object",
                    "required": ["kind", "target", "noteCount", "metricCount", "linkCount", "attentionCount"],
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["day", "week", "changed", "note", "metric", "source"]
                        },
                        "target": { "type": "string" },
                        "label": { "type": "string" },
                        "noteCount": { "type": "integer", "minimum": 0 },
                        "metricCount": { "type": "integer", "minimum": 0 },
                        "linkCount": { "type": "integer", "minimum": 0 },
                        "attentionCount": { "type": "integer", "minimum": 0 }
                    },
                    "additionalProperties": false
                },
                "history": {
                    "type": "object",
                    "properties": {
                        "notes": {
                            "type": "array",
                            "items": context_note_item_schema.clone()
                        },
                        "metrics": {
                            "type": "array",
                            "items": metric_record_entry_schema.clone()
                        }
                    },
                    "additionalProperties": false
                },
                "activity": {
                    "type": "object",
                    "properties": {
                        "notes": {
                            "type": "array",
                            "items": context_note_item_schema.clone()
                        },
                        "notesCreated": {
                            "type": "array",
                            "items": context_note_item_schema.clone()
                        },
                        "notesUpdated": {
                            "type": "array",
                            "items": context_note_item_schema.clone()
                        },
                        "metrics": {
                            "type": "array",
                            "items": metric_record_entry_schema.clone()
                        },
                        "links": {
                            "type": "array",
                            "items": context_link_schema.clone()
                        },
                        "files": {
                            "type": "array",
                            "items": metric_file_summary_schema.clone()
                        }
                    },
                    "additionalProperties": false
                },
                "links": {
                    "type": "object",
                    "properties": {
                        "items": {
                            "type": "array",
                            "items": context_link_schema.clone()
                        }
                    },
                    "additionalProperties": false
                },
                "attention": {
                    "type": "object",
                    "properties": {
                        "items": {
                            "type": "array",
                            "items": context_attention_item_schema.clone()
                        }
                    },
                    "additionalProperties": false
                },
                "related": {
                    "type": "object",
                    "properties": {
                        "days": {
                            "type": "array",
                            "items": {
                                "type": "string",
                                "description": "Related day identifier in YYYY-MM-DD format."
                            }
                        },
                        "notes": {
                            "type": "array",
                            "items": context_note_item_schema.clone()
                        },
                        "metrics": {
                            "type": "array",
                            "items": context_metric_item_schema.clone()
                        },
                        "metricKeys": {
                            "type": "array",
                            "items": { "type": "string" }
                        },
                        "sources": {
                            "type": "array",
                            "items": { "type": "string" }
                        }
                    },
                    "additionalProperties": false
                },
                "pivots": {
                    "type": "array",
                    "items": context_pivot_schema
                }
            },
            "additionalProperties": false
        });
        let note_list_item_schema = json!({
            "type": "object",
            "description": "Summary information for a single note.",
            "required": ["noteId"],
            "properties": {
                "noteId": note_id_field_schema.clone(),
                "title": {
                    "type": "string",
                    "description": "Optional title resolved from note metadata."
                },
                "relativePath": path_schema("Vault-relative filesystem path to the note file."),
                "fileModifiedAt": date_time_schema("Last modification timestamp of the note file."),
                "createdAt": date_time_schema("Creation timestamp recorded for the note, when available.")
            },
            "additionalProperties": false
        });
        let note_metadata_payload_schema = json!({
            "type": "object",
            "description": "Metadata for a single note without the Markdown body.",
            "required": ["noteId", "metadata"],
            "properties": {
                "noteId": note_id_field_schema.clone(),
                "title": {
                    "type": "string",
                    "description": "Optional note title."
                },
                "metadata": metadata_map_schema()
            },
            "additionalProperties": false
        });
        let note_content_payload_schema = json!({
            "type": "object",
            "description": "Complete note payload including metadata and Markdown content.",
            "required": ["noteId", "metadata", "content", "relativePath", "fileModifiedAt"],
            "properties": {
                "noteId": note_id_field_schema.clone(),
                "title": {
                    "type": "string",
                    "description": "Optional note title derived from metadata."
                },
                "metadata": metadata_map_schema(),
                "content": {
                    "type": "string",
                    "description": "Markdown body with frontmatter removed."
                },
                "raw": {
                    "type": "string",
                    "description": "Full Markdown text including frontmatter when available."
                },
                "relativePath": path_schema("Vault-relative filesystem path to the note."),
                "fileModifiedAt": date_time_schema("Last modification timestamp recorded for the note file."),
                "createdAt": date_time_schema("Creation timestamp for the note, when known.")
            },
            "additionalProperties": false
        });
        let search_result_item_schema = json!({
            "type": "object",
            "description": "Individual search result entry.",
            "required": ["noteId", "score", "metadata"],
            "properties": {
                "noteId": note_id_field_schema.clone(),
                "title": {
                    "type": "string",
                    "description": "Optional title associated with the note."
                },
                "score": {
                    "type": "number",
                    "description": "Combined relevance score between 0 and 1."
                },
                "bm25": {
                    "type": "number",
                    "description": "Raw BM25 rank (lower is better) when returned by the FTS index."
                },
                "relativePath": path_schema("Vault-relative path for the note file."),
                "preview": {
                    "type": "string",
                    "description": "Optional snippet excerpt that includes highlighted matches."
                },
                "reason": {
                    "type": "string",
                    "description": "Human-readable explanation of why the note matched."
                },
                "metadata": metadata_map_schema()
            },
            "additionalProperties": false
        });
        let search_results_payload_schema = json!({
            "type": "object",
            "description": "Search response containing ranked results.",
            "required": ["total", "results"],
            "properties": {
                "total": {
                    "type": "integer",
                    "description": "Number of results returned in this response."
                },
                "results": {
                    "type": "array",
                    "description": "Ranked search results.",
                    "items": search_result_item_schema
                }
            },
            "additionalProperties": false
        });
        let graph_context_output_schema = json!({
            "type": "object",
            "description": "Combined backlink and forward-link context for a note.",
            "required": ["noteId", "backlinks", "forwardLinks"],
            "properties": {
                "noteId": note_id_field_schema.clone(),
                "backlinks": {
                    "type": "array",
                    "description": "Notes that link to the requested note.",
                    "items": link_edge_schema.clone()
                },
                "forwardLinks": {
                    "type": "array",
                    "description": "Links originating from the requested note.",
                    "items": link_edge_schema.clone()
                }
            },
            "additionalProperties": false
        });
        let graph_links_payload_schema = json!({
            "type": "object",
            "description": "Directional graph links for a note.",
            "required": ["noteId", "links"],
            "properties": {
                "noteId": note_id_field_schema.clone(),
                "links": {
                    "type": "array",
                    "description": "Collected link edges in the requested direction.",
                    "items": link_edge_schema.clone()
                }
            },
            "additionalProperties": false
        });
        let graph_orphan_item_schema = json!({
            "type": "object",
            "description": "Note that currently has no inbound or outbound links.",
            "required": ["noteId"],
            "properties": {
                "noteId": note_id_field_schema.clone(),
                "title": {
                    "type": "string",
                    "description": "Optional note title."
                },
                "relativePath": path_schema("Vault-relative path for the orphaned note.")
            },
            "additionalProperties": false
        });
        let graph_orphans_payload_schema = json!({
            "type": "object",
            "description": "Summary of notes that are not referenced anywhere in the vault.",
            "required": ["total", "notes"],
            "properties": {
                "total": {
                    "type": "integer",
                    "description": "Count of orphaned notes."
                },
                "notes": {
                    "type": "array",
                    "description": "Details of orphaned notes.",
                    "items": graph_orphan_item_schema
                }
            },
            "additionalProperties": false
        });
        let graph_unresolved_payload_schema = json!({
            "type": "object",
            "description": "Unresolved WikiLinks that do not point to existing notes.",
            "required": ["total", "links"],
            "properties": {
                "total": {
                    "type": "integer",
                    "description": "Number of unresolved links discovered."
                },
                "links": {
                    "type": "array",
                    "description": "Link entries that failed to resolve.",
                    "items": link_edge_schema.clone()
                }
            },
            "additionalProperties": false
        });
        let note_delete_payload_schema = json!({
            "type": "object",
            "description": "Confirmation payload returned after deleting a note.",
            "required": ["noteId", "deleted"],
            "properties": {
                "noteId": note_id_field_schema,
                "deleted": {
                    "type": "boolean",
                    "description": "Indicates whether the note file was removed."
                },
                "prunedDirectories": {
                    "type": "array",
                    "description": "Vault-relative directories removed after deletion.",
                    "items": path_schema("Directory pruned as part of note deletion."),
                    "default": []
                }
            },
            "additionalProperties": false
        });
        let related_note_schema = json!({
            "type": "object",
            "description": "Single related note surfaced by discovery tooling.",
            "required": ["noteId"],
            "properties": {
                "noteId": {
                    "type": "string",
                    "description": "Identifier of the related note."
                },
                "title": {
                    "type": "string",
                    "description": "Optional title of the related note."
                },
                "score": {
                    "type": "number",
                    "description": "Similarity score reported by the strategy."
                },
                "reason": {
                    "type": "string",
                    "description": "Explanation of why the note was returned."
                },
                "metadata": metadata_map_schema()
            },
            "additionalProperties": false
        });
        let related_notes_payload_schema = json!({
            "type": "object",
            "description": "Discovery results describing notes related to an anchor or query.",
            "required": ["strategy", "related"],
            "properties": {
                "noteId": {
                    "type": "string",
                    "description": "Anchor note identifier when one was provided."
                },
                "query": {
                    "type": "string",
                    "description": "Original query string when discovery was seeded from free text."
                },
                "strategy": {
                    "type": "string",
                    "enum": ["auto", "semantic", "graph", "hybrid"],
                    "description": "Strategy that produced the related notes."
                },
                "fallbackStrategy": {
                    "type": "string",
                    "enum": ["auto", "semantic", "graph", "hybrid"],
                    "description": "Fallback strategy used when the requested one was unavailable."
                },
                "related": {
                    "type": "array",
                    "description": "Related notes ranked by relevance.",
                    "items": related_note_schema
                }
            },
            "additionalProperties": false
        });
        let vault_stats_payload_schema = json!({
            "type": "object",
            "description": "Aggregated vault statistics snapshot.",
            "required": ["generatedAt", "totalNotes"],
            "properties": {
                "generatedAt": date_time_schema("Timestamp when the statistics snapshot was generated."),
                "totalNotes": {
                    "type": "integer",
                    "description": "Total number of markdown notes discovered in the vault."
                },
                "indexedNotes": {
                    "type": "integer",
                    "description": "Number of notes currently indexed by the Arrowhead daemon."
                },
                "errorNotes": {
                    "type": "integer",
                    "description": "Number of notes that reported indexing errors."
                },
                "totalWords": {
                    "type": "integer",
                    "description": "Approximate aggregate word count across the vault."
                },
                "averageWordsPerNote": {
                    "type": "number",
                    "description": "Average word count per note."
                },
                "recentNotes": {
                    "type": "array",
                    "description": "Optional summary of recently modified notes.",
                    "items": note_list_item_schema.clone()
                }
            },
            "additionalProperties": false
        });
        let naming_pattern_schema = json!({
            "type": "object",
            "description": "Summary describing a naming convention detected in the vault.",
            "required": ["pattern", "count"],
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Human-readable description of the naming pattern."
                },
                "count": {
                    "type": "integer",
                    "description": "Number of notes matching the pattern."
                },
                "examples": {
                    "type": "array",
                    "description": "Representative examples illustrating the pattern.",
                    "items": {
                        "type": "string"
                    },
                    "default": []
                }
            },
            "additionalProperties": false
        });
        let metadata_value_kind_schema = json!({
            "type": "string",
            "enum": ["string", "number", "boolean", "array", "object", "null"],
            "description": "Metadata value kind observed for the field."
        });
        let metadata_common_value_schema = json!({
            "type": "object",
            "description": "Common metadata value and its frequency.",
            "required": ["value", "count"],
            "properties": {
                "value": {
                    "description": "Captured metadata value.",
                    "default": null
                },
                "count": {
                    "type": "integer",
                    "description": "Number of notes that contained this value."
                }
            },
            "additionalProperties": false
        });
        let metadata_field_stats_schema = json!({
            "type": "object",
            "description": "Aggregated metadata statistics for a single field.",
            "required": ["field", "noteCount"],
            "properties": {
                "field": {
                    "type": "string",
                    "description": "Field name as it appears in note frontmatter."
                },
                "noteCount": {
                    "type": "integer",
                    "description": "Number of notes that specified the field."
                },
                "valueKinds": {
                    "type": "array",
                    "description": "Value categories observed for the field.",
                    "items": metadata_value_kind_schema,
                    "default": []
                },
                "commonValues": {
                    "type": "array",
                    "description": "Most common values ordered by frequency.",
                    "items": metadata_common_value_schema,
                    "default": []
                }
            },
            "additionalProperties": false
        });
        let style_guide_schema = json!({
            "type": "object",
            "description": "User-authored style guide surfaced to agents.",
            "required": ["relativePath", "content"],
            "properties": {
                "relativePath": path_schema("Vault-relative path to the style guide document."),
                "content": {
                    "type": "string",
                    "description": "Raw Markdown content of the style guide."
                }
            },
            "additionalProperties": false
        });
        let workspace_settings_schema = json!({
            "type": "object",
            "description": "Workspace configuration relevant to conventions analysis.",
            "properties": {
                "kind": {
                    "type": "string",
                    "description": "Workspace flavour (e.g., obsidian or generic)."
                },
                "attachmentsFolder": path_schema("Attachments directory relative to the vault root."),
                "ignoredFolders": {
                    "type": "array",
                    "description": "User-defined ignore list derived from workspace preferences.",
                    "items": path_schema("Folder ignored by workspace configuration."),
                    "default": []
                },
                "dailyNoteFormat": {
                    "type": "string",
                    "description": "Daily note file name template if configured."
                },
                "linkStyle": {
                    "type": "string",
                    "description": "Preferred internal link style (e.g., with or without file extension)."
                }
            },
            "additionalProperties": false
        });
        let metrics_settings_schema = json!({
            "type": "object",
            "description": "Resolved metrics conventions and discovered metrics files.",
            "required": [
                "source",
                "root",
                "extensions",
                "defaultWriteFile",
                "recordReferencePrefix",
                "weekStartDay",
                "dayStartHour"
            ],
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Source used to resolve metrics conventions."
                },
                "sourcePath": path_schema("Filesystem path backing the resolved metrics conventions."),
                "root": path_schema("Metrics root directory relative to the vault root."),
                "extensions": {
                    "type": "array",
                    "description": "File suffixes recognised as metrics files.",
                    "items": {
                        "type": "string"
                    }
                },
                "defaultWriteFile": path_schema("Default metrics write target relative to the vault root."),
                "recordReferencePrefix": {
                    "type": "string",
                    "description": "Prefix used for metrics references such as `metric:<id>`."
                },
                "weekStartDay": {
                    "type": "string",
                    "description": "Week start day used by metrics time windows."
                },
                "dayStartHour": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 23,
                    "description": "Hour offset that determines when a new metrics day starts."
                },
                "files": {
                    "type": "array",
                    "description": "Metrics files currently discovered under the configured root.",
                    "items": path_schema("Metrics file relative to the vault root."),
                    "default": []
                }
            },
            "additionalProperties": false
        });
        let obsidian_settings_schema = json!({
            "type": "object",
            "description": "Legacy Obsidian metadata retained for backward compatibility.",
            "properties": {
                "attachmentsFolder": path_schema("Attachments directory relative to the vault root."),
                "ignoredFolders": {
                    "type": "array",
                    "description": "User-defined ignore list derived from Obsidian preferences.",
                    "items": path_schema("Folder ignored by Obsidian configuration."),
                    "default": []
                },
                "dailyNoteFormat": {
                    "type": "string",
                    "description": "Daily note file name template if configured."
                },
                "linkStyle": {
                    "type": "string",
                    "description": "Preferred internal link style (e.g., with or without file extension)."
                }
            },
            "additionalProperties": false
        });
        let vault_conventions_payload_schema = json!({
            "type": "object",
            "description": "Summary of naming patterns, metadata usage, and conventions.",
            "required": ["namingPatterns", "metadataFields", "metrics"],
            "properties": {
                "namingPatterns": {
                    "type": "array",
                    "description": "Detected naming patterns across the vault.",
                    "items": naming_pattern_schema
                },
                "metadataFields": {
                    "type": "array",
                    "description": "Aggregated metadata field statistics.",
                    "items": metadata_field_stats_schema
                },
                "obsidian": obsidian_settings_schema,
                "workspace": workspace_settings_schema,
                "metrics": metrics_settings_schema,
                "styleGuide": style_guide_schema
            },
            "additionalProperties": false
        });
        let daemon_status_schema = json!({
            "type": "object",
            "description": "Snapshot of Arrowhead daemon activity.",
            "required": ["updatedAt", "indexedNotes", "errorNotes"],
            "properties": {
                "updatedAt": date_time_schema("Timestamp when the daemon status snapshot was recorded."),
                "indexedNotes": {
                    "type": "integer",
                    "description": "Total number of notes indexed by the daemon."
                },
                "errorNotes": {
                    "type": "integer",
                    "description": "Number of notes currently in an error state."
                },
                "activity": {
                    "type": "string",
                    "description": "Optional description of the daemon's current activity."
                },
                "queuedJobs": {
                    "type": "integer",
                    "description": "Number of queued jobs if the daemon is busy."
                },
                "summary": {
                    "type": "string",
                    "description": "Human-readable summary of daemon activity."
                }
            },
            "additionalProperties": false
        });

        let mut tools = vec![
            ToolDescriptor {
                name: "graph_get_context".to_string(),
                title: Some("Graph: Context".to_string()),
                description: Some(
                    "Return backlinks and forward links for a note to understand its neighbourhood."
                        .to_string(),
                ),
                input_schema: note_id_schema.clone(),
                output_schema: Some(graph_context_output_schema.clone()),
                annotations: Some(json!({ "method": "mcp.graph.get_context" })),
            },
            ToolDescriptor {
                name: "graph_get_backlinks".to_string(),
                title: Some("Graph: Backlinks".to_string()),
                description: Some(
                    "Return inbound WikiLinks pointing to the requested note.".to_string(),
                ),
                input_schema: note_id_schema.clone(),
                output_schema: Some(graph_links_payload_schema.clone()),
                annotations: Some(json!({ "method": "mcp.graph.get_backlinks" })),
            },
            ToolDescriptor {
                name: "graph_get_forward_links".to_string(),
                title: Some("Graph: Forward Links".to_string()),
                description: Some(
                    "Return outbound WikiLinks originating from the requested note.".to_string(),
                ),
                input_schema: note_id_schema.clone(),
                output_schema: Some(graph_links_payload_schema.clone()),
                annotations: Some(json!({ "method": "mcp.graph.get_forward_links" })),
            },
            ToolDescriptor {
                name: "graph_find_orphans".to_string(),
                title: Some("Graph: Orphans".to_string()),
                description: Some(
                    "List notes that are not referenced anywhere in the vault.".to_string(),
                ),
                input_schema: empty_schema(),
                output_schema: Some(graph_orphans_payload_schema),
                annotations: Some(json!({ "method": "mcp.graph.find_orphans" })),
            },
            ToolDescriptor {
                name: "graph_find_unresolved".to_string(),
                title: Some("Graph: Unresolved Links".to_string()),
                description: Some(
                    "List links that could not be resolved to an existing note.".to_string(),
                ),
                input_schema: empty_schema(),
                output_schema: Some(graph_unresolved_payload_schema),
                annotations: Some(json!({ "method": "mcp.graph.find_unresolved" })),
            },
            ToolDescriptor {
                name: "context_get_day".to_string(),
                title: Some("Context: Day".to_string()),
                description: Some(
                    "Build a context payload around a specific day using daily notes, recent note changes, and metric activity."
                        .to_string(),
                ),
                input_schema: context_day_schema,
                output_schema: Some(context_payload_schema.clone()),
                annotations: Some(json!({ "method": "mcp.context.get_day" })),
            },
            ToolDescriptor {
                name: "context_get_week".to_string(),
                title: Some("Context: Week".to_string()),
                description: Some(
                    "Build a context payload around a calendar week."
                        .to_string(),
                ),
                input_schema: context_week_schema,
                output_schema: Some(context_payload_schema.clone()),
                annotations: Some(json!({ "method": "mcp.context.get_week" })),
            },
            ToolDescriptor {
                name: "context_get_month".to_string(),
                title: Some("Context: Month".to_string()),
                description: Some(
                    "Build a context payload around a calendar month."
                        .to_string(),
                ),
                input_schema: context_month_schema,
                output_schema: Some(context_payload_schema.clone()),
                annotations: Some(json!({ "method": "mcp.context.get_month" })),
            },
            ToolDescriptor {
                name: "context_get_changed".to_string(),
                title: Some("Context: Changed".to_string()),
                description: Some(
                    "Build a context payload around recently changed notes, metric activity, and metrics files."
                        .to_string(),
                ),
                input_schema: context_changed_schema,
                output_schema: Some(context_payload_schema.clone()),
                annotations: Some(json!({ "method": "mcp.context.get_changed" })),
            },
            ToolDescriptor {
                name: "context_get_note".to_string(),
                title: Some("Context: Note".to_string()),
                description: Some(
                    "Build a context payload around a note using graph structure, related notes, and linked metrics."
                        .to_string(),
                ),
                input_schema: context_note_schema,
                output_schema: Some(context_payload_schema.clone()),
                annotations: Some(json!({ "method": "mcp.context.get_note" })),
            },
            ToolDescriptor {
                name: "context_get_metric".to_string(),
                title: Some("Context: Metric".to_string()),
                description: Some(
                    "Build a context payload around a metric id or metric key."
                        .to_string(),
                ),
                input_schema: context_metric_schema,
                output_schema: Some(context_payload_schema.clone()),
                annotations: Some(json!({ "method": "mcp.context.get_metric" })),
            },
            ToolDescriptor {
                name: "context_get_source".to_string(),
                title: Some("Context: Source".to_string()),
                description: Some(
                    "Build a context payload around a metrics source."
                        .to_string(),
                ),
                input_schema: context_source_schema,
                output_schema: Some(context_payload_schema),
                annotations: Some(json!({ "method": "mcp.context.get_source" })),
            },
            ToolDescriptor {
                name: "search_fts".to_string(),
                title: Some("Search: Full Text".to_string()),
                description: Some(
                    "Full-text search across all notes using SQLite FTS5. Defaults to 10 results."
                        .to_string(),
                ),
                input_schema: search_schema.clone(),
                output_schema: Some(search_results_payload_schema.clone()),
                annotations: Some(json!({ "method": "mcp.search.fts" })),
            },
            ToolDescriptor {
                name: "search_semantic".to_string(),
                title: Some("Search: Semantic".to_string()),
                description: Some(
                    "Semantic similarity search using embeddings. Returns ranked results with scores."
                        .to_string(),
                ),
                input_schema: search_schema.clone(),
                output_schema: Some(search_results_payload_schema.clone()),
                annotations: Some(json!({ "method": "mcp.search.semantic" })),
            },
            ToolDescriptor {
                name: "search_hybrid".to_string(),
                title: Some("Search: Hybrid".to_string()),
                description: Some(
                    "Combine semantic and keyword search results for balanced relevance.".to_string(),
                ),
                input_schema: search_schema.clone(),
                output_schema: Some(search_results_payload_schema),
                annotations: Some(json!({ "method": "mcp.search.hybrid" })),
            },
            ToolDescriptor {
                name: "metrics_list_files".to_string(),
                title: Some("Metrics: List Files".to_string()),
                description: Some(
                    "List indexed metrics files together with stored validation counts."
                        .to_string(),
                ),
                input_schema: empty_schema(),
                output_schema: Some(metrics_files_payload_schema),
                annotations: Some(json!({ "method": "mcp.metrics.list_files" })),
            },
            ToolDescriptor {
                name: "metrics_read".to_string(),
                title: Some("Metrics: Read".to_string()),
                description: Some(
                    "Read a single indexed metric record by stable id or `metric:<id>` reference."
                        .to_string(),
                ),
                input_schema: metric_id_schema,
                output_schema: Some(metric_read_payload_schema.clone()),
                annotations: Some(json!({ "method": "mcp.metrics.read" })),
            },
            ToolDescriptor {
                name: "metrics_search".to_string(),
                title: Some("Metrics: Search".to_string()),
                description: Some(
                    "Search indexed metrics records using free text plus `key:`, `source:`, `file:`, `date:`, and `note:` filters."
                        .to_string(),
                ),
                input_schema: search_schema.clone(),
                output_schema: Some(metrics_search_results_payload_schema),
                annotations: Some(json!({ "method": "mcp.metrics.search" })),
            },
            ToolDescriptor {
                name: "metrics_create".to_string(),
                title: Some("Metrics: Create".to_string()),
                description: Some(
                    "Create a metric record in the canonical NDJSON file and refresh the metrics index."
                        .to_string(),
                ),
                input_schema: metric_create_schema,
                output_schema: Some(metric_read_payload_schema.clone()),
                annotations: Some(json!({ "method": "mcp.metrics.create" })),
            },
            ToolDescriptor {
                name: "metrics_update".to_string(),
                title: Some("Metrics: Update".to_string()),
                description: Some(
                    "Update an existing metric record by stable id and refresh the metrics index."
                        .to_string(),
                ),
                input_schema: metric_update_schema,
                output_schema: Some(metric_read_payload_schema.clone()),
                annotations: Some(json!({ "method": "mcp.metrics.update" })),
            },
            ToolDescriptor {
                name: "metrics_delete".to_string(),
                title: Some("Metrics: Delete".to_string()),
                description: Some(
                    "Delete a metric record by stable id after explicit confirmation."
                        .to_string(),
                ),
                input_schema: metric_delete_schema,
                output_schema: Some(metric_delete_payload_schema),
                annotations: Some(json!({ "method": "mcp.metrics.delete" })),
            },
            ToolDescriptor {
                name: "metrics_create_file".to_string(),
                title: Some("Metrics: Create File".to_string()),
                description: Some(
                    "Create an empty metrics NDJSON file and refresh the metrics index."
                        .to_string(),
                ),
                input_schema: metric_file_create_schema,
                output_schema: Some(metric_file_create_payload_schema),
                annotations: Some(json!({ "method": "mcp.metrics.create_file" })),
            },
            ToolDescriptor {
                name: "metrics_rename_file".to_string(),
                title: Some("Metrics: Rename File".to_string()),
                description: Some(
                    "Rename a metrics file and move its indexed rows to the new path."
                        .to_string(),
                ),
                input_schema: metric_file_rename_schema,
                output_schema: Some(metric_file_rename_payload_schema),
                annotations: Some(json!({ "method": "mcp.metrics.rename_file" })),
            },
            ToolDescriptor {
                name: "metrics_delete_file".to_string(),
                title: Some("Metrics: Delete File".to_string()),
                description: Some(
                    "Delete a metrics file after explicit confirmation and remove it from the index."
                        .to_string(),
                ),
                input_schema: metric_file_delete_schema,
                output_schema: Some(metric_file_delete_payload_schema),
                annotations: Some(json!({ "method": "mcp.metrics.delete_file" })),
            },
            ToolDescriptor {
                name: "vault_status".to_string(),
                title: Some("Vault: Status".to_string()),
                description: Some(
                    "Summarise daemon activity, indexed counts, and queue status.".to_string(),
                ),
                input_schema: empty_schema(),
                output_schema: Some(daemon_status_schema),
                annotations: Some(json!({ "method": "mcp.vault.status" })),
            },
            ToolDescriptor {
                name: "notes_read".to_string(),
                title: Some("Notes: Read".to_string()),
                description: Some(
                    "Read a note's metadata and full Markdown content (including frontmatter)."
                        .to_string(),
                ),
                input_schema: note_id_schema.clone(),
                output_schema: Some(note_content_payload_schema.clone()),
                annotations: Some(json!({ "method": "mcp.notes.read" })),
            },
            ToolDescriptor {
                name: "notes_list".to_string(),
                title: Some("Notes: List".to_string()),
                description: Some(
                    "List notes from the vault, optionally limiting results or returning IDs only."
                        .to_string(),
                ),
                input_schema: notes_list_schema,
                output_schema: Some(json!({
                    "type": "object",
                    "description": "Collection of note summaries.",
                    "required": ["notes"],
                    "properties": {
                        "notes": {
                            "type": "array",
                            "items": note_list_item_schema.clone()
                        }
                    },
                    "additionalProperties": false
                })),
                annotations: Some(json!({ "method": "mcp.notes.list" })),
            },
            ToolDescriptor {
                name: "notes_metadata".to_string(),
                title: Some("Notes: Metadata".to_string()),
                description: Some(
                    "Fetch metadata for a specific note without loading the Markdown body."
                        .to_string(),
                ),
                input_schema: note_id_schema.clone(),
                output_schema: Some(note_metadata_payload_schema),
                annotations: Some(json!({ "method": "mcp.notes.metadata" })),
            },
            ToolDescriptor {
                name: "notes_create".to_string(),
                title: Some("Notes: Create".to_string()),
                description: Some(
                    "Create a new note; run Discovery: Vault Conventions first so naming and metadata match vault rules."
                        .to_string(),
                ),
                input_schema: note_create_schema,
                output_schema: Some(note_content_payload_schema.clone()),
                annotations: Some(json!({
                    "method": "mcp.notes.create",
                    "requiresVaultConventions": true
                })),
            },
            ToolDescriptor {
                name: "notes_update".to_string(),
                title: Some("Notes: Update".to_string()),
                description: Some(
                    "Update an existing note. Call Discovery: Vault Conventions first to honour vault naming and metadata expectations."
                        .to_string(),
                ),
                input_schema: note_update_schema,
                output_schema: Some(note_content_payload_schema),
                annotations: Some(json!({
                    "method": "mcp.notes.update",
                    "requiresVaultConventions": true
                })),
            },
            ToolDescriptor {
                name: "notes_delete".to_string(),
                title: Some("Notes: Delete".to_string()),
                description: Some(
                    "Delete a note. Call Discovery: Vault Conventions first to confirm removal aligns with vault policies. To proceed, set confirm=true; the response reports pruned paths."
                        .to_string(),
                ),
                input_schema: note_delete_schema,
                output_schema: Some(note_delete_payload_schema),
                annotations: Some(json!({
                    "method": "mcp.notes.delete",
                    "requiresVaultConventions": true
                })),
            },
            ToolDescriptor {
                name: "discovery_get_related_notes".to_string(),
                title: Some("Discovery: Related Notes".to_string()),
                description: Some(
                    "Return notes related to either an anchor note or a natural-language query."
                        .to_string(),
                ),
                input_schema: related_notes_schema,
                output_schema: Some(related_notes_payload_schema),
                annotations: Some(json!({ "method": "mcp.discovery.get_related_notes" })),
            },
            ToolDescriptor {
                name: "discovery_get_vault_stats".to_string(),
                title: Some("Discovery: Vault Stats".to_string()),
                description: Some("Summarise vault statistics.".to_string()),
                input_schema: empty_schema(),
                output_schema: Some(vault_stats_payload_schema),
                annotations: Some(json!({ "method": "mcp.discovery.get_vault_stats" })),
            },
            ToolDescriptor {
                name: "discovery_get_vault_conventions".to_string(),
                title: Some("Discovery: Vault Conventions".to_string()),
                description: Some(
                    "Summarise naming patterns, metadata usage, and conventions. Run this before creating, updating, or deleting notes."
                        .to_string(),
                ),
                input_schema: empty_schema(),
                output_schema: Some(vault_conventions_payload_schema),
                annotations: Some(json!({ "method": "mcp.discovery.get_vault_conventions" })),
            },
        ];
        tools.sort_by(|a, b| {
            let priority = |name: &str| {
                if name == "discovery_get_vault_conventions" {
                    0
                } else {
                    1
                }
            };
            priority(&a.name)
                .cmp(&priority(&b.name))
                .then_with(|| a.name.cmp(&b.name))
        });
        tools
    }

    async fn ensure_daemon_ready(&self) -> Result<(), ProtocolError> {
        let _ = self.daemon_status().await?;
        Ok(())
    }

    async fn daemon_status(&self) -> Result<DaemonStatus, ProtocolError> {
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
                "note {note_id} is not indexed. Run `arrowhead index start` to refresh the index."
            )))
        }
    }
}

#[async_trait]
impl MessageHandler for HandlerRegistry {
    #[allow(clippy::too_many_lines)]
    async fn handle_request(&self, request: Request) -> Result<Value, ProtocolError> {
        match request.method.as_str() {
            "initialize" => self.handle_initialize(request).await,
            "tools/list" => self.handle_tools_list(request).await,
            "tools/call" => self.handle_tools_call(request).await,
            "ping" => Ok(json!({})),
            _ => self.handle_named_tool(request).await,
        }
    }

    async fn handle_notification(&self, notification: Notification) -> Result<(), ProtocolError> {
        if notification.method.as_str() == "notifications/initialized" {
            debug!("received notifications/initialized acknowledgement");
            return Ok(());
        }

        debug!(
            method = %notification.method,
            "dropping unhandled notification"
        );
        Ok(())
    }
}

fn resolve_tool_method(name: &str) -> Option<&'static str> {
    match name {
        "graph_get_context" => Some("mcp.graph.get_context"),
        "graph_get_backlinks" => Some("mcp.graph.get_backlinks"),
        "graph_get_forward_links" => Some("mcp.graph.get_forward_links"),
        "graph_find_orphans" => Some("mcp.graph.find_orphans"),
        "graph_find_unresolved" => Some("mcp.graph.find_unresolved"),
        "context_get_day" => Some("mcp.context.get_day"),
        "context_get_week" => Some("mcp.context.get_week"),
        "context_get_month" => Some("mcp.context.get_month"),
        "context_get_changed" => Some("mcp.context.get_changed"),
        "context_get_note" => Some("mcp.context.get_note"),
        "context_get_metric" => Some("mcp.context.get_metric"),
        "context_get_source" => Some("mcp.context.get_source"),
        "search_fts" => Some("mcp.search.fts"),
        "search_semantic" => Some("mcp.search.semantic"),
        "search_hybrid" => Some("mcp.search.hybrid"),
        "metrics_list_files" => Some("mcp.metrics.list_files"),
        "metrics_read" => Some("mcp.metrics.read"),
        "metrics_search" => Some("mcp.metrics.search"),
        "metrics_create" => Some("mcp.metrics.create"),
        "metrics_update" => Some("mcp.metrics.update"),
        "metrics_delete" => Some("mcp.metrics.delete"),
        "metrics_create_file" => Some("mcp.metrics.create_file"),
        "metrics_rename_file" => Some("mcp.metrics.rename_file"),
        "metrics_delete_file" => Some("mcp.metrics.delete_file"),
        "vault_status" => Some("mcp.vault.status"),
        "notes_read" => Some("mcp.notes.read"),
        "notes_list" => Some("mcp.notes.list"),
        "notes_metadata" => Some("mcp.notes.metadata"),
        "notes_create" => Some("mcp.notes.create"),
        "notes_update" => Some("mcp.notes.update"),
        "notes_delete" => Some("mcp.notes.delete"),
        "discovery_get_related_notes" => Some("mcp.discovery.get_related_notes"),
        "discovery_get_vault_stats" => Some("mcp.discovery.get_vault_stats"),
        "discovery_get_vault_conventions" => Some("mcp.discovery.get_vault_conventions"),
        _ => None,
    }
}

fn resolve_week_selector(params: &ContextWeekParams) -> Result<WeekContextSelector, ProtocolError> {
    if params.this && params.last {
        return Err(ProtocolError::invalid_params(
            "`this` and `last` cannot both be true for week context",
        ));
    }
    if params.last {
        return Ok(WeekContextSelector::LastWeek);
    }
    if let Some(day) = params.day.as_deref() {
        let parsed = NaiveDate::parse_from_str(day.trim(), "%Y-%m-%d").map_err(|err| {
            ProtocolError::invalid_params(format!("invalid week day `{}`: {err}", day.trim()))
        })?;
        return Ok(WeekContextSelector::ContainingDay(parsed));
    }
    Ok(WeekContextSelector::ThisWeek)
}

fn resolve_month_selector(
    params: &ContextMonthParams,
) -> Result<MonthContextSelector, ProtocolError> {
    if params.this && params.last {
        return Err(ProtocolError::invalid_params(
            "`this` and `last` cannot both be true for month context",
        ));
    }
    if params.day.is_some() && (params.this || params.last) {
        return Err(ProtocolError::invalid_params(
            "`day` cannot be combined with `this` or `last` for month context",
        ));
    }
    if params.last {
        return Ok(MonthContextSelector::LastMonth);
    }
    if let Some(day) = params.day.as_deref() {
        let parsed = NaiveDate::parse_from_str(day.trim(), "%Y-%m-%d").map_err(|err| {
            ProtocolError::invalid_params(format!("invalid month day `{}`: {err}", day.trim()))
        })?;
        return Ok(MonthContextSelector::ContainingDay(parsed));
    }
    Ok(MonthContextSelector::ThisMonth)
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
    } else if message.contains("requires embeddings") {
        ProtocolError::custom(ErrorCode::ToolDisabled, message, None)
    } else {
        ProtocolError::internal(message)
    }
}

fn map_metrics_search_error(err: Error) -> ProtocolError {
    let message = err.to_string();
    if message.contains("empty metrics query") {
        ProtocolError::invalid_params("metrics query must not be empty")
    } else {
        ProtocolError::internal(message)
    }
}

fn map_metrics_mutation_error(err: Error) -> ProtocolError {
    let message = err.to_string();
    if message.contains("must not be empty")
        || message.contains("invalid metrics")
        || message.contains("requires at least one field change")
        || message.contains("was not found")
        || message.contains("already exists")
        || message.contains("ambiguous")
        || message.contains("cannot use `--")
        || message.contains("metric row is invalid")
        || message.contains("does not exist")
        || message.contains("must live under")
        || message.contains("must be different")
        || message.contains("no metrics files were discovered")
    {
        ProtocolError::invalid_params(message)
    } else {
        ProtocolError::internal(message)
    }
}

fn map_context_error(err: Error) -> ProtocolError {
    let message = err.to_string();
    if message.contains("must not be empty")
        || message.contains("not found")
        || message.contains("is not indexed")
        || message.contains("was not found")
        || message.contains("no metric")
        || message.contains("source `")
    {
        ProtocolError::invalid_params(message)
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

fn build_metric_create_request(
    params: MetricCreateParams,
) -> Result<MetricCreateRequest, ProtocolError> {
    Ok(MetricCreateRequest {
        file: params.file_path,
        id: params.id,
        ts: parse_metric_timestamp(&params.ts)?,
        key: params.key,
        value: params.value,
        source: params.source,
        date: params.date.as_deref().map(parse_metric_date).transpose()?,
        unit: params.unit,
        origin_id: params.origin_id,
        note: params.note,
        context: params.context,
        tags: params.tags,
        extra_fields: Default::default(),
    })
}

fn build_metric_update_request(
    params: MetricUpdateParams,
) -> Result<MetricUpdateRequest, ProtocolError> {
    Ok(MetricUpdateRequest {
        metric_id: params.metric_id,
        ts: params
            .ts
            .as_deref()
            .map(parse_metric_timestamp)
            .transpose()?,
        key: params.key,
        value: params.value,
        source: params.source,
        date: parse_metric_date_patch(params.date.as_deref(), params.clear_date)?,
        unit: parse_string_patch(params.unit, params.clear_unit, "unit")?,
        origin_id: parse_string_patch(params.origin_id, params.clear_origin_id, "originId")?,
        note: parse_string_patch(params.note, params.clear_note, "note")?,
        context: parse_context_patch(params.context, params.clear_context, "context")?,
        tags: parse_tags_patch(params.tags, params.clear_tags)?,
    })
}

fn parse_metric_timestamp(input: &str) -> Result<DateTime<FixedOffset>, ProtocolError> {
    DateTime::parse_from_rfc3339(input.trim()).map_err(|err| {
        ProtocolError::invalid_params(format!(
            "invalid metrics timestamp `{}`: {err}",
            input.trim()
        ))
    })
}

fn parse_metric_date(input: &str) -> Result<NaiveDate, ProtocolError> {
    NaiveDate::parse_from_str(input.trim(), "%Y-%m-%d").map_err(|err| {
        ProtocolError::invalid_params(format!("invalid metrics date `{}`: {err}", input.trim()))
    })
}

fn parse_metric_date_patch(
    input: Option<&str>,
    clear: bool,
) -> Result<PatchValue<NaiveDate>, ProtocolError> {
    if clear && input.is_some() {
        return Err(ProtocolError::invalid_params(
            "cannot set `date` and `clearDate` together",
        ));
    }
    if clear {
        return Ok(PatchValue::Clear);
    }
    Ok(input
        .map(parse_metric_date)
        .transpose()?
        .map(PatchValue::Set)
        .unwrap_or(PatchValue::Unchanged))
}

fn parse_string_patch(
    input: Option<String>,
    clear: bool,
    field_name: &str,
) -> Result<PatchValue<String>, ProtocolError> {
    if clear && input.is_some() {
        return Err(ProtocolError::invalid_params(format!(
            "cannot set `{field_name}` and clear it together"
        )));
    }
    if clear {
        Ok(PatchValue::Clear)
    } else {
        Ok(input.map(PatchValue::Set).unwrap_or(PatchValue::Unchanged))
    }
}

fn parse_context_patch(
    input: Option<serde_json::Map<String, Value>>,
    clear: bool,
    field_name: &str,
) -> Result<PatchValue<serde_json::Map<String, Value>>, ProtocolError> {
    if clear && input.is_some() {
        return Err(ProtocolError::invalid_params(format!(
            "cannot set `{field_name}` and clear it together"
        )));
    }
    if clear {
        Ok(PatchValue::Clear)
    } else {
        Ok(input.map(PatchValue::Set).unwrap_or(PatchValue::Unchanged))
    }
}

fn parse_tags_patch(
    input: Vec<String>,
    clear: bool,
) -> Result<PatchValue<Vec<String>>, ProtocolError> {
    if clear && !input.is_empty() {
        return Err(ProtocolError::invalid_params(
            "cannot set `tags` and `clearTags` together",
        ));
    }
    if clear {
        Ok(PatchValue::Clear)
    } else if input.is_empty() {
        Ok(PatchValue::Unchanged)
    } else {
        Ok(PatchValue::Set(input))
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
            return Err(ProtocolError::invalid_params(
                "empty note title updates are not allowed; omit `title` to keep the current title",
            ));
        }
        metadata_map.insert("title".to_string(), Value::String(title_value));
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

fn build_daemon_status_payload(status: DaemonStatus) -> DaemonStatusPayload {
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
        ActivityState::Starting => Some("starting".to_string()),
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
