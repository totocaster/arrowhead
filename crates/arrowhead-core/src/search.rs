//! Search coordination across FTS, semantic, and hybrid strategies.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use tokio::task;
use tracing::{debug, info};

use crate::{
    MetadataMap, NoteId, embeddings::EmbeddingPipeline, query::parse_query, sqlite::IndexDatabase,
};

/// Unified search result payload spanning the different search modes.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    /// Identifier of the matched note.
    pub note_id: NoteId,
    /// Combined relevance score (mode-specific meaning).
    pub score: f32,
    /// Raw BM25 rank reported by SQLite (lower is better).
    pub bm25: f32,
    /// Relative path of the note within the vault, when known.
    pub relative_path: Option<String>,
    /// Optional snippet or preview text.
    pub preview: Option<String>,
    /// High-level explanation of why this result ranked where it did.
    pub reason: Option<String>,
    /// Metadata attached to the note, useful for display.
    pub metadata: MetadataMap,
    /// Optional note title for display purposes.
    pub title: Option<String>,
}

impl SearchResult {
    /// Create a result placeholder while the real implementation is pending.
    pub fn placeholder(note_id: NoteId) -> Self {
        Self {
            note_id,
            score: 0.0,
            bm25: f32::MAX,
            relative_path: None,
            preview: None,
            reason: None,
            metadata: MetadataMap::default(),
            title: None,
        }
    }

    /// Normalised BM25 score that omits sentinel values.
    pub fn bm25_score(&self) -> Option<f32> {
        if !self.bm25.is_finite() || self.bm25 == f32::MAX {
            None
        } else {
            Some(self.bm25)
        }
    }
}

/// Configuration parameters shared by all search modes.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchConfig {
    /// Default number of results to return when a limit is not provided.
    pub default_limit: usize,
    /// Minimum similarity score for semantic matches.
    pub semantic_threshold: f32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            default_limit: 10,
            semantic_threshold: 0.3,
        }
    }
}

/// Public entry point for executing searches.
#[derive(Debug, Clone)]
pub struct SearchService {
    database: Arc<IndexDatabase>,
    config: SearchConfig,
    embeddings: Option<Arc<EmbeddingPipeline>>,
}

impl SearchService {
    /// Create a new search service with the supplied configuration.
    pub fn new(
        database: Arc<IndexDatabase>,
        config: SearchConfig,
        embeddings: Option<Arc<EmbeddingPipeline>>,
    ) -> Self {
        Self {
            database,
            config,
            embeddings,
        }
    }

