//! Embedding generation, configuration, and persistence.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt, fs, mem,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use rusqlite::{Connection, OptionalExtension, params};
use tokio::task;
use tracing::{debug, info};
use zerocopy::AsBytes;

use crate::{
    Vault,
    sqlite::{IndexDatabase, NoteIndexState},
};

/// User-facing presets for embedding model quality/speed trade-offs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingPreset {
    /// Fastest option, smaller vectors, good default quality.
    Fast,
    /// Balanced option with improved quality over the fastest preset.
    Good,
    /// Higher dimensional model with better semantic fidelity at additional cost.
    Better,
}

impl EmbeddingPreset {
    /// Canonical identifier exposed to configuration.
    pub fn identifier(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Good => "good",
            Self::Better => "better",
        }
    }

    /// Hugging Face repository backing the preset.
    pub fn repository(self) -> &'static str {
        match self {
            Self::Fast => "sentence-transformers/all-MiniLM-L6-v2",
            Self::Good => "BAAI/bge-small-en-v1.5",
            Self::Better => "BAAI/bge-base-en-v1.5",
        }
    }

    /// Associated fastembed model variant.
    pub fn embedding_model(self) -> EmbeddingModel {
        match self {
            Self::Fast => EmbeddingModel::AllMiniLML6V2,
            Self::Good => EmbeddingModel::BGESmallENV15,
            Self::Better => EmbeddingModel::BGEBaseENV15,
        }
    }

    /// Output vector dimensionality for the preset.
    pub fn dimension(self) -> usize {
        match self {
            Self::Fast => 384,
            Self::Good => 384,
            Self::Better => 768,
        }
    }

    /// Parse a preset from a user-supplied identifier.
    pub fn from_identifier(value: &str) -> Result<Self> {
        let normalized = value.trim().to_ascii_lowercase();
        Ok(match normalized.as_str() {
            "fast" | "mini" | "all-minilm-l6-v2" | "sentence-transformers/all-minilm-l6-v2" => {
                Self::Fast
            }
            "good"
            | "bge-small"
            | "bge-small-en"
            | "bge-small-en-v1.5"
            | "baai/bge-small-en-v1.5" => Self::Good,
            "better"
            | "bge-base"
            | "bge-base-en"
            | "bge-base-en-v1.5"
            | "baai/bge-base-en-v1.5" => Self::Better,
            other => bail!("unknown embedding preset or model identifier `{other}`"),
        })
    }

    /// All supported presets in declaration order.
    pub fn all() -> &'static [Self] {
        &[Self::Fast, Self::Good, Self::Better]
    }
}

impl std::fmt::Display for EmbeddingPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.identifier())
    }
}

/// Descriptor combining preset metadata with resolved model details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingDescriptor {
    preset: EmbeddingPreset,
    identifier: String,
    repository: String,
    dimension: usize,
    model: EmbeddingModel,
}

impl EmbeddingDescriptor {
    /// Resolve a descriptor from a preset identifier or model alias.
    pub fn resolve(identifier: &str) -> Result<Self> {
        let preset = EmbeddingPreset::from_identifier(identifier)?;
        Ok(Self {
            preset,
            identifier: preset.identifier().to_string(),
            repository: preset.repository().to_string(),
            dimension: preset.dimension(),
            model: preset.embedding_model(),
        })
    }

    /// Access the resolved preset.
    pub fn preset(&self) -> EmbeddingPreset {
        self.preset
    }

    /// Canonical identifier string for this descriptor.
    pub fn identifier(&self) -> &str {
        &self.identifier
    }

    /// Hugging Face repository backing the descriptor.
    pub fn repository(&self) -> &str {
        &self.repository
    }

    /// Output vector dimensionality.
    pub fn dimension(&self) -> usize {
        self.dimension
    }

    /// fastembed model enum.
    pub fn model(&self) -> &EmbeddingModel {
        &self.model
    }
}

/// Configuration for instantiating an embedding generator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingConfig {
    descriptor: EmbeddingDescriptor,
    cache_dir: PathBuf,
    max_length: Option<usize>,
    show_download_progress: bool,
}

impl EmbeddingConfig {
    /// Construct a configuration for the supplied descriptor and cache directory.
    pub fn new(descriptor: EmbeddingDescriptor, cache_dir: PathBuf) -> Self {
        Self {
            descriptor,
            cache_dir,
            max_length: None,
            show_download_progress: true,
        }
    }

    /// Cache directory used for model artifacts.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Descriptor describing the chosen preset/model.
    pub fn descriptor(&self) -> &EmbeddingDescriptor {
        &self.descriptor
    }

    /// Override the maximum token length accepted by the model.
    pub fn with_max_length(mut self, max_length: Option<usize>) -> Self {
        self.max_length = max_length;
        self
    }

    /// Control whether download progress is surfaced to stdout.
    pub fn with_download_progress(mut self, enabled: bool) -> Self {
        self.show_download_progress = enabled;
        self
    }
}

