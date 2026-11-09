//! Watcher-driven updates must leave the index exactly where a full pass would.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use arrowhead_core::{
    SearchConfig, SearchService, Vault, VaultConfig,
    indexer::{Indexer, IndexerConfig},
    sqlite::IndexDatabase,
};
use tempfile::TempDir;

struct Fixture {
    dir: TempDir,
    vault: Arc<Vault>,
    database: Arc<IndexDatabase>,
    indexer: Indexer,
}

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("vault");
        fs::create_dir(&root).expect("vault dir");
        for (relative, content) in files {
            write(&root.join(relative), content);
        }
        Self::open(dir, &root, "index.db")
    }

    fn open(dir: TempDir, root: &Path, database: &str) -> Self {
        let vault = Arc::new(Vault::new(VaultConfig::new(root.to_path_buf())).expect("vault"));
        let database =
            Arc::new(IndexDatabase::open(dir.path().join(database)).expect("index database"));
        let indexer = Indexer::new(
            Arc::clone(&vault),
            Arc::clone(&database),
            IndexerConfig::default(),
            None,
        );
        Self {
            dir,
            vault,
            database,
            indexer,
        }
    }

    fn root(&self) -> PathBuf {
        self.vault.paths().root.clone()
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root().join(relative)
    }

    fn outside(&self) -> PathBuf {
        let outside = self.dir.path().join("outside");
        fs::create_dir_all(&outside).expect("outside dir");
        outside
    }

    fn note_ids(&self) -> Vec<String> {
        self.database.list_note_ids().expect("indexed note ids")
    }

    fn metric_files(&self) -> Vec<String> {
        self.database
            .list_metric_files()
            .expect("indexed metric files")
            .into_iter()
            .map(|file| file.relative_path.to_string_lossy().into_owned())
            .collect()
    }

    async fn search(&self, query: &str) -> Vec<String> {
        SearchService::new(Arc::clone(&self.database), SearchConfig::default(), None)
            .search_fts(query, Some(10))
            .await
            .expect("full-text search")
            .into_iter()
            .map(|result| result.note_id)
            .collect()
    }
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().expect("parent dir")).expect("parent dirs");
    fs::write(path, content).expect("write file");
}

fn rewrite_keeping_mtime(path: &Path, content: &str) {
    let modified = fs::metadata(path)
        .and_then(|meta| meta.modified())
        .expect("original mtime");
    fs::write(path, content).expect("rewrite file");
    fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_times(fs::FileTimes::new().set_modified(modified)))
        .expect("restore mtime");
    assert_eq!(
        fs::metadata(path)
            .and_then(|meta| meta.modified())
            .expect("restored mtime"),
        modified,
        "the edit must keep the original mtime"
    );
}

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

#[tokio::test]
async fn directory_rename_moves_the_whole_subtree() {
    let fixture = Fixture::new(&[
        ("Projects/Alpha.md", "# Alpha\n"),
        ("Projects/Deep/Bravo.md", "# Bravo\n"),
        ("Keep.md", "# Keep\n"),
    ]);
    fixture.indexer.index_all().await.expect("full pass");

    fs::rename(fixture.path("Projects"), fixture.path("Archive")).expect("rename dir");
    fixture
        .indexer
        .reindex_paths(&[fixture.path("Projects"), fixture.path("Archive")])
        .await
        .expect("directory events");

    assert_eq!(
        fixture.note_ids(),
        ids(&["Archive/Alpha", "Archive/Deep/Bravo", "Keep"])
    );
}

#[tokio::test]
async fn directory_moved_out_of_the_vault_drops_its_notes() {
    let fixture = Fixture::new(&[
        ("Projects/Alpha.md", "# Alpha\n"),
        ("Projects/Deep/Bravo.md", "# Bravo\n"),
        ("Keep.md", "# Keep\n"),
    ]);
    fixture.indexer.index_all().await.expect("full pass");

    fs::rename(fixture.path("Projects"), fixture.outside().join("Projects"))
        .expect("move dir out of the vault");
    let stats = fixture
        .indexer
        .reindex_paths(&[fixture.path("Projects")])
        .await
        .expect("deleted directory event");

    assert_eq!(fixture.note_ids(), ids(&["Keep"]));
    assert_eq!(stats.removed, 2);
    assert_eq!(stats.errors, 0);
}

#[tokio::test]
async fn directory_event_indexes_notes_that_had_no_events_of_their_own() {
    let fixture = Fixture::new(&[("Keep.md", "# Keep\n")]);
    fixture.indexer.index_all().await.expect("full pass");

    let staging = fixture.outside().join("Inbox");
    write(&staging.join("New.md"), "# New\n\nfreshinbox\n");
    write(&staging.join("Sub/Deeper.md"), "# Deeper\n");
    fs::rename(&staging, fixture.path("Inbox")).expect("move dir into the vault");
    fixture
        .indexer
        .reindex_paths(&[fixture.path("Inbox")])
        .await
        .expect("directory event");

    assert_eq!(
        fixture.note_ids(),
        ids(&["Inbox/New", "Inbox/Sub/Deeper", "Keep"])
    );
    assert_eq!(fixture.search("freshinbox").await, ids(&["Inbox/New"]));
}