    /// Execute a full-text search query.
    pub async fn search_fts(
        &self,
        _query: &str,
        _limit: Option<usize>,
    ) -> Result<Vec<SearchResult>> {
        let query = _query.trim();
        if query.is_empty() {
            bail!("empty search query");
        }

        let parsed = parse_query(query)
            .with_context(|| format!("failed to parse search query `{query}`"))?;
        let crate::query::ParsedQuery {
            fts,
            excludes,
            filters,
        } = parsed;

        let limit = _limit.unwrap_or(self.config.default_limit);
        let limit = limit.max(1);
        let filter_count = filters.active_count();
        let database = Arc::clone(&self.database);
        let filters_clone = filters.clone();
        let excludes_clone = excludes.clone();
        let fetch_limit = if excludes.is_empty() {
            limit
        } else {
            limit * 3
        };
        info!(query = query, limit, "executing full-text search");
        if let Some(ref fts_expr) = fts {
            debug!(
                query = query,
                rewritten = fts_expr.as_str(),
                filters = filter_count,
                "parsed query for FTS"
            );
        } else {
            debug!(
                query = query,
                filters = filter_count,
                "executing filter-only query"
            );
        }

        let results = task::spawn_blocking(move || -> Result<Vec<SearchResult>> {
            let mut results = Vec::new();

            if let Some(fts_expr) = &fts {
                let mut matches =
                    database.search_fts_with_filters(fts_expr, fetch_limit, &filters_clone)?;
                if !excludes_clone.is_empty() {
                    let exclude_ids = collect_excluded_ids(&database, &excludes_clone)?;
                    matches.retain(|item| !exclude_ids.contains(&item.note_id));
                }
                if matches.len() > limit {
                    matches.truncate(limit);
                }

                let note_ids: Vec<String> =
                    matches.iter().map(|item| item.note_id.clone()).collect();
                let metadata_maps = database.metadata_for_notes(&note_ids)?;

                for item in matches {
                    let metadata = metadata_maps
                        .get(&item.note_id)
                        .cloned()
                        .unwrap_or_default();
                    let bm25 = item.rank as f32;
                    let reason = Some(format!("Full-text match (rank {:.2})", item.rank));
                    results.push(SearchResult {
                        note_id: item.note_id,
                        score: rank_to_score(item.rank),
                        bm25,
                        relative_path: Some(item.relative_path),
                        preview: item.snippet,
                        reason,
                        metadata,
                        title: item.title,
                    });
                }
            } else {
                let fetch_limit = if excludes_clone.is_empty() {
                    fetch_limit
                } else {
                    limit * 3
                };
                let mut note_ids = database.notes_for_filters(&filters_clone, fetch_limit)?;
                if !excludes_clone.is_empty() {
                    let exclude_ids = collect_excluded_ids(&database, &excludes_clone)?;
                    note_ids.retain(|id| !exclude_ids.contains(id));
                }
                if note_ids.len() > limit {
                    note_ids.truncate(limit);
                }

                if note_ids.is_empty() {
                    return Ok(Vec::new());
                }

                let metadata_maps = database.metadata_for_notes(&note_ids)?;
                let title_map = database.titles_for_notes(&note_ids)?;
                let relative_path_map = database.relative_paths_for_notes(&note_ids)?;

                for note_id in note_ids {
                    let metadata = metadata_maps.get(&note_id).cloned().unwrap_or_default();
                    let title = title_map.get(&note_id).cloned().unwrap_or(None);
                    let relative_path = relative_path_map.get(&note_id).cloned();
                    let preview = database
                        .note_excerpt(&note_id, 240)
                        .context("failed to fetch note excerpt")?;
                    results.push(SearchResult {
                        note_id,
                        score: 0.0,
                        bm25: f32::MAX,
                        relative_path,
                        preview,
                        reason: Some("Filter match".to_string()),
                        metadata,
                        title,
                    });
                }
            }

            Ok(results)
        })
        .await
        .context("search task aborted")??;

        let result_count = results.len();
        info!(query = query, result_count, "full-text search completed");
        Ok(results)
    }