/// Wrapper around `fastembed` for embedding generation.
#[derive(Clone)]
pub struct EmbeddingGenerator {
    config: EmbeddingConfig,
    pool: Arc<ModelPool>,
}

impl EmbeddingGenerator {
    /// Load the configured embedding model, downloading weights as needed.
    pub fn initialise(config: EmbeddingConfig) -> Result<Self> {
        info!(
            model = config.descriptor().identifier(),
            cache_dir = %config.cache_dir.display(),
            "initialising embedding generator"
        );
        if let Some(parent) = config.cache_dir.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create embedding cache directory {}",
                    parent.display()
                )
            })?;
        }
        fs::create_dir_all(&config.cache_dir).with_context(|| {
            format!(
                "failed to create embedding cache directory {}",
                config.cache_dir.display()
            )
        })?;

        let pool_size = embedding_pool_size();
        debug!(
            model = config.descriptor().identifier(),
            pool_size, "preparing embedding model pool"
        );
        let mut models = Vec::with_capacity(pool_size);

        for _ in 0..pool_size {
            let mut options = TextInitOptions::new(config.descriptor.model().clone())
                .with_cache_dir(config.cache_dir.clone())
                .with_show_download_progress(config.show_download_progress);
            if let Some(max_length) = config.max_length {
                options = options.with_max_length(max_length);
            }

            let model = TextEmbedding::try_new(options)
                .context("failed to initialise embedding model via fastembed")?;
            models.push(model);
        }

        let generator = Self {
            config,
            pool: Arc::new(ModelPool::new(models)),
        };
        info!(
            model = generator.config.descriptor().identifier(),
            pool_size, "embedding generator ready"
        );
        Ok(generator)
    }

    /// Access the generator configuration.
    pub fn config(&self) -> &EmbeddingConfig {
        &self.config
    }

    /// Generate embeddings for multiple documents.
    pub fn embed_documents(&self, documents: &[String]) -> Result<Vec<Vec<f32>>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }

        // fastembed accepts `Vec<S: AsRef<str>>`, so we can avoid cloning all input strings.
        let inputs: Vec<&str> = documents.iter().map(|doc| doc.as_str()).collect();
        let mut lease = self.pool.checkout();
        let embeddings = lease
            .embed(inputs, None)
            .context("failed to embed documents")?;

        embeddings.into_iter().map(normalize_vector).collect()
    }

    /// Generate an embedding for a single document.
    pub fn embed_document(&self, document: &str) -> Result<Vec<f32>> {
        let mut lease = self.pool.checkout();
        let mut embeddings = lease
            .embed(vec![document], None)
            .context("failed to embed document")?;
        let vector = embeddings
            .pop()
            .ok_or_else(|| anyhow!("document embedding missing"))?;
        normalize_vector(vector)
    }

    /// Generate an embedding suitable for semantic query comparisons.
    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let mut lease = self.pool.checkout();
        let mut embeddings = lease
            .embed(vec![query], None)
            .context("failed to embed query")?;
        let vector = embeddings
            .pop()
            .ok_or_else(|| anyhow!("query embedding missing"))?;
        normalize_vector(vector)
    }
}

/// Number of model instances kept alive for concurrent embedding.
fn embedding_pool_size() -> usize {
    num_cpus::get().clamp(1, 8)
}

struct ModelPool {
    models: Mutex<Vec<TextEmbedding>>,
    available: Condvar,
}

impl ModelPool {
    fn new(models: Vec<TextEmbedding>) -> Self {
        debug_assert!(!models.is_empty(), "embedding pool must not be empty");
        Self {
            models: Mutex::new(models),
            available: Condvar::new(),
        }
    }

    fn checkout(&self) -> ModelLease<'_> {
        let mut guard = self.models.lock().expect("embedding pool poisoned");
        loop {
            if let Some(model) = guard.pop() {
                return ModelLease {
                    pool: self,
                    model: Some(model),
                };
            }
            guard = self
                .available
                .wait(guard)
                .expect("embedding pool wait poisoned");
        }
    }

    fn checkin(&self, model: TextEmbedding) {
        let mut guard = self.models.lock().expect("embedding pool poisoned");
        guard.push(model);
        self.available.notify_one();
    }
}

struct ModelLease<'a> {
    pool: &'a ModelPool,
    model: Option<TextEmbedding>,
}

impl std::ops::Deref for ModelLease<'_> {
    type Target = TextEmbedding;

    fn deref(&self) -> &Self::Target {
        self.model
            .as_ref()
            .expect("embedding model lease missing instance")
    }
}

impl std::ops::DerefMut for ModelLease<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.model
            .as_mut()
            .expect("embedding model lease missing instance")
    }
}

impl Drop for ModelLease<'_> {
    fn drop(&mut self) {
        if let Some(model) = self.model.take() {
            self.pool.checkin(model);
        }
    }
}

