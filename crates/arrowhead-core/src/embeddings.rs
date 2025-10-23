//! Embedding generation, configuration, and persistence.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, anyhow, bail};
#[cfg(feature = "vector-lancedb")]
use chrono::{DateTime, Utc};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
#[cfg(feature = "vector-lancedb")]
use serde::{Deserialize, Serialize};

use crate::Vault;

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
    model: Arc<Mutex<TextEmbedding>>,
}

impl EmbeddingGenerator {
    /// Load the configured embedding model, downloading weights as needed.
    pub fn initialise(config: EmbeddingConfig) -> Result<Self> {
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

        let mut options = TextInitOptions::new(config.descriptor.model().clone())
            .with_cache_dir(config.cache_dir.clone())
            .with_show_download_progress(config.show_download_progress);
        if let Some(max_length) = config.max_length {
            options = options.with_max_length(max_length);
        }

        let model = TextEmbedding::try_new(options)
            .context("failed to initialise embedding model via fastembed")?;

        Ok(Self {
            config,
            model: Arc::new(Mutex::new(model)),
        })
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

        let mut guard = self.model.lock().expect("embedding model poisoned");
        let embeddings = guard
            .embed(documents.to_vec(), None)
            .context("failed to embed documents")?;

        embeddings.into_iter().map(normalize_vector).collect()
    }

    /// Generate an embedding for a single document.
    pub fn embed_document(&self, document: &str) -> Result<Vec<f32>> {
        let inputs = vec![document.to_string()];
        let mut all = self.embed_documents(&inputs)?;
        all.pop()
            .ok_or_else(|| anyhow!("document embedding missing"))
    }

    /// Generate an embedding suitable for semantic query comparisons.
    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let inputs = vec![query.to_string()];
        let mut guard = self.model.lock().expect("embedding model poisoned");
        let mut embeddings = guard.embed(inputs, None).context("failed to embed query")?;
        let vector = embeddings
            .pop()
            .ok_or_else(|| anyhow!("query embedding missing"))?;
        normalize_vector(vector)
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

#[cfg(feature = "vector-lancedb")]
use {
    arrow_array::builder::{FixedSizeListBuilder, Float32Builder, Int64Builder, StringBuilder},
    arrow_array::{ArrayRef, RecordBatch, RecordBatchIterator},
    arrow_schema::{ArrowError, DataType, Field, Schema, SchemaRef},
    futures::StreamExt,
    lancedb::{
        DistanceType, connect,
        connection::Connection,
        query::{ExecutableQuery, QueryBase},
    },
    std::collections::BTreeSet,
    tokio::sync::Mutex as AsyncMutex,
};

#[cfg(feature = "vector-lancedb")]
const EMBEDDING_TABLE_NAME: &str = "note_embeddings";

#[cfg(feature = "vector-lancedb")]
#[derive(Debug, Clone)]
/// Embedding payload captured during indexing for a single note.
pub struct EmbeddingRecord {
    /// Identifier of the note the vector belongs to.
    pub note_id: String,
    /// Normalised embedding vector produced by fastembed.
    pub vector: Vec<f32>,
    /// Timestamp when the embedding was generated.
    pub indexed_at: DateTime<Utc>,
}

#[cfg(not(feature = "vector-lancedb"))]
#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub struct EmbeddingRecord;

#[cfg(feature = "vector-lancedb")]
#[derive(Debug, Clone)]
/// Result row returned from LanceDB vector search.
pub struct EmbeddingMatch {
    /// Note identifier matched by LanceDB.
    pub note_id: String,
    /// Raw cosine distance reported by LanceDB.
    pub distance: f32,
}

#[cfg(not(feature = "vector-lancedb"))]
#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub struct EmbeddingMatch;

#[cfg(feature = "vector-lancedb")]
struct EmbeddingPipelineInner {
    descriptor: EmbeddingDescriptor,
    generator: Arc<EmbeddingGenerator>,
    store: Arc<EmbeddingStore>,
    model_changed: bool,
}

/// Combined embedding generator + persistence pipeline.
#[derive(Clone)]
pub struct EmbeddingPipeline {
    #[cfg(feature = "vector-lancedb")]
    inner: Arc<EmbeddingPipelineInner>,
    #[cfg(not(feature = "vector-lancedb"))]
    _marker: std::marker::PhantomData<()>,
}

impl EmbeddingPipeline {
    /// Whether LanceDB-backed embeddings are compiled into this build.
    pub fn is_supported() -> bool {
        cfg!(feature = "vector-lancedb")
    }
}

impl fmt::Debug for EmbeddingPipeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[cfg(feature = "vector-lancedb")]
        {
            return f
                .debug_struct("EmbeddingPipeline")
                .field("model", &self.descriptor().identifier())
                .field("model_changed", &self.model_changed())
                .finish();
        }

