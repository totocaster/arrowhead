//! Search coordination across FTS, semantic, and hybrid strategies.

use std::{
    cmp::Ordering,
    collections::HashMap,
    sync::{Arc, LazyLock},
};

use anyhow::{Context, Result, bail};
use regex::Regex;
use tokio::task;
use tracing::{debug, info};

use crate::{MetadataMap, NoteId, embeddings::EmbeddingPipeline, sqlite::IndexDatabase};

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

        let limit = _limit.unwrap_or(self.config.default_limit);
        let limit = limit.max(1);
        let rewritten = process_query(query);
        let database = Arc::clone(&self.database);
        info!(query = query, limit, "executing full-text search");
        debug!(
            query = query,
            rewritten = rewritten.as_str(),
            "rewrote query for FTS"
        );

        let results = task::spawn_blocking(move || -> Result<Vec<SearchResult>> {
            let matches = database.search_fts(&rewritten, limit)?;
            let note_ids: Vec<String> = matches.iter().map(|item| item.note_id.clone()).collect();
            let metadata_maps = database.metadata_for_notes(&note_ids)?;

            let mut results = Vec::with_capacity(matches.len());
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

            Ok(results)
        })
        .await
        .context("search task aborted")??;

        let result_count = results.len();
        info!(query = query, result_count, "full-text search completed");
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

        let pipeline = match self.embeddings.as_ref() {
            Some(pipeline) => pipeline,
            None => bail!("semantic search requires embeddings to be enabled"),
        };

        let limit = limit.unwrap_or(self.config.default_limit).max(1);
        info!(query = query, limit, "executing semantic search");
        debug!(
            query = query,
            model = pipeline.descriptor().identifier(),
            "using embedding pipeline for semantic search"
        );
        let query_vector = pipeline
            .generator()
            .embed_query(query)
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
        let metadata_maps = self
            .database
            .metadata_for_notes(&note_ids)
            .context("failed to load metadata for semantic results")?;
        let title_map = self
            .database
            .titles_for_notes(&note_ids)
            .context("failed to load note titles for semantic results")?;
        let relative_path_map = self
            .database
            .relative_paths_for_notes(&note_ids)
            .context("failed to load note paths for semantic results")?;

        let mut results = Vec::new();
        for item in matches.into_iter().take(limit) {
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

        let pipeline = match self.embeddings.as_ref() {
            Some(pipeline) => pipeline,
            None => bail!("hybrid search requires embeddings to be enabled"),
        };

        let limit = limit.unwrap_or(self.config.default_limit).max(1);
        info!(query = query, limit, "executing hybrid search");
        debug!(
            query = query,
            model = pipeline.descriptor().identifier(),
            "using embedding pipeline for hybrid search"
        );

        let query_vector = pipeline
            .generator()
            .embed_query(query)
            .context("failed to embed search query")?;

        let semantic_matches = pipeline
            .store()
            .search(&query_vector, limit * 3, self.config.semantic_threshold)
            .await
            .context("semantic vector search failed")?;

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
            let similarity = (1.0_f32 - item.distance).max(0.0_f32);
            if similarity <= 0.0 {
                continue;
            }
            combined.entry(item.note_id.clone()).or_default().semantic = Some(similarity);
        }

        let missing_ids: Vec<String> = combined
            .iter()
            .filter_map(|(note_id, entry)| {
                (entry.fts.is_none() && entry.semantic.is_some()).then(|| note_id.clone())
            })
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

const FIELD_PATTERN_STR: &str = "(?P<field>[A-Za-z0-9_]+):(?P<value>\"[^\"]*\"|\\S+)";

fn process_query(query: &str) -> String {
    static FIELD_PATTERN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(FIELD_PATTERN_STR).expect("valid field:value regex"));

    let trimmed = query.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let mut matches = Vec::new();
    for captures in FIELD_PATTERN.captures_iter(trimmed) {
        let full = captures.get(0).unwrap();
        let field = captures.name("field").unwrap().as_str();
        let value = captures.name("value").unwrap().as_str();
        matches.push((
            full.start(),
            full.end(),
            field.to_string(),
            value.to_string(),
        ));
    }

    let has_field_patterns = !matches.is_empty();
    let mut processed = trimmed.to_string();

    for (start, end, field, raw_value) in matches.into_iter().rev() {
        let was_quoted = raw_value.starts_with('"') && raw_value.ends_with('"');
        let inner = if was_quoted && raw_value.len() >= 2 {
            raw_value[1..raw_value.len() - 1].to_string()
        } else {
            raw_value.clone()
        };

        if inner.contains("://") {
            continue;
        }

        let replacement = if field == "content" {
            if was_quoted {
                let escaped = inner.replace('"', "\"\"");
                format!("content:\"{}\"", escaped)
            } else {
                format!("content:{}", escape_fts_query(&inner))
            }
        } else {
            let token = format!("{}:{}", field, inner);
            let escaped = token.replace('"', "\"\"");
            format!("metadata:\"{}\"", escaped)
        };

        processed.replace_range(start..end, &replacement);
    }

    if !has_field_patterns {
        return format!("{{content metadata}} : {}", escape_fts_query(trimmed));
    }

    sanitize_mixed_query(&processed)
}

