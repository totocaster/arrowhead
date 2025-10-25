//! WikiLink graph persistence and query services.

use std::{str::FromStr, sync::Arc};

use anyhow::{Context, Result, anyhow};
use tokio::{task, try_join};

use crate::{
    NoteId,
    sqlite::{IndexDatabase, LinkResolutionMaps},
};

/// Canonicalise link keys for case-insensitive lookups.
pub fn normalise_link_lookup(value: &str) -> String {
    value.trim().replace('\\', "/").to_lowercase()
}

/// Classification describing how a link target was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkReason {
    /// Direct filename/identifier match.
    Direct,
    /// Match resolved via another note's title.
    Title,
    /// Match resolved via an alias defined in the target note.
    Alias,
    /// Link could not be resolved to a note.
    Unresolved,
}

impl LinkReason {
    /// Render the reason as a stable string for persistence.
    pub fn as_str(&self) -> &'static str {
        match self {
            LinkReason::Direct => "direct",
            LinkReason::Title => "title",
            LinkReason::Alias => "alias",
            LinkReason::Unresolved => "unresolved",
        }
    }
}

impl FromStr for LinkReason {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "direct" => Ok(LinkReason::Direct),
            "title" => Ok(LinkReason::Title),
            "alias" => Ok(LinkReason::Alias),
            "unresolved" => Ok(LinkReason::Unresolved),
            other => Err(anyhow!("unknown link reason '{other}'")),
        }
    }
}

/// Intermediate record describing a resolved WikiLink edge prior to persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkResolutionRecord {
    /// Raw text between the WikiLink delimiters.
    pub raw: String,
    /// Resolved note identifier, if any.
    pub target: Option<NoteId>,
    /// Optional display text (`[[target|display]]`).
    pub display: Option<String>,
    /// Optional heading anchor (`[[target#Heading]]`).
    pub heading: Option<String>,
    /// Resolution strategy applied for this link.
    pub reason: LinkReason,
}

/// Persisted relationship between two notes within the vault graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkEdge {
    /// The note that owns the outgoing link.
    pub source: NoteId,
    /// The note being referenced, if the link resolved.
    pub target: Option<NoteId>,
    /// Original link text without delimiters.
    pub raw: String,
    /// Optional alias/display override supplied in the link.
    pub display_text: Option<String>,
    /// Optional heading component supplied in the link.
    pub heading: Option<String>,
    /// Explanation for how the relationship was determined.
    pub reason: LinkReason,
}

/// Combined view of inbound and outbound edges for a single note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphContext {
    /// Notes that reference the target note.
    pub backlinks: Vec<LinkEdge>,
    /// Notes referenced by the target note.
    pub forward_links: Vec<LinkEdge>,
}

/// High-level operations over the note graph.
#[derive(Debug, Clone)]
pub struct GraphService {
    database: Arc<IndexDatabase>,
}

impl GraphService {
    /// Create a new graph service backed by the supplied index database.
    pub fn new(database: Arc<IndexDatabase>) -> Self {
        Self { database }
    }

    /// Fetch backlinks pointing to the supplied note identifier.
    pub async fn backlinks(&self, note_id: &str) -> Result<Vec<LinkEdge>> {
        let db = Arc::clone(&self.database);
        let note_id = note_id.to_string();
        task::spawn_blocking(move || {
            let conn = db.connection()?;
            let mut stmt = conn.prepare(
                "SELECT source_id, target_id, raw_text, display_text, heading, reason
                 FROM note_links
                 WHERE target_id = ?1
                 ORDER BY source_id, raw_text",
            )?;
            let mut rows = stmt.query([&note_id])?;
            let mut edges = Vec::new();
            while let Some(row) = rows.next()? {
                let reason_raw: String = row.get(5)?;
                let reason = LinkReason::from_str(&reason_raw).with_context(|| {
                    format!("invalid link reason '{reason_raw}' for backlink to {note_id}")
                })?;
                edges.push(LinkEdge {
                    source: row.get(0)?,
                    target: row.get(1)?,
                    raw: row.get(2)?,
                    display_text: row.get(3)?,
                    heading: row.get(4)?,
                    reason,
                });
            }
            Ok::<_, anyhow::Error>(edges)
        })
        .await
        .context("backlink task aborted")?
    }

    /// Fetch forward links originating from the supplied note identifier.
    pub async fn forward_links(&self, note_id: &str) -> Result<Vec<LinkEdge>> {
        let db = Arc::clone(&self.database);
        let note_id = note_id.to_string();
        task::spawn_blocking(move || {
            let conn = db.connection()?;
            let mut stmt = conn.prepare(
                "SELECT source_id, target_id, raw_text, display_text, heading, reason
                 FROM note_links
                 WHERE source_id = ?1
                 ORDER BY target_id, raw_text",
            )?;
            let mut rows = stmt.query([&note_id])?;
            let mut edges = Vec::new();
            while let Some(row) = rows.next()? {
                let reason_raw: String = row.get(5)?;
                let reason = LinkReason::from_str(&reason_raw).with_context(|| {
                    format!("invalid link reason '{reason_raw}' for forward link from {note_id}")
                })?;
                edges.push(LinkEdge {
                    source: row.get(0)?,
                    target: row.get(1)?,
                    raw: row.get(2)?,
                    display_text: row.get(3)?,
                    heading: row.get(4)?,
                    reason,
                });
            }
            Ok::<_, anyhow::Error>(edges)
        })
        .await
        .context("forward link task aborted")?
    }