    /// Find notes semantically related to an indexed anchor note.
    pub async fn related_to_note(
        &self,
        note_id: &str,
        limit: Option<usize>,
    ) -> Result<Vec<SearchResult>> {
        let note_id = note_id.trim();
        if note_id.is_empty() {
            bail!("note id must not be empty");
        }

        let pipeline = match self.embeddings.as_ref() {
            Some(pipeline) => pipeline,
            None => bail!("semantic related-note search requires embeddings to be enabled"),
        };

        let vector = pipeline
            .store()
            .vector_for_note(note_id)
            .await?
            .with_context(|| {
                format!(
                    "note {note_id} does not have embeddings yet. Run `arrowhead index start` to reindex it."
                )
            })?;

        let limit = limit.unwrap_or(self.config.default_limit).max(1);
        info!(
            note_id = note_id,
            limit, "executing semantic related-note search"
        );
        debug!(
            note_id = note_id,
            model = pipeline.descriptor().identifier(),
            "using embedding pipeline for related-note search"
        );

        let matches = pipeline
            .store()
            .search(&vector, limit * 2, self.config.semantic_threshold)
            .await
            .context("semantic vector search failed")?;

        if matches.is_empty() {
            return Ok(Vec::new());
        }

        let allowed_list = matches
            .iter()
            .filter(|item| item.note_id != note_id)
            .map(|item| item.note_id.clone())
            .collect::<Vec<_>>();
        if allowed_list.is_empty() {
            return Ok(Vec::new());
        }

        let metadata_maps = self
            .database
            .metadata_for_notes(&allowed_list)
            .context("failed to load metadata for related-note results")?;
        let title_map = self
            .database
            .titles_for_notes(&allowed_list)
            .context("failed to load note titles for related-note results")?;
        let relative_path_map = self
            .database
            .relative_paths_for_notes(&allowed_list)
            .context("failed to load note paths for related-note results")?;

        let mut results = Vec::new();
        for item in matches {
            if item.note_id == note_id {
                continue;
            }

            let similarity = (1.0_f32 - item.distance).max(0.0_f32);
            if similarity < self.config.semantic_threshold {
                continue;
            }

            let metadata = metadata_maps
                .get(&item.note_id)
                .cloned()
                .unwrap_or_default();
            let title = title_map.get(&item.note_id).cloned().unwrap_or(None);
            let relative_path = relative_path_map.get(&item.note_id).cloned();
            let preview = self
                .database
                .note_excerpt(&item.note_id, 240)
                .context("failed to fetch note excerpt")?;

            results.push(SearchResult {
                note_id: item.note_id,
                score: similarity,
                bm25: f32::MAX,
                relative_path,
                preview,
                reason: Some(format!("Semantic similarity {:.2}", similarity)),
                metadata,
                title,
            });

            if results.len() >= limit {
                break;
            }
        }

        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
        info!(
            note_id = note_id,
            result_count = results.len(),
            "semantic related-note search completed"
        );
        Ok(results)
    }

    /// Execute a semantic similarity search.
    pub async fn search_semantic(
        &self,
        query: &str,
        limit: Option<usize>,
    ) -> Result<Vec<SearchResult>> {
        let query = query.trim();
        if query.is_empty() {
            bail!("empty search query");
        }

        let parsed = parse_query(query)
            .with_context(|| format!("failed to parse search query `{query}`"))?;
        let crate::query::ParsedQuery {
            fts,
            excludes,
            filters,
        } = parsed;
        let filters_clone = filters.clone();

        let embedding_seed = fts
            .as_ref()
            .map(|value| semantic_embedding_text(value))
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| query.to_string());
        if embedding_seed.trim().is_empty() {
            bail!("query must include at least one search term");
        }

        let pipeline = match self.embeddings.as_ref() {
            Some(pipeline) => pipeline,
            None => bail!("semantic search requires embeddings to be enabled"),
        };

        let limit = limit.unwrap_or(self.config.default_limit).max(1);
        info!(query = query, limit, "executing semantic search");
        debug!(
            query = query,
            filters = filters.active_count(),
            model = pipeline.descriptor().identifier(),
            "using embedding pipeline for semantic search"
        );
        let generator = pipeline.generator().clone();
        let seed = embedding_seed;
        let query_vector = task::spawn_blocking(move || generator.embed_query(&seed))
            .await
            .context("embedding task aborted")?
            .context("failed to embed search query")?;

        let matches = pipeline
            .store()
            .search(&query_vector, limit * 2, self.config.semantic_threshold)
            .await
            .context("semantic vector search failed")?;

        if matches.is_empty() {
            return Ok(Vec::new());
        }

        let note_ids: Vec<String> = matches.iter().map(|m| m.note_id.clone()).collect();
        let mut allowed_ids = if filters.is_empty() {
            note_ids.iter().cloned().collect::<HashSet<_>>()
        } else {
            self.database
                .filter_note_ids(&note_ids, &filters_clone)
                .context("failed to apply filters to semantic results")?
        };

        if !excludes.is_empty() && !allowed_ids.is_empty() {
            let exclude_ids = collect_excluded_ids(&self.database, &excludes)
                .context("failed to apply NOT exclusions")?;
            allowed_ids.retain(|id| !exclude_ids.contains(id));
        }

        if allowed_ids.is_empty() {
            return Ok(Vec::new());
        }

