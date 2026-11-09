//! Stored WikiLinks must follow the notes they can resolve to, without touching their sources.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use arrowhead_core::{
    GraphService, LinkEdge, LinkReason, Vault, VaultConfig,
    indexer::{Indexer, IndexerConfig},
    sqlite::IndexDatabase,
};
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    vault: Arc<Vault>,
    indexer: Indexer,
    graph: GraphService,
}

impl Fixture {
    async fn indexed(files: &[(&str, &str)]) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("vault");
        fs::create_dir(&root).expect("vault dir");
        for (relative, content) in files {
            write(&root.join(relative), content);
        }
        let vault = Arc::new(Vault::new(VaultConfig::new(root)).expect("vault"));
        let database =
            Arc::new(IndexDatabase::open(dir.path().join("index.db")).expect("index database"));
        let indexer = Indexer::new(
            Arc::clone(&vault),
            Arc::clone(&database),
            IndexerConfig::default(),
            None,
        );
        indexer.index_all().await.expect("initial full pass");
        Self {
            _dir: dir,
            vault,
            indexer,
            graph: GraphService::new(database),
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.vault.paths().root.join(relative)
    }

    async fn reindex(&self, relative: &[&str]) {
        let paths: Vec<PathBuf> = relative.iter().map(|path| self.path(path)).collect();
        let stats = self
            .indexer
            .reindex_paths(&paths)
            .await
            .expect("targeted reindex");
        assert_eq!(stats.errors, 0);
    }

    async fn links_from(&self, source: &str) -> Vec<LinkEdge> {
        self.graph
            .forward_links(source)
            .await
            .expect("forward links")
    }

    async fn backlink_sources(&self, target: &str) -> Vec<String> {
        self.graph
            .backlinks(target)
            .await
            .expect("backlinks")
            .into_iter()
            .map(|edge| edge.source)
            .collect()
    }
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().expect("parent dir")).expect("parent dirs");
    fs::write(path, content).expect("write file");
}

fn file_snapshot(path: &Path) -> (String, SystemTime) {
    let content = fs::read_to_string(path).expect("read source");
    let modified = fs::metadata(path)
        .and_then(|meta| meta.modified())
        .expect("source mtime");
    (content, modified)
}

fn edge(source: &str, target: Option<&str>, raw: &str, reason: LinkReason) -> LinkEdge {
    LinkEdge {
        source: source.to_string(),
        target: target.map(str::to_string),
        raw: raw.to_string(),
        display_text: None,
        heading: None,
        reason,
    }
}

#[tokio::test]
async fn creating_the_target_later_resolves_existing_links() {
    let fixture = Fixture::indexed(&[("Source.md", "See [[Target#Plans|the plan]].\n")]).await;
    let source_before = file_snapshot(&fixture.path("Source.md"));

    write(&fixture.path("Target.md"), "# Target\n");
    fixture.reindex(&["Target.md"]).await;

    assert_eq!(
        fixture.graph.backlinks("Target").await.expect("backlinks"),
        vec![LinkEdge {
            source: "Source".to_string(),
            target: Some("Target".to_string()),
            raw: "Target#Plans|the plan".to_string(),
            display_text: Some("the plan".to_string()),
            heading: Some("Plans".to_string()),
            reason: LinkReason::Direct,
        }]
    );
    assert_eq!(
        file_snapshot(&fixture.path("Source.md")),
        source_before,
        "the source note is never rewritten"
    );
}

#[tokio::test]
async fn full_pass_resolves_links_to_notes_created_later() {
    let fixture = Fixture::indexed(&[("Source.md", "See [[Target]].\n")]).await;

    write(&fixture.path("Target.md"), "# Target\n");
    fixture.indexer.index_all().await.expect("second full pass");

    assert_eq!(fixture.backlink_sources("Target").await, vec!["Source"]);
}