fn normalize_vector(mut vector: Vec<f32>) -> Result<Vec<f32>> {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm <= f32::EPSILON {
        bail!("embedding vector norm is zero; cannot normalise");
    }
    for value in vector.iter_mut() {
        *value /= norm;
    }
    Ok(vector)
}

pub(crate) const EMBEDDING_TABLE_NAME: &str = "note_embeddings";
const EMBEDDING_METADATA_SINGLETON: i64 = 1;

/// Embedding payload captured during indexing for a single note.
#[derive(Debug, Clone)]
pub struct EmbeddingRecord {
    /// Identifier of the note the vector belongs to.
    pub note_id: String,
    /// Normalised embedding vector produced by fastembed.
    pub vector: Vec<f32>,
    /// Index generation of the note text this vector was computed from. The vector is
    /// current only while this equals the note row's `indexed_at`.
    pub indexed_at: DateTime<Utc>,
}

/// Result row returned from sqlite-vec vector search.
#[derive(Debug, Clone)]
pub struct EmbeddingMatch {
    /// Note identifier matched by the vector search.
    pub note_id: String,
    /// Raw cosine distance reported by sqlite-vec (lower is closer).
    pub distance: f32,
}

struct EmbeddingPipelineInner {
    descriptor: EmbeddingDescriptor,
    generator: Arc<EmbeddingGenerator>,
    store: Arc<EmbeddingStore>,
    model_changed: bool,
}

/// Combined embedding generator + persistence pipeline.
#[derive(Clone)]
pub struct EmbeddingPipeline {
    inner: Arc<EmbeddingPipelineInner>,
}

impl EmbeddingPipeline {
    /// Semantic embeddings are always available now that sqlite-vec ships in every build.
    pub fn is_supported() -> bool {
        true
    }

    /// Prepare the embedding pipeline for the supplied vault and model identifier.
    pub async fn initialise(
        vault: &Vault,
        database: Arc<IndexDatabase>,
        model_id: &str,
    ) -> Result<Self> {
        let descriptor = EmbeddingDescriptor::resolve(model_id)?;
        let vault_paths = vault.paths();
        let models_dir = vault_paths
            .arrowhead_dir
            .join("models")
            .join(descriptor.identifier());

        info!(
            model = descriptor.identifier(),
            models_dir = %models_dir.display(),
            "initialising embedding pipeline"
        );

        fs::create_dir_all(&models_dir).with_context(|| {
            format!(
                "failed to create embedding model directory {}",
                models_dir.display()
            )
        })?;

        let generator = EmbeddingGenerator::initialise(EmbeddingConfig::new(
            descriptor.clone(),
            models_dir.clone(),
        ))?;
        let (store, model_changed) =
            EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor).await?;

        let pipeline = Self {
            inner: Arc::new(EmbeddingPipelineInner {
                descriptor,
                generator: Arc::new(generator),
                store: Arc::new(store),
                model_changed,
            }),
        };
        info!(
            model = pipeline.descriptor().identifier(),
            reset = model_changed,
            "embedding pipeline ready"
        );
        Ok(pipeline)
    }

    /// Access the generator component.
    pub fn generator(&self) -> &EmbeddingGenerator {
        self.inner.generator.as_ref()
    }

    /// Access the embedding store.
    pub fn store(&self) -> &EmbeddingStore {
        self.inner.store.as_ref()
    }

    /// Descriptor describing the active model.
    pub fn descriptor(&self) -> &EmbeddingDescriptor {
        &self.inner.descriptor
    }

    /// Whether the pipeline had to reset embeddings due to a model change.
    pub fn model_changed(&self) -> bool {
        self.inner.model_changed
    }
}

impl fmt::Debug for EmbeddingPipeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmbeddingPipeline")
            .field("model", &self.descriptor().identifier())
            .field("model_changed", &self.model_changed())
            .finish()
    }
}

/// sqlite-vec backed note embedding store.
#[derive(Clone)]
pub struct EmbeddingStore {
    database: Arc<IndexDatabase>,
    dimension: usize,
    model_id: String,
}

impl EmbeddingStore {
    /// Ensure the sqlite-vec table exists and matches the requested descriptor.
    pub async fn bootstrap(
        database: Arc<IndexDatabase>,
        descriptor: &EmbeddingDescriptor,
    ) -> Result<(Self, bool)> {
        let descriptor_clone = descriptor.clone();
        let db_for_task = Arc::clone(&database);
        let dispatch = tracing::dispatcher::get_default(|current| current.clone());
        let model_changed = task::spawn_blocking(move || -> Result<bool> {
            tracing::dispatcher::with_default(&dispatch, || {
                let mut conn = db_for_task
                    .connection()
                    .context("failed to open SQLite connection for embeddings")?;
                let tx = conn
                    .transaction()
                    .context("failed to open transaction for embedding bootstrap")?;
                let changed = ensure_embedding_schema(&tx, &descriptor_clone)?;
                tx.commit()
                    .context("failed to commit embedding bootstrap transaction")?;
                Ok(changed)
            })
        })
        .await
        .context("embedding bootstrap task aborted")??;

        let store = Self {
            database,
            dimension: descriptor.dimension(),
            model_id: descriptor.identifier().to_string(),
        };
        Ok((store, model_changed))
    }

