//! WikiLink graph navigation primitives.

use anyhow::{Result, bail};

use crate::NoteId;

/// Relationship between two notes in the vault graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkEdge {
    /// The note that owns the outgoing link.
    pub source: NoteId,
    /// The note being referenced, if resolved.
    pub target: Option<NoteId>,
    /// Original link text as written in the markdown file.
    pub display_text: Option<String>,
}

/// High-level operations over the note graph.
#[derive(Debug, Default, Clone)]
pub struct GraphService;

impl GraphService {
    /// Create a new instance.
    pub fn new() -> Self {
        Self
    }

    /// Fetch backlinks pointing to the supplied note ID.
    pub async fn backlinks(&self, _note_id: &str) -> Result<Vec<LinkEdge>> {
        bail!("backlink queries not implemented yet")
    }

    /// Fetch forward links originating from the supplied note ID.
    pub async fn forward_links(&self, _note_id: &str) -> Result<Vec<LinkEdge>> {
        bail!("forward link queries not implemented yet")
    }

    /// Identify orphan notes that have neither incoming nor outgoing links.
    pub async fn orphans(&self) -> Result<Vec<NoteId>> {
        bail!("orphan detection not implemented yet")
    }

    /// List unresolved WikiLinks that need manual attention.
    pub async fn unresolved_links(&self) -> Result<Vec<LinkEdge>> {
        bail!("unresolved link listing not implemented yet")
    }
}
