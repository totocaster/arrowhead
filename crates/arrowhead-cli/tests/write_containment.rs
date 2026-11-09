#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// A temporary workspace holding a vault, a folder outside it, and an empty CLI config, which
/// turns embeddings off so no model is loaded.
struct Workspace {
    _dir: tempfile::TempDir,
    vault: PathBuf,
    outside: PathBuf,
    config: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let vault = dir.path().join("vault");
        let outside = dir.path().join("outside");
        let config = dir.path().join("config.toml");
        fs::create_dir(&vault).expect("create vault");
        fs::create_dir(&outside).expect("create outside folder");
        fs::write(&config, "").expect("write config");
        Self {
            _dir: dir,
            vault,
            outside,
            config,
        }
    }

    fn arrowhead(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_arrowhead"))
            .arg("--vault")
            .arg(&self.vault)
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .output()
            .expect("spawn arrowhead")
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).expect("read file")
}

#[test]
fn notes_update_does_not_write_through_a_hard_link() {
    let workspace = Workspace::new();
    let sentinel = workspace.outside.join("sentinel.md");
    fs::write(&sentinel, "outside original\n").expect("write sentinel");
    fs::hard_link(&sentinel, workspace.vault.join("Hard.md")).expect("hard link");

    let output = workspace.arrowhead(&["notes", "update", "Hard", "--content", "agent wrote this"]);

    assert!(
        output.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(read(&sentinel), "outside original\n");
    assert!(read(&workspace.vault.join("Hard.md")).contains("agent wrote this"));
}

#[test]
fn notes_create_refuses_another_spelling_of_an_ignored_folder() {
    let workspace = Workspace::new();
    fs::create_dir_all(workspace.vault.join(".obsidian")).expect("obsidian dir");
    fs::write(
        workspace.vault.join(".obsidian/app.json"),
        r#"{ "userIgnoreFilters": ["Private/"] }"#,
    )
    .expect("write app config");
    fs::create_dir(workspace.vault.join("Private")).expect("ignored folder");
    if !workspace.vault.join("PRIVATE").exists() {
        // On a case-sensitive filesystem `PRIVATE` is an ordinary, separate folder.
        return;
    }

    let output =
        workspace.arrowhead(&["notes", "create", "--id", "PRIVATE/Leak", "--content", "x"]);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "the alias must be refused");
    assert!(
        stderr.contains("the vault spells `PRIVATE` as `Private`"),
        "{stderr}"
    );
    assert!(!workspace.vault.join("Private/Leak.md").exists());
}

#[test]
fn metrics_commands_refuse_a_symlinked_metrics_root() {
    let workspace = Workspace::new();
    let row = r#"{"id":"01OUT","ts":"2026-04-14T08:30:00+00:00","key":"body.weight","value":1.0,"source":"outside"}"#;
    fs::write(
        workspace.outside.join("Data.metrics.ndjson"),
        format!("{row}\n"),
    )
    .expect("write outside metrics");
    std::os::unix::fs::symlink(&workspace.outside, workspace.vault.join("Metrics"))
        .expect("symlink metrics root");

    let create = workspace.arrowhead(&[
        "metrics",
        "create",
        "--file",
        "Metrics/Proof.metrics.ndjson",
        "--id",
        "01NEW",
        "--ts",
        "2026-04-14T08:30:00+00:00",
        "--key",
        "body.weight",
        "--value",
        "2",
        "--source",
        "agent",
        "--json",
    ]);
    let update = workspace.arrowhead(&["metrics", "update", "01OUT", "--value", "9", "--json"]);

    for (command, output) in [("create", create), ("update", update)] {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{command} must fail");
        assert!(stderr.contains("is a symbolic link"), "{command}: {stderr}");
    }
    assert!(!workspace.outside.join("Proof.metrics.ndjson").exists());
    assert_eq!(
        read(&workspace.outside.join("Data.metrics.ndjson")),
        format!("{row}\n")
    );
}