        let allowed_list: Vec<String> = allowed_ids.iter().cloned().collect();
        let metadata_maps = self
            .database
            .metadata_for_notes(&allowed_list)
            .context("failed to load metadata for semantic results")?;
        let title_map = self
            .database
            .titles_for_notes(&allowed_list)
            .context("failed to load note titles for semantic results")?;
        let relative_path_map = self
            .database
            .relative_paths_for_notes(&allowed_list)
            .context("failed to load note paths for semantic results")?;

        let mut results = Vec::new();
        for item in matches {
            if !allowed_ids.contains(&item.note_id) {
                continue;
            }

            let similarity = (1.0_f32 - item.distance).max(0.0_f32);
            if similarity < self.config.semantic_threshold {
                continue;
            }

            let metadata = metadata_maps
                .get(&item.note_id)
                .cloned()
                .unwrap_or_default();
            let title = title_map.get(&item.note_id).cloned().unwrap_or(None);
            let relative_path = relative_path_map.get(&item.note_id).cloned();
            let preview = self
                .database
                .note_excerpt(&item.note_id, 240)
                .context("failed to fetch note excerpt")?;

            results.push(SearchResult {
                note_id: item.note_id,
                score: similarity,
                bm25: f32::MAX,
                relative_path,
                preview,
                reason: Some(format!("Semantic similarity {:.2}", similarity)),
                metadata,
                title,
            });

            if results.len() >= limit {
                break;
            }
        }

        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
        info!(
            query = query,
            result_count = results.len(),
            "semantic search completed"
        );
        Ok(results)
    }

    /// Execute a hybrid search, combining semantic and keyword results.
    pub async fn search_hybrid(
        &self,
        query: &str,
        limit: Option<usize>,
    ) -> Result<Vec<SearchResult>> {
        const FTS_WEIGHT: f32 = 0.7;
        const SEM_WEIGHT: f32 = 0.5;

        let query = query.trim();
        if query.is_empty() {
            bail!("empty search query");
        }

        let parsed = parse_query(query)
            .with_context(|| format!("failed to parse search query `{query}`"))?;
        let crate::query::ParsedQuery {
            fts,
            excludes,
            filters,
        } = parsed;
        let filters_clone = filters.clone();

        let embedding_seed = fts
            .as_ref()
            .map(|value| semantic_embedding_text(value))
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| query.to_string());
        if embedding_seed.trim().is_empty() {
            bail!("query must include at least one search term");
        }

        let pipeline = match self.embeddings.as_ref() {
            Some(pipeline) => pipeline,
            None => bail!("hybrid search requires embeddings to be enabled"),
        };

        let limit = limit.unwrap_or(self.config.default_limit).max(1);
        info!(query = query, limit, "executing hybrid search");
        debug!(
            query = query,
            filters = filters.active_count(),
            model = pipeline.descriptor().identifier(),
            "using embedding pipeline for hybrid search"
        );

        let generator = pipeline.generator().clone();
        let seed = embedding_seed;
        let query_vector = task::spawn_blocking(move || generator.embed_query(&seed))
            .await
            .context("embedding task aborted")?
            .context("failed to embed search query")?;

        let semantic_matches = pipeline
            .store()
            .search(&query_vector, limit * 3, self.config.semantic_threshold)
            .await
            .context("semantic vector search failed")?;

        let semantic_ids: Vec<String> =
            semantic_matches.iter().map(|m| m.note_id.clone()).collect();
        let mut allowed_semantic_ids = if filters.is_empty() {
            semantic_ids.iter().cloned().collect::<HashSet<_>>()
        } else {
            self.database
                .filter_note_ids(&semantic_ids, &filters_clone)
                .context("failed to apply filters to hybrid semantic results")?
        };
        if !excludes.is_empty() && !allowed_semantic_ids.is_empty() {
            let exclude_ids = collect_excluded_ids(&self.database, &excludes)
                .context("failed to apply NOT exclusions")?;
            allowed_semantic_ids.retain(|id| !exclude_ids.contains(id));
        }

        let fts_results = self
            .search_fts(query, Some(limit * 3))
            .await
            .context("fts portion of hybrid search failed")?;

        #[derive(Default)]
        struct CombinedEntry {
            fts: Option<SearchResult>,
            semantic: Option<f32>,
        }

        let mut combined: HashMap<String, CombinedEntry> = HashMap::new();

        for result in fts_results {
            let note_id = result.note_id.clone();
            combined.entry(note_id).or_default().fts = Some(result);
        }

        for item in semantic_matches {
            if !allowed_semantic_ids.contains(&item.note_id) {
                continue;
            }
            let similarity = (1.0_f32 - item.distance).max(0.0_f32);
            if similarity <= 0.0 {
                continue;
            }
            combined.entry(item.note_id.clone()).or_default().semantic = Some(similarity);
        }

        let missing_ids: Vec<String> = combined
            .iter()
            .filter(|(_, entry)| entry.fts.is_none() && entry.semantic.is_some())
            .map(|(note_id, _)| note_id.clone())
            .collect();

        let metadata_map = self
            .database
            .metadata_for_notes(&missing_ids)
            .context("failed to load metadata for hybrid search results")?;
        let title_map = self
            .database
            .titles_for_notes(&missing_ids)
            .context("failed to load note titles for hybrid search results")?;
        let relative_path_map = self
            .database
            .relative_paths_for_notes(&missing_ids)
            .context("failed to load note paths for hybrid search results")?;

        let mut excerpt_map = HashMap::new();
        for note_id in &missing_ids {
            let excerpt = self
                .database
                .note_excerpt(note_id, 240)
                .context("failed to fetch note excerpt")?;
            excerpt_map.insert(note_id.clone(), excerpt);
        }

        let mut results = Vec::new();
        for (note_id, entry) in combined.into_iter() {
            match (entry.fts, entry.semantic) {
                (Some(mut fts), semantic) => {
                    let semantic = semantic.unwrap_or(0.0);
                    let combined_score = FTS_WEIGHT * fts.score + SEM_WEIGHT * semantic;
                    if semantic > 0.0 && combined_score < self.config.semantic_threshold {
                        continue;
                    }
                    let base_fts_score = fts.score;
                    fts.score = combined_score;
                    fts.reason = if semantic > 0.0 {
                        Some(format!(
                            "Hybrid match (FTS {:.2}, semantic {:.2})",
                            base_fts_score, semantic
                        ))
                    } else {
                        Some(format!("Full-text match (score {:.2})", base_fts_score))
                    };
                    results.push(fts);
                }
                (None, Some(semantic)) => {
                    let combined_score = SEM_WEIGHT * semantic;
                    if combined_score >= self.config.semantic_threshold {
                        let metadata = metadata_map.get(&note_id).cloned().unwrap_or_default();
                        let title = title_map.get(&note_id).cloned().unwrap_or(None);
                        let relative_path = relative_path_map.get(&note_id).cloned();
                        let preview = excerpt_map.remove(&note_id).unwrap_or(None);
                        results.push(SearchResult {
                            note_id,
                            score: combined_score,
                            bm25: f32::MAX,
                            relative_path,
                            preview,
                            reason: Some(format!("Semantic similarity {:.2}", semantic)),
                            metadata,
                            title,
                        });
                    }
                }
                (None, None) => {}
            }
        }

        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
        if results.len() > limit {
            results.truncate(limit);
        }
        info!(
            query = query,
            result_count = results.len(),
            "hybrid search completed"
        );
        Ok(results)
    }

    /// Access the current search configuration.
    pub fn config(&self) -> &SearchConfig {
        &self.config
    }
}