#[tokio::test]
async fn deleting_the_target_unresolves_its_backlinks() {
    let fixture = Fixture::indexed(&[
        ("Source.md", "See [[Target]].\n"),
        ("Target.md", "# Target\n"),
    ])
    .await;

    fs::remove_file(fixture.path("Target.md")).expect("delete target");
    fixture.reindex(&["Target.md"]).await;

    assert_eq!(
        fixture.links_from("Source").await,
        vec![edge("Source", None, "Target", LinkReason::Unresolved)]
    );
    assert_eq!(
        fixture
            .graph
            .unresolved_links()
            .await
            .expect("unresolved links"),
        vec![edge("Source", None, "Target", LinkReason::Unresolved)]
    );
}

#[tokio::test]
async fn renamed_target_keeps_alias_links_and_drops_direct_ones() {
    let fixture = Fixture::indexed(&[
        ("Source.md", "See [[Target]] and [[TGT]].\n"),
        (
            "Target.md",
            "---\naliases:\n  - TGT\n---\n\n# Launch plan\n",
        ),
    ])
    .await;

    fs::rename(fixture.path("Target.md"), fixture.path("Moved.md")).expect("rename target");
    fixture.reindex(&["Target.md", "Moved.md"]).await;

    assert_eq!(
        fixture.links_from("Source").await,
        vec![
            edge("Source", None, "Target", LinkReason::Unresolved),
            edge("Source", Some("Moved"), "TGT", LinkReason::Alias),
        ]
    );
}

#[tokio::test]
async fn alias_links_follow_ambiguity_changes() {
    let fixture = Fixture::indexed(&[
        ("Source.md", "See [[Shared]].\n"),
        ("Alpha.md", "---\naliases:\n  - Shared\n---\n\n# Alpha\n"),
        ("Bravo.md", "---\naliases:\n  - Shared\n---\n\n# Bravo\n"),
    ])
    .await;
    assert_eq!(
        fixture.links_from("Source").await,
        vec![edge("Source", None, "Shared", LinkReason::Unresolved)],
        "two notes claim the alias"
    );

    write(&fixture.path("Bravo.md"), "# Bravo\n");
    fixture.reindex(&["Bravo.md"]).await;
    assert_eq!(
        fixture.links_from("Source").await,
        vec![edge("Source", Some("Alpha"), "Shared", LinkReason::Alias)],
        "one claimant left"
    );

    write(
        &fixture.path("Charlie.md"),
        "---\naliases:\n  - Shared\n---\n\n# Charlie\n",
    );
    fixture.reindex(&["Charlie.md"]).await;
    assert_eq!(
        fixture.links_from("Source").await,
        vec![edge("Source", None, "Shared", LinkReason::Unresolved)],
        "a new claimant makes the alias ambiguous again"
    );
}

#[tokio::test]
async fn title_links_follow_title_changes() {
    let fixture = Fixture::indexed(&[
        ("Source.md", "See [[Old Title]] and [[New Title]].\n"),
        ("Target.md", "# Old Title\n"),
    ])
    .await;
    assert_eq!(
        fixture.links_from("Source").await,
        vec![
            edge("Source", None, "New Title", LinkReason::Unresolved),
            edge("Source", Some("Target"), "Old Title", LinkReason::Title),
        ]
    );

    write(&fixture.path("Target.md"), "# New Title\n");
    fixture.reindex(&["Target.md"]).await;

    assert_eq!(
        fixture.links_from("Source").await,
        vec![
            edge("Source", None, "Old Title", LinkReason::Unresolved),
            edge("Source", Some("Target"), "New Title", LinkReason::Title),
        ]
    );
}

#[tokio::test]
async fn direct_links_win_over_titles_of_other_notes() {
    let fixture =
        Fixture::indexed(&[("Source.md", "See [[Plan]].\n"), ("Roadmap.md", "# Plan\n")]).await;
    assert_eq!(
        fixture.links_from("Source").await,
        vec![edge("Source", Some("Roadmap"), "Plan", LinkReason::Title)]
    );

    write(&fixture.path("Plan.md"), "# Something else\n");
    fixture.reindex(&["Plan.md"]).await;

    assert_eq!(
        fixture.links_from("Source").await,
        vec![edge("Source", Some("Plan"), "Plan", LinkReason::Direct)]
    );
}