fn sanitize_mixed_query(processed: &str) -> String {
    const METADATA_PREFIX: &str = "metadata:";
    const CONTENT_PREFIX: &str = "content:";

    let chars: Vec<(usize, char)> = processed.char_indices().collect();
    let mut result = String::with_capacity(processed.len());
    let mut index = 0usize;

    while index < chars.len() {
        let (byte_index, ch) = chars[index];

        if ch.is_whitespace() {
            result.push(ch);
            index += 1;
            continue;
        }

        let remaining = &processed[byte_index..];
        if remaining.starts_with(METADATA_PREFIX) {
            result.push_str(METADATA_PREFIX);
            index += METADATA_PREFIX.chars().count();
            index = copy_field_value(&mut result, &chars, index);
            continue;
        }

        if remaining.starts_with(CONTENT_PREFIX) {
            result.push_str(CONTENT_PREFIX);
            index += CONTENT_PREFIX.chars().count();
            index = copy_field_value(&mut result, &chars, index);
            continue;
        }

        let start_byte = byte_index;
        let mut next = index;
        while next < chars.len() && !chars[next].1.is_whitespace() {
            next += 1;
        }

        let end_byte = if next < chars.len() {
            chars[next].0
        } else {
            processed.len()
        };

        let token = processed[start_byte..end_byte].trim();
        if !token.is_empty() {
            result.push_str(&escape_fts_query(token));
        }

        index = next;
    }

    result.trim().to_string()
}

fn copy_field_value(result: &mut String, chars: &[(usize, char)], mut index: usize) -> usize {
    if index >= chars.len() {
        return index;
    }

    if chars[index].1 == '"' {
        result.push('"');
        index += 1;
        let mut escaped = false;
        while index < chars.len() {
            let ch = chars[index].1;
            result.push(ch);
            if ch == '"' && !escaped {
                index += 1;
                break;
            }
            escaped = ch == '\\' && !escaped;
            index += 1;
        }
    } else {
        while index < chars.len() && !chars[index].1.is_whitespace() {
            result.push(chars[index].1);
            index += 1;
        }
    }

    index
}

fn escape_fts_query(query: &str) -> String {
    static HYPHEN_PATTERN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b[\w]+-[\w-]+\b").expect("valid hyphen regex"));

    let trimmed = query.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() >= 2 {
        return trimmed.to_string();
    }

    let has_hyphenated_terms = HYPHEN_PATTERN.is_match(trimmed);
    let has_special_chars = trimmed
        .chars()
        .any(|ch| matches!(ch, '[' | ']' | '(' | ')' | '"' | '*'));
    let starts_with_dash = trimmed.starts_with('-');
    let contains_boolean =
        trimmed.contains(" AND ") || trimmed.contains(" OR ") || trimmed.contains(" NOT ");

    if has_hyphenated_terms || has_special_chars || starts_with_dash || contains_boolean {
        let escaped = trimmed.replace('"', "\"\"");
        format!("\"{}\"", escaped)
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, path::Path, sync::Arc};

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

    #[test]
    fn escape_quotes_and_hyphenated_terms() {
        assert_eq!(escape_fts_query("2025-10-06"), "\"2025-10-06\"");
        assert_eq!(escape_fts_query("out-of-memory"), "\"out-of-memory\"");
        assert_eq!(escape_fts_query("simple"), "simple");
        assert_eq!(escape_fts_query("\"already quoted\""), "\"already quoted\"");
    }

    #[test]
    fn escape_special_characters_and_booleans() {
        assert_eq!(escape_fts_query("test[123]"), "\"test[123]\"");
        assert_eq!(escape_fts_query("test(abc)"), "\"test(abc)\"");
        assert_eq!(escape_fts_query("test*"), "\"test*\"");
        assert_eq!(escape_fts_query("foo AND bar"), "\"foo AND bar\"");
    }

    #[test]
    fn process_simple_queries() {
        assert_eq!(
            process_query("simple query"),
            "{content metadata} : simple query"
        );
        assert_eq!(
            process_query("2025-10-06"),
            "{content metadata} : \"2025-10-06\""
        );
    }

    #[test]
    fn process_content_fields() {
        assert_eq!(process_query("content:swift"), "content:swift");
        assert_eq!(
            process_query("content:\"async await\""),
            "content:\"async await\""
        );
        assert_eq!(
            process_query("content:2025-10-06"),
            "content:\"2025-10-06\""
        );
    }

    #[test]
    fn process_metadata_fields() {
        assert_eq!(
            process_query("category:project"),
            "metadata:\"category:project\""
        );
        assert_eq!(process_query("tags:swift"), "metadata:\"tags:swift\"");
    }

    #[test]
    fn process_multiple_fields_and_plain_text() {
        let query = "category:project tags:swift";
        let result = process_query(query);
        assert!(result.contains("metadata:\"category:project\""));
        assert!(result.contains("metadata:\"tags:swift\""));

        let mixed = process_query("category:project machine learning");
        assert!(mixed.contains("metadata:\"category:project\""));
        assert!(mixed.contains("machine learning"));
    }

    #[test]
    fn process_quoted_and_date_values() {
        assert_eq!(
            process_query("category:\"my project\""),
            "metadata:\"category:my project\""
        );

        let result = process_query("category:project 2025-10-06");
        assert!(result.contains("metadata:\"category:project\""));
        assert!(result.contains("\"2025-10-06\""));
    }

    #[test]
    fn process_leaves_urls_intact() {
        let result = process_query("http://example.com content:camera");
        assert!(result.contains("http://example.com"));
        assert!(result.contains("content:camera"));
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