        #[cfg(not(feature = "vector-lancedb"))]
        {
            f.debug_struct("EmbeddingPipeline")
                .field("supported", &false)
                .finish()
        }
    }
}

#[cfg(feature = "vector-lancedb")]
impl EmbeddingPipeline {
    /// Prepare the embedding pipeline for the supplied vault and model identifier.
    pub async fn initialise(vault: &Vault, model_id: &str) -> Result<Self> {
        let descriptor = EmbeddingDescriptor::resolve(model_id)?;
        let vault_paths = vault.paths();
        let models_dir = vault_paths
            .arrowhead_dir
            .join("models")
            .join(descriptor.identifier());
        let vectors_dir = vault_paths.arrowhead_dir.join("vectors");

        fs::create_dir_all(&models_dir).with_context(|| {
            format!(
                "failed to create embedding model directory {}",
                models_dir.display()
            )
        })?;

        let model_changed = prepare_vector_directory(&vectors_dir, &descriptor)?;

        let generator = EmbeddingGenerator::initialise(EmbeddingConfig::new(
            descriptor.clone(),
            models_dir.clone(),
        ))?;
        let store = EmbeddingStore::connect(&vectors_dir, &descriptor).await?;

        write_metadata(&vectors_dir, &descriptor).context("failed to write embedding metadata")?;

        Ok(Self {
            inner: Arc::new(EmbeddingPipelineInner {
                descriptor,
                generator: Arc::new(generator),
                store: Arc::new(store),
                model_changed,
            }),
        })
    }

    /// Access the generator component.
    pub fn generator(&self) -> &EmbeddingGenerator {
        self.inner.generator.as_ref()
    }

    /// Access the LanceDB store.
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

#[cfg(not(feature = "vector-lancedb"))]
#[allow(missing_docs)]
impl EmbeddingPipeline {
    /// Stub initialiser when LanceDB support is not compiled.
    pub async fn initialise(_vault: &Vault, _model_id: &str) -> Result<Self> {
        bail!(
            "semantic embeddings require the `vector-lancedb` Cargo feature. Rebuild Arrowhead with --features vector-lancedb."
        );
    }

    pub fn generator(&self) -> &EmbeddingGenerator {
        panic!("semantic embeddings are unavailable in this build")
    }

    pub fn store(&self) -> &EmbeddingStore {
        panic!("semantic embeddings are unavailable in this build")
    }

    pub fn descriptor(&self) -> &EmbeddingDescriptor {
        panic!("semantic embeddings are unavailable in this build")
    }

