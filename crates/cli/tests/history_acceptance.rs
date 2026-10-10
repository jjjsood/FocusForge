// SPDX-License-Identifier: GPL-3.0-or-later

//! A real edit history over `fixtures/mantis-512`, driven only through the
//! shipped binary: a laplacian stack, three chained checkpoints, one branch,
//! one delete, then every visible checkpoint's Z-Map exported in a fresh process.
//! Each op checkpoint's map must equal the root's, compared by
//! `tools/tiff-diff.py`, which shares no code with the writer under test, and
//! its export must differ from the root's.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use focusforge_core::project::Project;
use serde_json::{Value, json};

fn run(dir: &Path, args: &[&str]) -> common::CliOutput {
    common::run_in(&mut common::cli_bin("history_acceptance"), dir, args)
}

fn run_ok(dir: &Path, args: &[&str]) -> common::CliOutput {
    let output = run(dir, args);
    assert!(output.success, "{args:?} stderr: {}", output.stderr);
    output
}

fn arg(path: &Path) -> &str {
    path.to_str().expect("test paths are UTF-8")
}

/// The oracle's exit code over two single-channel index maps.
fn oracle(a: &Path, b: &Path) -> i32 {
    let output = Command::new("python3")
        .arg(common::workspace_root().join("tools/tiff-diff.py"))
        .args([a, b])
        .arg("--zmap")
        .output()
        .expect("python3 must be installed");
    let code = output.status.code().expect("the oracle exits normally");
    if code != 0 && code != 1 {
        panic!(
            "oracle failed on {a:?} {b:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    code
}

fn assert_identical(a: &Path, b: &Path) {
    assert_eq!(oracle(a, b), 0, "{a:?} differs from {b:?}");
}

fn only_project(dir: &Path) -> PathBuf {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "ffp"))
        .collect();
    assert_eq!(found.len(), 1, "exactly one project expected in {dir:?}");
    found.pop().unwrap()
}

fn listed(dir: &Path, project: &Path) -> Vec<Value> {
    let output = run_ok(dir, &["history", "list", arg(project), "--json"]);
    let doc: Value = serde_json::from_str(&output.stdout).unwrap();
    doc["checkpoints"].as_array().unwrap().clone()
}

/// `stroke` as the one-op list `retouch` reads.
fn write_op_file(dir: &Path, name: &str, stroke: &Value) -> PathBuf {
    let path = dir.join(format!("{name}.ops.json"));
    std::fs::write(&path, json!([stroke]).to_string()).unwrap();
    path
}

#[test]
fn real_history_round_trips() {
    let cwd = common::tmp_dir("history_acceptance");
    let out = cwd.join("out");
    let (after, strokes) = (cwd.join("after"), cwd.join("strokes"));
    for dir in [&out, &after, &strokes] {
        std::fs::create_dir_all(dir).unwrap();
    }

    let input = common::mantis_512_dir();
    let stack = common::push_no_gpu(vec![
        "stack",
        arg(&input),
        "-o",
        arg(&out),
        "--algorithm",
        "laplacian",
        "--no-align",
        "--no-crop",
        "--set",
        "equalize=false",
    ]);
    run_ok(&cwd, &stack);
    let project = only_project(&out);
    let golden = input.join("golden/laplacian-zmap.tif");

    let root = listed(&cwd, &project)[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let fixture = common::fixtures_dir().join("retouch/stroke-list.json");
    let all: Vec<Value> = serde_json::from_slice(&std::fs::read(fixture).unwrap()).unwrap();
    assert_eq!(all.len(), 3, "the fixture holds three strokes");

    // c1, c2, c3 chain; b is stroke 3 sealed straight under the root.
    let plan = [
        ("c1", 0, None),
        ("c2", 1, Some("c1")),
        ("c3", 2, Some("c2")),
        ("b", 2, None),
    ];
    let mut ids: BTreeMap<&str, String> = BTreeMap::from([("root", root.clone())]);
    for (name, stroke, parent) in plan {
        let ops = write_op_file(&strokes, name, &all[stroke]);
        let from = parent.map_or(root.as_str(), |p| ids[p].as_str());
        let output = run_ok(
            &cwd,
            &[
                "retouch",
                arg(&project),
                "--ops",
                arg(&ops),
                "--from",
                from,
                "--json",
            ],
        );
        let report: Value = serde_json::from_str(&output.stdout).unwrap();
        ids.insert(name, report["checkpoints"][0].as_str().unwrap().to_string());
    }

    let output = run_ok(
        &cwd,
        &[
            "history",
            "delete",
            arg(&project),
            "--checkpoint",
            &ids["c2"],
            "--json",
        ],
    );
    let report: Value = serde_json::from_str(&output.stdout).unwrap();
    assert_eq!(report["hidden"], ids["c2"].as_str());
    assert!(
        report["removed"].as_array().unwrap().is_empty(),
        "c3 is built on c2, so c2 stays stored"
    );

    let records = listed(&cwd, &project);
    let mut listed_ids: Vec<&str> = records.iter().map(|r| r["id"].as_str().unwrap()).collect();
    listed_ids.sort_unstable();
    let mut visible: Vec<&str> = ["root", "c1", "c3", "b"]
        .iter()
        .map(|name| ids[name].as_str())
        .collect();
    visible.sort_unstable();
    assert_eq!(listed_ids, visible, "exactly root, c1, c3 and b are listed");
    let parent_of = |name: &str| {
        records
            .iter()
            .find(|r| r["id"] == ids[name].as_str())
            .unwrap()["parent"]
            .clone()
    };
    assert_eq!(parent_of("c3"), ids["c1"].as_str(), "c3 lists c1 as parent");
    assert_eq!(parent_of("b"), ids["root"].as_str());

    // An op checkpoint is a record naming the root's Z-Map; the edits show in
    // its render, and a render equal to the root's would be no evidence.
    let recorded = Project::open(&project).unwrap();
    let root_blob = recorded.history().get(&root).unwrap().zmap_blob.clone();
    let export = |name: &str| {
        let out = after.join(format!("{name}-export.tif"));
        run_ok(
            &cwd,
            &[
                "export",
                arg(&project),
                "--checkpoint",
                &ids[name],
                "-o",
                arg(&out),
            ],
        );
        std::fs::read(&out).unwrap()
    };
    let root_render = export("root");
    for name in ["c1", "c3", "b"] {
        assert_eq!(
            recorded.history().get(&ids[name]).unwrap().zmap_blob,
            root_blob,
            "{name} names the root's Z-Map"
        );
        assert_ne!(export(name), root_render, "{name} renders its strokes");
    }

    // Every command above is its own process; each map is read back now.
    for name in ["root", "c1", "c3", "b"] {
        let exported = after.join(format!("{name}.tif"));
        run_ok(
            &cwd,
            &[
                "export",
                arg(&project),
                "--checkpoint",
                &ids[name],
                "--zmap",
                arg(&exported),
            ],
        );
        assert_identical(&exported, &golden);
    }
}