fn rank_to_score(rank: f64) -> f32 {
    if !rank.is_finite() {
        return 0.0;
    }

    let adjusted = rank.max(0.0);
    (-adjusted).exp() as f32
}

fn collect_excluded_ids(database: &IndexDatabase, excludes: &[String]) -> Result<HashSet<String>> {
    let mut combined = HashSet::new();
    for expr in excludes {
        let ids = database
            .matching_note_ids(expr)
            .with_context(|| format!("failed to evaluate NOT clause `{expr}`"))?;
        combined.extend(ids);
    }
    Ok(combined)
}

fn semantic_embedding_text(fts: &str) -> String {
    let mut text = fts.to_string();
    for pattern in [
        "{content metadata} :",
        "metadata:",
        "content:",
        "AND",
        "OR",
        "NOT",
    ] {
        text = text.replace(pattern, " ");
    }

    text = text.replace(['{', '}', '(', ')', '"'], " ");
    text = text.replace(':', " ");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, fs, path::Path, sync::Arc};

    use tempfile::TempDir;

    use crate::{
        indexer::{Indexer, IndexerConfig},
        sqlite::IndexDatabase,
        vault::{Vault, VaultConfig},
    };

    fn fixture_vault() -> Arc<Vault> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tests")
            .join("fixtures")
            .join("test-vault");
        Arc::new(Vault::new(VaultConfig::new(root)).expect("vault initialises"))
    }

    #[tokio::test]
    async fn fts_search_supports_field_and_boolean_queries() {
        let vault = fixture_vault();
        let db_dir = TempDir::new().expect("tempdir");
        let db_path = db_dir.path().join("index.db");
        let database = Arc::new(IndexDatabase::open(&db_path).expect("database opens"));
        let indexer = Indexer::new(
            Arc::clone(&vault),
            Arc::clone(&database),
            IndexerConfig::default(),
            None,
        );
        indexer.index_all().await.expect("indexing succeeds");

        let service = SearchService::new(database, SearchConfig::default(), None);
        let results = service
            .search_fts("category:reference AND tags:photography", Some(10))
            .await
            .expect("search succeeds");

        let ids: HashSet<_> = results.iter().map(|r| r.note_id.as_str()).collect();
        assert!(ids.contains("Photography Equipment"));
        assert!(results.iter().all(|result| result.score >= 0.0));
        assert!(results.iter().all(|result| {
            result
                .reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with("Full-text match"))
        }));
    }

    #[tokio::test]
    async fn fts_search_applies_metadata_date_filters() {
        let vault = fixture_vault();
        let db_dir = TempDir::new().expect("tempdir");
        let db_path = db_dir.path().join("index.db");
        let database = Arc::new(IndexDatabase::open(&db_path).expect("database opens"));
        let indexer = Indexer::new(
            Arc::clone(&vault),
            Arc::clone(&database),
            IndexerConfig::default(),
            None,
        );
        indexer.index_all().await.expect("indexing succeeds");

        let service = SearchService::new(database, SearchConfig::default(), None);
        let results = service
            .search_fts("metadata AND date:2024-01-01..2024-01-31", Some(10))
            .await
            .expect("search succeeds");
        assert!(
            results
                .iter()
                .any(|result| result.note_id == "Complex Metadata Types")
        );

        let empty = service
            .search_fts("metadata AND date:2023-01-01..2023-12-31", Some(10))
            .await
            .expect("search succeeds");
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn fts_search_applies_modified_filters() {
        let vault_dir = TempDir::new().expect("vault");
        let note_path = vault_dir.path().join("Recent.md");
        fs::write(
            &note_path,
            "---\ntitle: Recent Note\n---\n\nThis is a recent note for testing.",
        )
        .expect("write note");

        let vault = Arc::new(
            Vault::new(VaultConfig::new(vault_dir.path().to_path_buf()))
                .expect("vault initialises"),
        );
        vault.ensure_arrowhead_dirs().expect("arrowhead dirs");
        let db_path = vault.paths().arrowhead_dir.join("index.db");
        let database = Arc::new(IndexDatabase::open(&db_path).expect("database opens"));
        let indexer = Indexer::new(
            Arc::clone(&vault),
            Arc::clone(&database),
            IndexerConfig::default(),
            None,
        );
        indexer.index_all().await.expect("indexing succeeds");

        let service = SearchService::new(database, SearchConfig::default(), None);
        let recent = service
            .search_fts("recent AND modified:past7d", Some(5))
            .await
            .expect("search succeeds");
        assert!(recent.iter().any(|result| result.note_id == "Recent"));

        let stale = service
            .search_fts("recent AND modified:<2000-01-01", Some(5))
            .await
            .expect("search succeeds");
        assert!(stale.is_empty());
    }

    #[tokio::test]
    async fn fts_search_supports_not_operator() {
        let vault_dir = TempDir::new().expect("vault");
        let analog_path = vault_dir.path().join("Analog.md");
        fs::write(
            &analog_path,
            "---\ntitle: Analog Photography\n---\n\nAnalog photography gear and film notes.",
        )
        .expect("write note");
        let digital_path = vault_dir.path().join("Digital.md");
        fs::write(
            &digital_path,
            "---\ntitle: Digital Photography\n---\n\nDigital photography workflow and sensors.",
        )
        .expect("write note");

        let vault = Arc::new(
            Vault::new(VaultConfig::new(vault_dir.path().to_path_buf()))
                .expect("vault initialises"),
        );
        vault.ensure_arrowhead_dirs().expect("arrowhead dirs");
        let db_path = vault.paths().arrowhead_dir.join("index.db");
        let database = Arc::new(IndexDatabase::open(&db_path).expect("database opens"));
        let indexer = Indexer::new(
            Arc::clone(&vault),
            Arc::clone(&database),
            IndexerConfig::default(),
            None,
        );
        indexer.index_all().await.expect("indexing succeeds");

        let service = SearchService::new(database, SearchConfig::default(), None);
        let results = service
            .search_fts("photography NOT digital", Some(10))
            .await
            .expect("search succeeds");
        assert!(results.iter().any(|r| r.note_id == "Analog"));
        assert!(results.iter().all(|r| r.note_id != "Digital"));

        let negative_only = service
            .search_fts("NOT digital", Some(10))
            .await
            .expect("negative-only search succeeds");
        assert!(negative_only.iter().all(|r| r.note_id != "Digital"));
    }

    #[tokio::test]
    async fn filter_only_queries_return_results() {
        let vault_dir = TempDir::new().expect("vault");
        let note_path = vault_dir.path().join("Journal.md");
        fs::write(
            &note_path,
            "---\ntitle: Daily Journal\n---\n\nNotes about the day.",
        )
        .expect("write note");

        let vault = Arc::new(
            Vault::new(VaultConfig::new(vault_dir.path().to_path_buf()))
                .expect("vault initialises"),
        );
        vault.ensure_arrowhead_dirs().expect("arrowhead dirs");
        let db_path = vault.paths().arrowhead_dir.join("index.db");
        let database = Arc::new(IndexDatabase::open(&db_path).expect("database opens"));
        let indexer = Indexer::new(
            Arc::clone(&vault),
            Arc::clone(&database),
            IndexerConfig::default(),
            None,
        );
        indexer.index_all().await.expect("indexing succeeds");

        let service = SearchService::new(database, SearchConfig::default(), None);
        let results = service
            .search_fts("modified:past30d", Some(5))
            .await
            .expect("filter-only search succeeds");
        assert!(!results.is_empty());
        assert_eq!(results[0].note_id, "Journal");
        assert_eq!(results[0].reason.as_deref(), Some("Filter match"));
    }

    #[tokio::test]
    async fn empty_query_is_rejected() {
        let vault = fixture_vault();
        let db_dir = TempDir::new().expect("tempdir");
        let db_path = db_dir.path().join("index.db");
        let database = Arc::new(IndexDatabase::open(&db_path).expect("database opens"));
        let indexer = Indexer::new(
            Arc::clone(&vault),
            Arc::clone(&database),
            IndexerConfig::default(),
            None,
        );
        indexer.index_all().await.expect("indexing succeeds");

        let service = SearchService::new(database, SearchConfig::default(), None);
        let err = service
            .search_fts("   ", Some(5))
            .await
            .expect_err("empty query");
        assert!(err.to_string().contains("empty"));
    }
}
