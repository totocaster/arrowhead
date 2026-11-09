#![cfg(unix)]

use std::{
    fs,
    path::Path,
    process::{Command, Output},
    thread,
};

use serde_json::Value;

const METRICS_FILE: &str = "Metrics/Shared.metrics.ndjson";

fn create_metric(workspace: &Path, id: &str, value: usize) -> Output {
    Command::new(env!("CARGO_BIN_EXE_arrowhead"))
        .arg("--vault")
        .arg(workspace.join("vault"))
        .arg("--config")
        .arg(workspace.join("config.toml"))
        .args(["metrics", "create", "--file", METRICS_FILE, "--id", id])
        .args(["--ts", "2026-04-14T08:30:00+04:00", "--key", "body.weight"])
        .args([
            "--value",
            &value.to_string(),
            "--source",
            "withings",
            "--json",
        ])
        .output()
        .expect("spawn arrowhead")
}

#[test]
fn concurrent_cli_creates_keep_every_record() {
    const WRITERS: usize = 8;
    const CREATES_PER_WRITER: usize = 6;
    let workspace = tempfile::tempdir().expect("tempdir");
    let workspace = workspace.path();
    fs::create_dir(workspace.join("vault")).expect("create vault");

    let seed = create_metric(workspace, "01SEED", 0);
    assert!(
        seed.status.success(),
        "seed create failed: {}",
        String::from_utf8_lossy(&seed.stderr)
    );

    let failures = thread::scope(|scope| {
        let writers = (0..WRITERS)
            .map(|writer| {
                scope.spawn(move || {
                    (0..CREATES_PER_WRITER)
                        .filter_map(|n| {
                            let id = format!("01W{writer}N{n}");
                            let output = create_metric(workspace, &id, writer * 100 + n);
                            (!output.status.success()).then(|| {
                                format!("{id}: {}", String::from_utf8_lossy(&output.stderr).trim())
                            })
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        writers
            .into_iter()
            .flat_map(|writer| writer.join().expect("join writer"))
            .collect::<Vec<_>>()
    });

    let content =
        fs::read_to_string(workspace.join("vault").join(METRICS_FILE)).expect("read metrics file");
    let mut stored_ids = content
        .lines()
        .map(|line| {
            let row: Value = serde_json::from_str(line).expect("metrics row is JSON");
            row["id"].as_str().expect("metrics row id").to_string()
        })
        .collect::<Vec<_>>();
    stored_ids.sort();
    let mut expected_ids = (0..WRITERS)
        .flat_map(|writer| (0..CREATES_PER_WRITER).map(move |n| format!("01W{writer}N{n}")))
        .chain(["01SEED".to_string()])
        .collect::<Vec<_>>();
    expected_ids.sort();
    let missing_ids = expected_ids
        .iter()
        .filter(|id| !stored_ids.contains(id))
        .collect::<Vec<_>>();
    assert!(
        failures.is_empty() && stored_ids == expected_ids,
        "rejected creates: {failures:#?}\nids missing from disk: {missing_ids:?}\nstored rows: {} of {}",
        stored_ids.len(),
        expected_ids.len()
    );
}
