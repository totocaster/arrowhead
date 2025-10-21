//! Embedding generation and storage primitives.

use std::{path::Path, sync::Arc};

use anyhow::{Context, Result, bail};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

#[cfg(feature = "vector-lancedb")]
use lancedb::database::Database;

/// Configuration for setting up the embedding generator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingConfig {
    /// Which embedding model to load.
    pub model: EmbeddingModel,
    /// Optional override for the maximum input token length.
    pub max_length: Option<usize>,
    /// Whether to display download progress when fetching model artifacts.
    pub show_download_progress: bool,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            model: EmbeddingModel::AllMiniLML6V2,
            max_length: None,
            show_download_progress: true,
        }
    }
}

/// Lightweight wrapper around `fastembed` for generating embeddings.
#[derive(Clone)]
pub struct EmbeddingGenerator {
    config: EmbeddingConfig,
    #[allow(dead_code)]
    model: Arc<TextEmbedding>,
}

impl EmbeddingGenerator {
    /// Load the configured embedding model, downloading weights as necessary.
    pub fn initialise(config: EmbeddingConfig) -> Result<Self> {
        let mut options = TextInitOptions::new(config.model.clone());
        options.show_download_progress = config.show_download_progress;
        if let Some(max_length) = config.max_length {
            options.max_length = max_length;
        }

        let model =
            TextEmbedding::try_new(options).context("failed to initialise text embedding model")?;

        Ok(Self {
            config,
            model: Arc::new(model),
        })
    }

    /// Return the underlying configuration.
    pub fn config(&self) -> &EmbeddingConfig {
        &self.config
    }

    /// Generate embeddings for the supplied documents.
    pub fn embed_documents(&self, _documents: &[String]) -> Result<Vec<Vec<f32>>> {
        bail!("document embedding not implemented yet")
    }

    /// Generate a single embedding for a query string.
    pub fn embed_query(&self, _query: &str) -> Result<Vec<f32>> {
        bail!("query embedding not implemented yet")
    }
}

/// LanceDB-backed storage for embeddings.
#[derive(Debug, Clone)]
pub struct EmbeddingStore {
    #[cfg(feature = "vector-lancedb")]
    database: Arc<Database>,
    table_name: String,
}

impl EmbeddingStore {
    /// Connect (or create) a LanceDB database at the given path.
    #[cfg(feature = "vector-lancedb")]
    pub async fn connect<P: AsRef<Path>, S: Into<String>>(path: P, table: S) -> Result<Self> {
        let path_str = path
            .as_ref()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("invalid database path"))?
            .to_string();

        let database = lancedb::connect(path_str)
            .execute()
            .await
            .context("unable to open LanceDB database")?;

        Ok(Self {
            database: Arc::new(database),
            table_name: table.into(),
        })
    }

    /// Connect placeholder when the LanceDB feature is disabled.
    #[cfg(not(feature = "vector-lancedb"))]
    pub async fn connect<P: AsRef<Path>, S: Into<String>>(path: P, table: S) -> Result<Self> {
        let _ = path;
        let _ = table.into();
        bail!("LanceDB support is not compiled in. Enable the `vector-lancedb` feature to use it.")
    }

    /// Return the logical table name storing note vectors.
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// Placeholder for batch upsert operations.
    pub async fn upsert_embeddings(&self) -> Result<()> {
        #[cfg(feature = "vector-lancedb")]
        {
            bail!("embedding persistence not implemented yet")
        }

        #[cfg(not(feature = "vector-lancedb"))]
        {
            bail!(
                "LanceDB support is not compiled in. Enable the `vector-lancedb` feature to use it."
            )
        }
    }
}