    pub fn model_changed(&self) -> bool {
        false
    }
}

#[cfg(feature = "vector-lancedb")]
#[derive(Clone)]
/// Wrapper over the LanceDB table used to persist note embeddings.
pub struct EmbeddingStore {
    connection: Arc<Connection>,
    table_name: String,
    dimension: usize,
    model_id: String,
    write_lock: Arc<AsyncMutex<()>>,
}

#[cfg(feature = "vector-lancedb")]
impl EmbeddingStore {
    /// Open (or create) the embeddings table for the supplied vault/model pair.
    pub async fn connect(path: &Path, descriptor: &EmbeddingDescriptor) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create vectors directory {}", parent.display())
            })?;
        }
        fs::create_dir_all(path)
            .with_context(|| format!("failed to create vectors directory {}", path.display()))?;

        let uri = path
            .to_str()
            .ok_or_else(|| anyhow!("invalid vectors path: {}", path.display()))?;
        let connection = connect(uri)
            .execute()
            .await
            .context("failed to open LanceDB connection")?;

        ensure_table_schema(&connection, EMBEDDING_TABLE_NAME, descriptor)
            .await
            .context("failed to prepare LanceDB table for embeddings")?;

        Ok(Self {
            connection: Arc::new(connection),
            table_name: EMBEDDING_TABLE_NAME.to_string(),
            dimension: descriptor.dimension(),
            model_id: descriptor.identifier().to_string(),
            write_lock: Arc::new(AsyncMutex::new(())),
        })
    }

    /// Upsert the supplied embeddings into the LanceDB table.
    pub async fn upsert_embeddings(&self, records: &[EmbeddingRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        let _guard = self.write_lock.lock().await;
        let table = self
            .connection
            .open_table(&self.table_name)
            .execute()
            .await
            .context("failed to open embedding table")?;

        let mut unique_ids = BTreeSet::new();
        for record in records {
            unique_ids.insert(record.note_id.clone());
        }

        for chunk in unique_ids.iter().collect::<Vec<_>>().chunks(256) {
            let predicate = chunk
                .iter()
                .map(|id| format!("note_id = '{}'", id.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(" OR ");
            table.delete(&predicate).await.with_context(|| {
                format!("failed to delete embeddings for predicate {predicate}")
            })?;
        }

        let batch = build_embedding_batch(records, self.dimension, &self.model_id)?;
        table
            .add(batch)
            .execute()
            .await
            .context("failed to append embeddings into LanceDB")?;

        Ok(())
    }

    /// Delete embeddings for the supplied note identifiers.
    pub async fn delete_embeddings(&self, note_ids: &[String]) -> Result<()> {
        if note_ids.is_empty() {
            return Ok(());
        }

        let _guard = self.write_lock.lock().await;
        let table = self
            .connection
            .open_table(&self.table_name)
            .execute()
            .await
            .context("failed to open embedding table for deletion")?;

        for chunk in note_ids.chunks(256) {
            let predicate = chunk
                .iter()
                .map(|id| format!("note_id = '{}'", id.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(" OR ");
            table.delete(&predicate).await.with_context(|| {
                format!("failed to delete embeddings for predicate {predicate}")
            })?;
        }

        Ok(())
    }

    /// Execute a vector search using cosine distance.
    pub async fn search(
        &self,
        query: &[f32],
        limit: usize,
        threshold: f32,
    ) -> Result<Vec<EmbeddingMatch>> {
        if query.len() != self.dimension {
            bail!(
                "query vector dimension mismatch: expected {}, got {}",
                self.dimension,
                query.len()
            );
        }

        let table = self
            .connection
            .open_table(&self.table_name)
            .execute()
            .await
            .context("failed to open embedding table for search")?;

        let mut stream = table
            .vector_search(query.to_vec())
            .context("failed to prepare vector search")?
            .distance_type(DistanceType::Cosine)
            .limit(limit.max(1))
            .execute()
            .await
            .context("semantic search execution failed")?;

        let mut matches = Vec::new();
        while let Some(batch) = stream.next().await {
            let batch = batch.context("failed to read LanceDB result batch")?;
            let note_ids = batch
                .column_by_name("note_id")
                .context("LanceDB result missing note_id column")?
                .as_any()
                .downcast_ref::<arrow_array::StringArray>()
                .context("invalid arrow type for note_id column")?;
            let distances = batch
                .column_by_name("_distance")
                .context("LanceDB result missing _distance column")?
                .as_any()
                .downcast_ref::<arrow_array::Float32Array>()
                .context("invalid arrow type for _distance column")?;
            for index in 0..batch.num_rows() {
                let note_id = note_ids.value(index).to_string();
                let distance = distances.value(index);
                let similarity = (1.0_f32 - distance).max(0.0_f32);
                if similarity >= threshold {
                    matches.push(EmbeddingMatch { note_id, distance });
                }
            }
        }

        // Results are already distance-sorted; retain ordering while enforcing limit.
        if matches.len() > limit {
            matches.truncate(limit);
        }

        Ok(matches)
    }
}

#[cfg(not(feature = "vector-lancedb"))]
#[allow(missing_docs)]
#[derive(Clone)]
pub struct EmbeddingStore;

#[cfg(not(feature = "vector-lancedb"))]
#[allow(missing_docs)]
impl EmbeddingStore {
    pub async fn upsert_embeddings(&self, _records: &[EmbeddingRecord]) -> Result<()> {
        bail!("semantic embeddings are unavailable in this build")
    }

    pub async fn delete_embeddings(&self, _note_ids: &[String]) -> Result<()> {
        bail!("semantic embeddings are unavailable in this build")
    }

    pub async fn search(
        &self,
        _query: &[f32],
        _limit: usize,
        _threshold: f32,
    ) -> Result<Vec<EmbeddingMatch>> {
        bail!("semantic embeddings are unavailable in this build")
    }
}

#[cfg(feature = "vector-lancedb")]
fn build_embedding_batch(
    records: &[EmbeddingRecord],
    dimension: usize,
    model_id: &str,
) -> Result<RecordBatchIterator<std::vec::IntoIter<Result<RecordBatch, ArrowError>>>> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("note_id", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dimension as i32,
            ),
            false,
        ),
        Field::new("model", DataType::Utf8, false),
        Field::new("indexed_at", DataType::Int64, false),
    ]));

    let mut id_builder = StringBuilder::new();
    let mut model_builder = StringBuilder::new();
    let mut timestamp_builder = Int64Builder::with_capacity(records.len());
    let value_builder = Float32Builder::with_capacity(records.len() * dimension);
    let mut vector_builder = FixedSizeListBuilder::new(value_builder, dimension as i32);

    for record in records {
        if record.vector.len() != dimension {
            bail!(
                "embedding dimension mismatch: expected {}, got {}",
                dimension,
                record.vector.len()
            );
        }
        id_builder.append_value(&record.note_id);
        model_builder.append_value(model_id);
        timestamp_builder.append_value(record.indexed_at.timestamp_micros());
        {
            let values = vector_builder.values();
            for value in &record.vector {
                values.append_value(*value);
            }
        }
        vector_builder.append(true);
    }

    let arrays: Vec<ArrayRef> = vec![
        Arc::new(id_builder.finish()),
        Arc::new(vector_builder.finish()),
        Arc::new(model_builder.finish()),
        Arc::new(timestamp_builder.finish()),
    ];

    let batch = RecordBatch::try_new(schema.clone(), arrays)
        .context("failed to assemble embedding record batch")?;

    Ok(RecordBatchIterator::new(
        vec![Ok::<_, ArrowError>(batch)].into_iter(),
        schema,
    ))
}

