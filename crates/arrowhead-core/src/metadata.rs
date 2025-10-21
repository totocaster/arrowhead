//! Metadata extraction utilities.

use anyhow::{Result, bail};

use crate::{MetadataMap, NoteRecord};

/// Output of metadata extraction combining frontmatter and inline discoveries.
#[derive(Debug, Clone, Default)]
pub struct MetadataExtraction {
    /// Metadata fields as key/value pairs ready for persistence.
    pub metadata: MetadataMap,
    /// WikiLink targets discovered in the note body.
    pub wikilinks: Vec<String>,
    /// Inline tags extracted from content.
    pub tags: Vec<String>,
}

/// Parses notes and produces structured metadata for indexing.
#[derive(Debug, Default, Clone)]
pub struct MetadataExtractor;

impl MetadataExtractor {
    /// Create a new metadata extractor instance.
    pub fn new() -> Self {
        Self
    }

    /// Extract metadata from the supplied note.
    pub fn extract(&self, _note: &NoteRecord) -> Result<MetadataExtraction> {
        bail!("metadata extraction not implemented yet")
    }
}