    /// Persist (or replace) embeddings for the supplied records.
    pub async fn upsert_embeddings(&self, records: &[EmbeddingRecord]) -> Result<()> {
        if records.is_empty() {
            debug!(
                table = EMBEDDING_TABLE_NAME,
                "skipping embedding upsert for empty batch"
            );
            return Ok(());
        }

        let entries = records
            .iter()
            .map(|record| {
                ensure_dimension(&record.vector, self.dimension)?;
                let blob = vector_to_blob(&record.vector);
                let timestamp = record.indexed_at.timestamp_micros();
                Ok((record.note_id.clone(), blob, timestamp))
            })
            .collect::<Result<Vec<_>>>()?;

        let database = Arc::clone(&self.database);
        let model_id = self.model_id.clone();
        let count = entries.len();

        info!(
            table = EMBEDDING_TABLE_NAME,
            model = model_id.as_str(),
            count,
            "upserting embeddings batch"
        );

        task::spawn_blocking(move || -> Result<()> {
            let mut conn = database
                .connection()
                .context("failed to open SQLite connection for embedding upsert")?;
            let tx = conn
                .transaction()
                .context("failed to open transaction for embedding upsert")?;

            {
                let mut delete_stmt = tx.prepare_cached(&format!(
                    "DELETE FROM {EMBEDDING_TABLE_NAME} WHERE note_id = ?1"
                ))?;
                for (note_id, _, _) in &entries {
                    delete_stmt
                        .execute([note_id])
                        .context("failed to delete existing embedding row")?;
                }
            }

            {
                let mut insert_stmt = tx.prepare_cached(&format!(
                    "INSERT INTO {EMBEDDING_TABLE_NAME} (note_id, vector, model, indexed_at) VALUES (?1, ?2, ?3, ?4)"
                ))?;
                for (note_id, blob, timestamp) in &entries {
                    insert_stmt.execute(params![
                        note_id,
                        blob.as_slice(),
                        &model_id,
                        timestamp
                    ])?;
                }
            }

            tx.commit()
                .context("failed to commit embedding upsert transaction")?;
            Ok(())
        })
        .await
        .context("embedding upsert task aborted")??;

        Ok(())
    }

    /// Remove stored embeddings for the supplied note identifiers.
    pub async fn delete_embeddings(&self, note_ids: &[String]) -> Result<()> {
        if note_ids.is_empty() {
            return Ok(());
        }

        let unique_ids: Vec<String> = {
            let mut set = BTreeSet::new();
            for id in note_ids {
                set.insert(id.clone());
            }
            set.into_iter().collect()
        };

        let database = Arc::clone(&self.database);

        task::spawn_blocking(move || -> Result<()> {
            let mut conn = database
                .connection()
                .context("failed to open SQLite connection for embedding delete")?;
            let tx = conn
                .transaction()
                .context("failed to open transaction for embedding delete")?;
            {
                let mut stmt = tx.prepare_cached(&format!(
                    "DELETE FROM {EMBEDDING_TABLE_NAME} WHERE note_id = ?1"
                ))?;
                for note_id in &unique_ids {
                    stmt.execute([note_id])
                        .context("failed to delete embedding row")?;
                }
            }
            tx.commit()
                .context("failed to commit embedding delete transaction")?;
            Ok(())
        })
        .await
        .context("embedding delete task aborted")??;

        Ok(())
    }