#[cfg(feature = "vector-lancedb")]
async fn ensure_table_schema(
    connection: &Connection,
    table_name: &str,
    descriptor: &EmbeddingDescriptor,
) -> Result<()> {
    let names = connection
        .table_names()
        .execute()
        .await
        .context("failed to list LanceDB tables")?;

    if !names.iter().any(|name| name == table_name) {
        create_table(connection, table_name, descriptor)
            .await
            .context("failed to create embedding table")?;
        return Ok(());
    }

    let table = connection
        .open_table(table_name)
        .execute()
        .await
        .context("failed to open embedding table for validation")?;

    let schema = table
        .schema()
        .await
        .context("failed to fetch LanceDB table schema")?;

    if schema_compatible(&schema, descriptor.dimension()) {
        return Ok(());
    }

    connection
        .drop_table(table_name)
        .await
        .context("failed to drop incompatible embedding table")?;
    create_table(connection, table_name, descriptor)
        .await
        .context("failed to recreate embedding table")
}

#[cfg(feature = "vector-lancedb")]
fn schema_compatible(schema: &Schema, dimension: usize) -> bool {
    fn field_dim(field: &Field) -> Option<usize> {
        match field.data_type() {
            DataType::FixedSizeList(inner, len) => {
                if !matches!(inner.data_type(), DataType::Float32) {
                    return None;
                }
                Some(*len as usize)
            }
            _ => None,
        }
    }

    let vector_field = schema.field_with_name("vector").ok();
    let note_id_field = schema.field_with_name("note_id").ok();
    let model_field = schema.field_with_name("model").ok();
    let indexed_field = schema.field_with_name("indexed_at").ok();

    match (vector_field, note_id_field, model_field, indexed_field) {
        (Some(vector), Some(note_id), Some(model), Some(indexed)) => {
            vector.is_nullable() == false
                && note_id.data_type() == &DataType::Utf8
                && !note_id.is_nullable()
                && model.data_type() == &DataType::Utf8
                && !model.is_nullable()
                && indexed.data_type() == &DataType::Int64
                && !indexed.is_nullable()
                && field_dim(vector) == Some(dimension)
        }
        _ => false,
    }
}