#[tokio::test]
async fn timestamp_preserving_edit_is_searchable_after_its_event() {
    let fixture = Fixture::new(&[("Old.md", "oldword\n")]);
    fixture.indexer.index_all().await.expect("full pass");

    rewrite_keeping_mtime(&fixture.path("Old.md"), "newword\n");
    let stats = fixture
        .indexer
        .reindex_paths(&[fixture.path("Old.md")])
        .await
        .expect("edit event");

    assert_eq!(stats.indexed, 1);
    assert_eq!(fixture.search("newword").await, ids(&["Old"]));
    assert!(fixture.search("oldword").await.is_empty());
}

#[tokio::test]
async fn full_pass_picks_up_timestamp_preserving_edits() {
    let fixture = Fixture::new(&[("Old.md", "oldword\n")]);
    fixture.indexer.index_all().await.expect("first full pass");

    rewrite_keeping_mtime(&fixture.path("Old.md"), "newword\n");
    fixture.indexer.index_all().await.expect("second full pass");

    assert_eq!(fixture.search("newword").await, ids(&["Old"]));
    assert!(fixture.search("oldword").await.is_empty());
}

#[tokio::test]
async fn unchanged_notes_stay_skipped() {
    let fixture = Fixture::new(&[("Same.md", "# Same\n")]);
    fixture.indexer.index_all().await.expect("first full pass");

    let stats = fixture
        .indexer
        .reindex_paths(&[fixture.path("Same.md")])
        .await
        .expect("spurious event");

    assert_eq!((stats.indexed, stats.skipped), (0, 1));
}

#[tokio::test]
async fn events_inside_excluded_folders_index_nothing() {
    let fixture = Fixture::new(&[
        (".trash/Deleted.md", "trashedunique\n"),
        ("Projects/attachments/Scan.md", "scanunique\n"),
        ("Projects/.hidden/Draft.md", "draftunique\n"),
    ]);
    assert!(
        fixture.vault.inventory().expect("inventory").is_empty(),
        "the full inventory excludes every fixture note"
    );

    let stats = fixture
        .indexer
        .reindex_paths(&[
            fixture.path(".trash/Deleted.md"),
            fixture.path("Projects/attachments/Scan.md"),
            fixture.path("Projects/.hidden/Draft.md"),
        ])
        .await
        .expect("events in excluded folders");

    assert_eq!(stats.indexed, 0);
    assert!(fixture.note_ids().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_notes_and_folders_index_nothing() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new(&[("Keep.md", "# Keep\n")]);
    let outside = fixture.outside();
    write(&outside.join("Linked.md"), "linkedunique\n");
    write(&outside.join("Folder/Inner.md"), "innerunique\n");
    symlink(outside.join("Linked.md"), fixture.path("Linked.md")).expect("note symlink");
    symlink(outside.join("Folder"), fixture.path("Folder")).expect("folder symlink");
    assert_eq!(
        fixture
            .vault
            .inventory()
            .expect("inventory")
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        ids(&["Keep"]),
        "the full inventory never follows symlinks"
    );

    fixture
        .indexer
        .reindex_paths(&[
            fixture.path("Linked.md"),
            fixture.path("Folder"),
            fixture.path("Folder/Inner.md"),
        ])
        .await
        .expect("events for symlinks");

    assert!(fixture.note_ids().is_empty());
}

#[tokio::test]
async fn events_for_every_path_match_a_full_pass() {
    let files = [
        ("Keep.md", "# Keep\n"),
        ("Projects/Alpha.md", "# Alpha\n"),
        ("Projects/Deep/Bravo.md", "# Bravo\n"),
        ("Projects/attachments/Scan.md", "# Scan\n"),
        ("Attachments/Top.md", "# Top\n"),
        (".trash/Deleted.md", "# Deleted\n"),
        (".obsidian/snippets/Snippet.md", "# Snippet\n"),
        ("Projects/.hidden/Draft.md", "# Draft\n"),
        (".dotfile.md", "# Dotfile\n"),
        ("Readme.txt", "not a note\n"),
        ("Metrics/health.metrics.ndjson", METRIC_A),
        ("Metrics/.archive/old.metrics.ndjson", METRIC_OLD),
        ("Metrics/Upper.METRICS.NDJSON", METRIC_UPPER),
    ];
    let full = Fixture::new(&files);
    full.indexer.index_all().await.expect("full pass");

    let incremental = Fixture::new(&files);
    let mut every_path = Vec::new();
    collect_paths(&incremental.root(), &mut every_path);
    incremental
        .indexer
        .reindex_paths(&every_path)
        .await
        .expect("events for every path");

    assert_eq!(incremental.note_ids(), full.note_ids());
    assert_eq!(incremental.metric_files(), full.metric_files());
}

const METRIC_A: &str = r#"{"id":"01AAA","ts":"2026-04-14T08:30:00+00:00","key":"body.weight","value":105.6,"unit":"kg","source":"withings"}"#;
const METRIC_B: &str = r#"{"id":"01AAB","ts":"2026-04-14T08:30:00+00:00","key":"body.weight","value":105.6,"unit":"kg","source":"withings"}"#;
const METRIC_OLD: &str = r#"{"id":"01OLD","ts":"2025-04-14T08:30:00+00:00","key":"body.weight","value":99.0,"unit":"kg","source":"withings"}"#;
const METRIC_UPPER: &str = r#"{"id":"01UPP","ts":"2025-05-14T08:30:00+00:00","key":"body.weight","value":98.0,"unit":"kg","source":"withings"}"#;

fn collect_paths(dir: &Path, paths: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("dir entry").path();
        paths.push(path.clone());
        if fs::symlink_metadata(&path).expect("metadata").is_dir() {
            collect_paths(&path, paths);
        }
    }
}