    /// Return the ids in `notes` whose vector for the active model is missing or was written
    /// for a different index generation than the note row's `indexed_at`.
    ///
    /// A text reindex without a committed vector leaves this mismatch behind: an edit indexed
    /// with embeddings disabled, a failed vector flush, or a crash before the flush. The
    /// mismatch is the durable repair marker the indexer acts on.
    pub async fn stale_note_ids(
        &self,
        notes: &HashMap<String, NoteIndexState>,
    ) -> Result<HashSet<String>> {
        let database = Arc::clone(&self.database);
        let model_id = self.model_id.clone();

        let generations = task::spawn_blocking(move || -> Result<HashMap<String, i64>> {
            let conn = database
                .connection()
                .context("failed to open SQLite connection for embedding freshness check")?;
            let mut stmt = conn.prepare(&format!(
                "SELECT note_id, indexed_at FROM {EMBEDDING_TABLE_NAME} WHERE model = ?1"
            ))?;
            let rows = stmt.query_map([&model_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            rows.collect::<rusqlite::Result<HashMap<_, _>>>()
                .context("failed to read embedding generations")
        })
        .await
        .context("embedding freshness task aborted")??;

        Ok(notes
            .iter()
            .filter(|(note_id, state)| {
                generations.get(*note_id) != Some(&state.indexed_at.timestamp_micros())
            })
            .map(|(note_id, _)| note_id.clone())
            .collect())
    }

    /// Retrieve the stored embedding vector for a note, when available.
    pub async fn vector_for_note(&self, note_id: &str) -> Result<Option<Vec<f32>>> {
        let database = Arc::clone(&self.database);
        let note_id = note_id.to_string();
        let model_id = self.model_id.clone();
        let dimension = self.dimension;

        let blob = task::spawn_blocking(move || -> Result<Option<Vec<u8>>> {
            let conn = database
                .connection()
                .context("failed to open SQLite connection for embedding fetch")?;
            let mut stmt = conn.prepare(&format!(
                "SELECT vector FROM {EMBEDDING_TABLE_NAME} \
                 WHERE note_id = ?1 AND model = ?2 \
                 LIMIT 1"
            ))?;
            let blob: Option<Vec<u8>> = stmt
                .query_row(params![&note_id, &model_id], |row| row.get(0))
                .optional()?;
            Ok(blob)
        })
        .await
        .context("embedding fetch task aborted")??;

        match blob {
            Some(bytes) => {
                let vector = blob_to_vector(&bytes, dimension)?;
                ensure_dimension(&vector, dimension)?;
                Ok(Some(vector))
            }
            None => Ok(None),
        }
    }

    /// Execute a cosine similarity search via sqlite-vec.
    ///
    /// Only notes that still have an index row are returned.
    pub async fn search(
        &self,
        query: &[f32],
        limit: usize,
        threshold: f32,
    ) -> Result<Vec<EmbeddingMatch>> {
        ensure_dimension(query, self.dimension)?;
        let blob = vector_to_blob(query);
        let database = Arc::clone(&self.database);
        let model_id = self.model_id.clone();
        let limit = limit.max(1) as i64;

        let matches = task::spawn_blocking(move || -> Result<Vec<EmbeddingMatch>> {
            let conn = database
                .connection()
                .context("failed to open SQLite connection for embedding search")?;
            let mut stmt = conn.prepare(&format!(
                "SELECT note_id, distance \
                 FROM {EMBEDDING_TABLE_NAME} \
                 WHERE vector MATCH ?1 AND model = ?2 \
                 ORDER BY distance \
                 LIMIT ?3"
            ))?;
            let mut note_exists = conn.prepare("SELECT 1 FROM notes WHERE id = ?1")?;
            let mut rows = stmt.query(params![blob.as_slice(), &model_id, limit])?;
            let mut results = Vec::new();
            while let Some(row) = rows.next()? {
                let note_id: String = row.get(0)?;
                if !note_exists.exists([&note_id])? {
                    continue;
                }
                let distance: f32 = row.get(1)?;
                results.push(EmbeddingMatch { note_id, distance });
            }
            Ok(results)
        })
        .await
        .context("embedding search task aborted")??;

        let mut filtered = Vec::new();
        for item in matches {
            let similarity = (1.0 - item.distance).max(0.0);
            if similarity >= threshold {
                filtered.push(item);
            }
        }
        Ok(filtered)
    }
}

#[derive(Debug, Clone)]
struct StoredEmbeddingMetadata {
    model_id: String,
    repository: String,
    dimension: usize,
}

impl StoredEmbeddingMetadata {
    /// Vectors are comparable only when they came from the same preset, model repository,
    /// and dimension.
    fn matches(&self, descriptor: &EmbeddingDescriptor) -> bool {
        self.model_id == descriptor.identifier()
            && self.repository == descriptor.repository()
            && self.dimension == descriptor.dimension()
    }
}

fn ensure_embedding_schema(conn: &Connection, descriptor: &EmbeddingDescriptor) -> Result<bool> {
    let metadata = load_embedding_metadata(conn)?;
    let table_exists = embedding_table_exists(conn)?;
    let mut model_changed = false;

    match (metadata, table_exists) {
        (Some(existing), true) => {
            if !existing.matches(descriptor) {
                drop_embedding_table(conn)?;
                create_embedding_table(conn, descriptor)?;
                model_changed = true;
            }
        }
        (Some(_), false) => {
            create_embedding_table(conn, descriptor)?;
            model_changed = true;
        }
        (None, true) => {
            drop_embedding_table(conn)?;
            create_embedding_table(conn, descriptor)?;
            model_changed = true;
        }
        (None, false) => {
            create_embedding_table(conn, descriptor)?;
        }
    }

    remove_vectors_without_notes(conn)?;
    write_embedding_metadata(conn, descriptor)?;
    Ok(model_changed)
}

/// Delete vectors whose note row is gone. Releases before v0.11.6 could remove a note and keep
/// its vector, so indexes they wrote can still hold such rows.
fn remove_vectors_without_notes(conn: &Connection) -> Result<()> {
    let removed = conn
        .execute(
            &format!(
                "DELETE FROM {EMBEDDING_TABLE_NAME} WHERE note_id NOT IN (SELECT id FROM notes)"
            ),
            [],
        )
        .context("failed to remove embeddings for notes missing from the index")?;
    if removed > 0 {
        info!(
            removed,
            "removed embeddings for notes missing from the index"
        );
    }
    Ok(())
}

fn load_embedding_metadata(conn: &Connection) -> Result<Option<StoredEmbeddingMetadata>> {
    conn.query_row(
        "SELECT model_id, repository, dimension FROM embedding_metadata WHERE singleton = ?1",
        [EMBEDDING_METADATA_SINGLETON],
        |row| {
            let model_id: String = row.get(0)?;
            let repository: String = row.get(1)?;
            let dimension: i64 = row.get(2)?;
            Ok(StoredEmbeddingMetadata {
                model_id,
                repository,
                dimension: dimension as usize,
            })
        },
    )
    .optional()
    .context("failed to load embedding metadata")
}

fn write_embedding_metadata(conn: &Connection, descriptor: &EmbeddingDescriptor) -> Result<()> {
    let timestamp = Utc::now().timestamp_micros();
    conn.execute(
        "INSERT INTO embedding_metadata (singleton, model_id, repository, dimension, updated_at)\n         VALUES (?1, ?2, ?3, ?4, ?5)\n         ON CONFLICT(singleton) DO UPDATE SET\n            model_id = excluded.model_id,\n            repository = excluded.repository,\n            dimension = excluded.dimension,\n            updated_at = excluded.updated_at",
        params![
            EMBEDDING_METADATA_SINGLETON,
            descriptor.identifier(),
            descriptor.repository(),
            descriptor.dimension() as i64,
            timestamp
        ],
    )
    .context("failed to persist embedding metadata")?;
    Ok(())
}

fn embedding_table_exists(conn: &Connection) -> Result<bool> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [EMBEDDING_TABLE_NAME],
            |row| row.get::<_, i32>(0),
        )
        .optional()
        .context("failed to probe sqlite-vec table")?;
    Ok(exists.is_some())
}