#[cfg(feature = "vector-lancedb")]
async fn create_table(
    connection: &Connection,
    table_name: &str,
    descriptor: &EmbeddingDescriptor,
) -> Result<()> {
    let schema: SchemaRef = Arc::new(Schema::new(vec![
        Field::new("note_id", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                descriptor.dimension() as i32,
            ),
            false,
        ),
        Field::new("model", DataType::Utf8, false),
        Field::new("indexed_at", DataType::Int64, false),
    ]));

    connection
        .create_empty_table(table_name, schema)
        .execute()
        .await
        .context("failed to create LanceDB embeddings table")?;
    Ok(())
}

#[cfg(feature = "vector-lancedb")]
fn prepare_vector_directory(path: &Path, descriptor: &EmbeddingDescriptor) -> Result<bool> {
    let metadata_path = path.join("metadata.json");
    if !path.exists() {
        return Ok(false);
    }

    let existing = fs::read_to_string(&metadata_path).ok();
    if let Some(raw) = existing {
        if let Ok(metadata) = serde_json::from_str::<EmbeddingMetadata>(&raw) {
            if metadata.model_id == descriptor.identifier()
                && metadata.dimension == descriptor.dimension()
            {
                return Ok(false);
            }
        }
    }

    fs::remove_dir_all(path).with_context(|| {
        format!(
            "failed to clear obsolete embedding directory {}",
            path.display()
        )
    })?;
    fs::create_dir_all(path)
        .with_context(|| format!("failed to recreate embedding directory {}", path.display()))?;
    Ok(true)
}

#[cfg(feature = "vector-lancedb")]
fn write_metadata(path: &Path, descriptor: &EmbeddingDescriptor) -> Result<()> {
    let metadata_path = path.join("metadata.json");
    let metadata = EmbeddingMetadata {
        model_id: descriptor.identifier().to_string(),
        repository: descriptor.repository().to_string(),
        dimension: descriptor.dimension(),
    };
    let json = serde_json::to_string_pretty(&metadata)
        .context("failed to serialise embedding metadata")?;
    fs::write(&metadata_path, json).with_context(|| {
        format!(
            "failed to write embedding metadata to {}",
            metadata_path.display()
        )
    })
}

#[cfg(feature = "vector-lancedb")]
#[derive(Debug, Serialize, Deserialize)]
struct EmbeddingMetadata {
    model_id: String,
    repository: String,
    dimension: usize,
}