#[tokio::test]
async fn metrics_directory_rename_moves_its_files() {
    let fixture = Fixture::new(&[
        ("Metrics/health.metrics.ndjson", METRIC_A),
        ("Metrics/Old/legacy.metrics.ndjson", METRIC_OLD),
    ]);
    fixture.indexer.index_all().await.expect("full pass");

    fs::rename(fixture.path("Metrics/Old"), fixture.path("Metrics/Archive"))
        .expect("rename metrics dir");
    let stats = fixture
        .indexer
        .reindex_paths(&[fixture.path("Metrics/Old"), fixture.path("Metrics/Archive")])
        .await
        .expect("metrics directory events");

    assert_eq!(stats.errors, 0);
    assert_eq!(
        fixture.metric_files(),
        ids(&[
            "Metrics/Archive/legacy.metrics.ndjson",
            "Metrics/health.metrics.ndjson"
        ])
    );
}

#[tokio::test]
async fn full_pass_moves_metrics_files_renamed_while_offline() {
    let fixture = Fixture::new(&[
        ("Metrics/health.metrics.ndjson", METRIC_A),
        ("Metrics/Old/legacy.metrics.ndjson", METRIC_OLD),
    ]);
    fixture.indexer.index_all().await.expect("first full pass");

    fs::rename(fixture.path("Metrics/Old"), fixture.path("Metrics/Archive"))
        .expect("rename metrics dir");
    let stats = fixture.indexer.index_all().await.expect("second full pass");

    assert_eq!(stats.errors, 0);
    assert_eq!(
        fixture.metric_files(),
        ids(&[
            "Metrics/Archive/legacy.metrics.ndjson",
            "Metrics/health.metrics.ndjson"
        ])
    );
}

#[tokio::test]
async fn metrics_timestamp_preserving_edit_replaces_records() {
    let fixture = Fixture::new(&[("Metrics/health.metrics.ndjson", METRIC_A)]);
    fixture.indexer.index_all().await.expect("full pass");

    rewrite_keeping_mtime(&fixture.path("Metrics/health.metrics.ndjson"), METRIC_B);
    fixture
        .indexer
        .reindex_paths(&[fixture.path("Metrics/health.metrics.ndjson")])
        .await
        .expect("metrics edit event");

    assert!(
        fixture
            .database
            .metric_record_by_id("01AAB")
            .expect("record lookup")
            .is_some()
    );
    assert!(
        fixture
            .database
            .metric_record_by_id("01AAA")
            .expect("record lookup")
            .is_none()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_metrics_files_index_nothing() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::new(&[("Metrics/health.metrics.ndjson", METRIC_A)]);
    let outside = fixture.outside().join("linked.metrics.ndjson");
    write(&outside, METRIC_OLD);
    symlink(&outside, fixture.path("Metrics/linked.metrics.ndjson")).expect("metrics symlink");
    fixture.indexer.index_all().await.expect("full pass");
    assert_eq!(
        fixture.metric_files(),
        ids(&["Metrics/health.metrics.ndjson"]),
        "the full pass never follows symlinks"
    );

    fixture
        .indexer
        .reindex_paths(&[fixture.path("Metrics/linked.metrics.ndjson")])
        .await
        .expect("metrics symlink event");

    assert_eq!(
        fixture.metric_files(),
        ids(&["Metrics/health.metrics.ndjson"])
    );
}