fn drop_embedding_table(conn: &Connection) -> Result<()> {
    conn.execute(&format!("DROP TABLE IF EXISTS {EMBEDDING_TABLE_NAME}"), [])
        .context("failed to drop existing embedding table")?;
    Ok(())
}

fn create_embedding_table(conn: &Connection, descriptor: &EmbeddingDescriptor) -> Result<()> {
    let sql = format!(
        "CREATE VIRTUAL TABLE {EMBEDDING_TABLE_NAME} USING vec0(\n            note_id TEXT,\n            vector FLOAT[{dimension}] distance_metric=cosine,\n            model TEXT,\n            indexed_at INTEGER\n        )",
        dimension = descriptor.dimension()
    );
    conn.execute(&sql, [])
        .context("failed to create sqlite-vec embeddings table")?;
    Ok(())
}

fn ensure_dimension(vector: &[f32], expected: usize) -> Result<()> {
    if vector.len() != expected {
        bail!(
            "embedding dimension mismatch: expected {}, got {}",
            expected,
            vector.len()
        );
    }
    Ok(())
}

fn vector_to_blob(vector: &[f32]) -> Vec<u8> {
    vector.as_bytes().to_vec()
}

fn blob_to_vector(blob: &[u8], dimension: usize) -> Result<Vec<f32>> {
    let expected = dimension * mem::size_of::<f32>();
    if blob.len() != expected {
        bail!(
            "embedding blob size mismatch: expected {} bytes, got {}",
            expected,
            blob.len()
        );
    }

    let mut vector = Vec::with_capacity(dimension);
    for chunk in blob.chunks_exact(mem::size_of::<f32>()) {
        let bytes: [u8; 4] = chunk
            .try_into()
            .map_err(|_| anyhow!("failed to decode embedding chunk"))?;
        vector.push(f32::from_ne_bytes(bytes));
    }
    Ok(vector)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::tempdir;

    use crate::{NoteRecord, metadata::MetadataExtraction};

    fn unit_vector(dimension: usize, fill: f32) -> Vec<f32> {
        let mut vec = vec![fill; dimension];
        let norm = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
        for value in vec.iter_mut() {
            *value /= norm;
        }
        vec
    }

    fn axis_vector(dimension: usize, axis: usize) -> Vec<f32> {
        let mut vec = vec![0.0; dimension];
        vec[axis] = 1.0;
        vec
    }

    /// Write a text-index row for `note_id` and return its index generation.
    fn index_note_row(database: &IndexDatabase, note_id: &str) -> DateTime<Utc> {
        let indexed_at = Utc::now();
        let note = NoteRecord::new(note_id, format!("{note_id}.md"), indexed_at, String::new());
        database
            .upsert_note(&note, &MetadataExtraction::default(), &[], indexed_at)
            .expect("index note row");
        indexed_at
    }

    fn vector_record(
        note_id: &str,
        vector: Vec<f32>,
        indexed_at: DateTime<Utc>,
    ) -> EmbeddingRecord {
        EmbeddingRecord {
            note_id: note_id.to_string(),
            vector,
            indexed_at,
        }
    }

    #[tokio::test]
    async fn bootstrap_roundtrip_embeddings() -> Result<()> {
        let dir = tempdir().expect("temp dir");
        let db_path = dir.path().join("index.db");
        let database = Arc::new(IndexDatabase::open(&db_path)?);
        let descriptor = EmbeddingDescriptor::resolve("fast")?;
        let (store, changed) =
            EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor).await?;
        assert!(!changed, "unexpected model change during first bootstrap");

        let timestamp = Utc::now();
        index_note_row(&database, "note-1");
        index_note_row(&database, "note-2");
        let base_vector = unit_vector(descriptor.dimension(), 1.0);
        let alt_vector = unit_vector(descriptor.dimension(), -1.0);

        let record = EmbeddingRecord {
            note_id: "note-1".to_string(),
            vector: base_vector.clone(),
            indexed_at: timestamp,
        };
        let other = EmbeddingRecord {
            note_id: "note-2".to_string(),
            vector: alt_vector.clone(),
            indexed_at: timestamp,
        };

        store.upsert_embeddings(&[record, other]).await?;

        let matches = store.search(&base_vector, 5, 0.3).await?;
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].note_id, "note-1");
        assert!(matches[0].distance <= 1e-4);

        store
            .delete_embeddings(&["note-1".to_string(), "note-2".to_string()])
            .await?;
        let after = store.search(&base_vector, 5, 0.3).await?;
        assert!(after.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn vector_for_note_returns_stored_vector() -> Result<()> {
        let dir = tempdir().expect("temp dir");
        let db_path = dir.path().join("index.db");
        let database = Arc::new(IndexDatabase::open(&db_path)?);
        let descriptor = EmbeddingDescriptor::resolve("fast")?;
        let (store, _) = EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor).await?;

        let timestamp = Utc::now();
        let vector = unit_vector(descriptor.dimension(), 0.75);
        let record = EmbeddingRecord {
            note_id: "note-1".to_string(),
            vector: vector.clone(),
            indexed_at: timestamp,
        };
        store.upsert_embeddings(&[record]).await?;

        let loaded = store
            .vector_for_note("note-1")
            .await?
            .expect("vector present");
        assert_eq!(loaded.len(), vector.len());
        for (expected, actual) in vector.iter().zip(loaded.iter()) {
            assert!((expected - actual).abs() <= f32::EPSILON);
        }

        let missing = store.vector_for_note("missing").await?;
        assert!(missing.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn database_note_removal_cleans_up_orphaned_embedding() -> Result<()> {
        let dir = tempdir().expect("temp dir");
        let database = Arc::new(IndexDatabase::open(dir.path().join("index.db"))?);
        let descriptor = EmbeddingDescriptor::resolve("fast")?;
        let (store, _) = EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor).await?;
        let note_id = "orphaned-note";

        store
            .upsert_embeddings(&[EmbeddingRecord {
                note_id: note_id.to_string(),
                vector: unit_vector(descriptor.dimension(), 0.5),
                indexed_at: Utc::now(),
            }])
            .await?;
        assert!(store.vector_for_note(note_id).await?.is_some());

        assert!(
            !database.remove_note(note_id)?,
            "the regression fixture intentionally has no text-index row"
        );
        assert!(store.vector_for_note(note_id).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn stale_note_ids_reports_missing_and_outdated_vectors_after_reopen() -> Result<()> {
        let dir = tempdir().expect("temp dir");
        let db_path = dir.path().join("index.db");
        let descriptor = EmbeddingDescriptor::resolve("fast")?;
        let dimension = descriptor.dimension();
        {
            let database = Arc::new(IndexDatabase::open(&db_path)?);
            let (store, _) = EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor).await?;
            let current = index_note_row(&database, "current");
            let first_outdated = index_note_row(&database, "outdated");
            index_note_row(&database, "missing");
            store
                .upsert_embeddings(&[
                    vector_record("current", axis_vector(dimension, 0), current),
                    vector_record("outdated", axis_vector(dimension, 1), first_outdated),
                ])
                .await?;
            assert!(
                store
                    .stale_note_ids(&database.note_states()?)
                    .await?
                    .contains("missing")
            );

            index_note_row(&database, "outdated");
        }

        let database = Arc::new(IndexDatabase::open(&db_path)?);
        let (store, changed) =
            EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor).await?;
        assert!(!changed);
        let stale = store.stale_note_ids(&database.note_states()?).await?;
        assert_eq!(
            stale,
            HashSet::from(["missing".to_string(), "outdated".to_string()]),
            "a vector is current only for the generation of its note row"
        );
        Ok(())
    }

    #[tokio::test]
    async fn search_skips_vectors_whose_note_row_is_gone() -> Result<()> {
        let dir = tempdir().expect("temp dir");
        let database = Arc::new(IndexDatabase::open(dir.path().join("index.db"))?);
        let descriptor = EmbeddingDescriptor::resolve("fast")?;
        let dimension = descriptor.dimension();
        let (store, _) = EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor).await?;
        let live = index_note_row(&database, "live");
        let mut near_query = axis_vector(dimension, 0);
        near_query[1] = 0.2;
        store
            .upsert_embeddings(&[
                vector_record("live", normalize_vector(near_query)?, live),
                vector_record("ghost", axis_vector(dimension, 0), live),
            ])
            .await?;

        let matches = store.search(&axis_vector(dimension, 0), 10, 0.3).await?;
        let ids: Vec<&str> = matches.iter().map(|item| item.note_id.as_str()).collect();
        assert_eq!(ids, vec!["live"]);
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_removes_vectors_whose_note_row_is_gone() -> Result<()> {
        let dir = tempdir().expect("temp dir");
        let db_path = dir.path().join("index.db");
        let descriptor = EmbeddingDescriptor::resolve("fast")?;
        let dimension = descriptor.dimension();
        {
            let database = Arc::new(IndexDatabase::open(&db_path)?);
            let (store, _) = EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor).await?;
            let live = index_note_row(&database, "live");
            store
                .upsert_embeddings(&[
                    vector_record("live", axis_vector(dimension, 0), live),
                    vector_record("ghost", axis_vector(dimension, 1), live),
                ])
                .await?;
        }

        let database = Arc::new(IndexDatabase::open(&db_path)?);
        let (store, changed) =
            EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor).await?;
        assert!(!changed, "removing dead vectors is not a model change");
        assert!(store.vector_for_note("ghost").await?.is_none());
        assert!(store.vector_for_note("live").await?.is_some());
        Ok(())
    }

    struct RecordingSubscriber(Arc<Mutex<Vec<String>>>);

    impl tracing::Subscriber for RecordingSubscriber {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            struct Message<'a>(&'a mut Vec<String>);
            impl tracing::field::Visit for Message<'_> {
                fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
                    if field.name() == "message" {
                        self.0.push(format!("{value:?}"));
                    }
                }
            }
            event.record(&mut Message(&mut self.0.lock().expect("log buffer")));
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[tokio::test]
    async fn bootstrap_cleanup_logs_to_the_callers_subscriber() -> Result<()> {
        let dir = tempdir().expect("temp dir");
        let database = Arc::new(IndexDatabase::open(dir.path().join("index.db"))?);
        let descriptor = EmbeddingDescriptor::resolve("fast")?;
        let (store, _) = EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor).await?;
        store
            .upsert_embeddings(&[vector_record(
                "ghost",
                axis_vector(descriptor.dimension(), 0),
                Utc::now(),
            )])
            .await?;

        let messages = Arc::new(Mutex::new(Vec::new()));
        let _guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(
            RecordingSubscriber(Arc::clone(&messages)),
        ));
        EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor).await?;

        assert!(
            messages
                .lock()
                .expect("log buffer")
                .iter()
                .any(|message| message.contains("removed embeddings for notes missing")),
            "cleanup ran on a blocking thread and its log must reach the caller's subscriber"
        );
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_resets_vectors_when_the_model_repository_changes() -> Result<()> {
        let dir = tempdir().expect("temp dir");
        let database = Arc::new(IndexDatabase::open(dir.path().join("index.db"))?);
        let descriptor = EmbeddingDescriptor::resolve("fast")?;
        let (store, _) = EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor).await?;
        let generation = index_note_row(&database, "note");
        store
            .upsert_embeddings(&[vector_record(
                "note",
                axis_vector(descriptor.dimension(), 0),
                generation,
            )])
            .await?;
        database.connection()?.execute(
            "UPDATE embedding_metadata SET repository = 'previous-org/previous-model'",
            [],
        )?;

        let (store, changed) =
            EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor).await?;
        assert!(
            changed,
            "vectors from another model repository must be rebuilt"
        );
        assert!(store.vector_for_note("note").await?.is_none());
        assert!(
            store
                .stale_note_ids(&database.note_states()?)
                .await?
                .contains("note")
        );

        let (_, changed_again) =
            EmbeddingStore::bootstrap(Arc::clone(&database), &descriptor).await?;
        assert!(!changed_again);
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_detects_model_change() -> Result<()> {
        let dir = tempdir().expect("temp dir");
        let db_path = dir.path().join("index.db");
        let database = Arc::new(IndexDatabase::open(&db_path)?);

        let fast = EmbeddingDescriptor::resolve("fast")?;
        let (_, changed_fast) = EmbeddingStore::bootstrap(Arc::clone(&database), &fast).await?;
        assert!(!changed_fast);

        let better = EmbeddingDescriptor::resolve("better")?;
        let (_, changed_better) = EmbeddingStore::bootstrap(Arc::clone(&database), &better).await?;
        assert!(changed_better);

        let (_, changed_again) = EmbeddingStore::bootstrap(Arc::clone(&database), &better).await?;
        assert!(!changed_again);
        Ok(())
    }
}