    /// Identify orphan notes that have neither inbound nor outbound resolved links.
    pub async fn orphans(&self) -> Result<Vec<NoteId>> {
        let db = Arc::clone(&self.database);
        task::spawn_blocking(move || {
            let conn = db.connection()?;
            let mut stmt = conn.prepare(
                "SELECT n.id
                 FROM notes n
                 LEFT JOIN note_links outgoing
                   ON outgoing.source_id = n.id AND outgoing.target_id IS NOT NULL
                 LEFT JOIN note_links incoming
                   ON incoming.target_id = n.id
                 WHERE outgoing.rowid IS NULL AND incoming.rowid IS NULL
                 ORDER BY n.id",
            )?;
            let ids = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, anyhow::Error>(ids)
        })
        .await
        .context("orphans task aborted")?
    }

    /// List unresolved WikiLinks that require manual attention.
    pub async fn unresolved_links(&self) -> Result<Vec<LinkEdge>> {
        let db = Arc::clone(&self.database);
        task::spawn_blocking(move || {
            let conn = db.connection()?;
            let mut stmt = conn.prepare(
                "SELECT source_id, target_id, raw_text, display_text, heading, reason
                 FROM note_links
                 WHERE target_id IS NULL
                 ORDER BY source_id, raw_text",
            )?;
            let mut rows = stmt.query([])?;
            let mut edges = Vec::new();
            while let Some(row) = rows.next()? {
                let reason_raw: String = row.get(5)?;
                let reason = LinkReason::from_str(&reason_raw)
                    .with_context(|| format!("invalid unresolved reason '{reason_raw}'"))?;
                edges.push(LinkEdge {
                    source: row.get(0)?,
                    target: None,
                    raw: row.get(2)?,
                    display_text: row.get(3)?,
                    heading: row.get(4)?,
                    reason,
                });
            }
            Ok::<_, anyhow::Error>(edges)
        })
        .await
        .context("unresolved link task aborted")?
    }

    /// Fetch both inbound and outbound edges for the supplied note identifier.
    pub async fn context(&self, note_id: &str) -> Result<GraphContext> {
        let (backlinks, forward_links) =
            try_join!(self.backlinks(note_id), self.forward_links(note_id))?;
        Ok(GraphContext {
            backlinks,
            forward_links,
        })
    }
}

impl LinkResolutionMaps {
    /// Merge resolution hints extracted from a note into the cached maps.
    pub fn ingest_note(&mut self, note_id: &str, title: Option<&str>, aliases: &[String]) {
        if let Some(title) = title {
            let key = normalise_link_lookup(title);
            if !key.is_empty() {
                self.titles
                    .entry(key)
                    .or_insert_with(|| note_id.to_string());
            }
        }

        for alias in aliases {
            let key = normalise_link_lookup(alias);
            if key.is_empty() {
                continue;
            }
            let entry = self.aliases.entry(key).or_default();
            if !entry.iter().any(|existing| existing == note_id) {
                entry.push(note_id.to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::Path, sync::Arc};

    use crate::{
        indexer::{Indexer, IndexerConfig},
        vault::{Vault, VaultConfig},
    };
    use anyhow::Context;
    use tempfile::TempDir;

    fn fixture_vault() -> Arc<Vault> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tests")
            .join("fixtures")
            .join("test-vault");
        Arc::new(Vault::new(VaultConfig::new(root)).expect("fixture vault initialises"))
    }

    fn temp_db() -> Arc<IndexDatabase> {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("index.db");
        // Leak tempdir so the database remains accessible for the duration of the test.
        Box::leak(Box::new(dir));
        Arc::new(IndexDatabase::open(&db_path).expect("database opens"))
    }

    #[tokio::test]
    async fn graph_service_reports_expected_edges() -> Result<()> {
        let vault = fixture_vault();
        let database = temp_db();
        let indexer = Indexer::new(
            Arc::clone(&vault),
            Arc::clone(&database),
            IndexerConfig::default(),
            None,
        );
        indexer
            .index_all()
            .await
            .context("indexing fixture vault should succeed")?;

        let service = GraphService::new(database);

        let forward = service
            .forward_links("Link Variations Test")
            .await
            .context("forward links query should succeed")?;
        assert!(
            forward
                .iter()
                .any(|edge| edge.target.as_deref() == Some("Photography Equipment"))
        );
        assert!(
            forward
                .iter()
                .any(|edge| edge.display_text.as_deref() == Some("My Camera Gear"))
        );

        let backlinks = service
            .backlinks("Photography Equipment")
            .await
            .context("backlink query should succeed")?;
        let sources: Vec<&str> = backlinks.iter().map(|edge| edge.source.as_str()).collect();
        assert!(sources.contains(&"Link Variations Test"));
        assert!(sources.contains(&"Broken Links Test"));

        let unresolved = service
            .unresolved_links()
            .await
            .context("unresolved link query should succeed")?;
        assert!(unresolved.iter().any(|edge| {
            edge.source == "Broken Links Test"
                && edge.raw == "This Note Does Not Exist"
                && edge.reason == LinkReason::Unresolved
        }));

        let orphans = service
            .orphans()
            .await
            .context("orphans query should succeed")?;
        assert!(orphans.contains(&"Orphan Note".to_string()));

        let context = service
            .context("Photography Equipment")
            .await
            .context("context query should succeed")?;
        assert!(
            context
                .forward_links
                .iter()
                .any(|edge| edge.target.as_deref() == Some("Sigma 35mm Art"))
        );
        assert!(
            context
                .backlinks
                .iter()
                .any(|edge| edge.source == "Link Variations Test")
        );

        Ok(())
    }
}
