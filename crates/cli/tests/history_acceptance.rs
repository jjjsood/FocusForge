// SPDX-License-Identifier: GPL-3.0-or-later

//! A real edit history over `fixtures/mantis-512`, driven only through the
//! shipped binary: a laplacian stack, three chained checkpoints, one branch,
//! one delete, then every visible checkpoint materialized in a fresh process.
//! Each map must equal the one its own seal wrote and the one a
//! `retouch` chain produces without ever reading a project, compared by
//! `tools/tiff-diff.py`, which shares no code with the writer under test.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

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

fn assert_differ(a: &Path, b: &Path) {
    assert_eq!(oracle(a, b), 1, "{a:?} and {b:?} are expected to differ");
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

/// `stroke` as the two op files a command reads: `{"ops":[stroke]}` for
/// `history apply`, a bare `[stroke]` for `retouch`.
fn write_op_files(dir: &Path, name: &str, stroke: &Value) -> (PathBuf, PathBuf) {
    let wrapped = dir.join(format!("{name}.ops.json"));
    let bare = dir.join(format!("{name}.bare.json"));
    std::fs::write(&wrapped, json!({ "ops": [stroke] }).to_string()).unwrap();
    std::fs::write(&bare, json!([stroke]).to_string()).unwrap();
    (wrapped, bare)
}

#[test]
fn real_history_round_trips() {
    let cwd = common::tmp_dir("history_acceptance");
    let out = cwd.join("out");
    let (before, after, expected, strokes) = (
        cwd.join("before"),
        cwd.join("after"),
        cwd.join("expected"),
        cwd.join("strokes"),
    );
    for dir in [&out, &before, &after, &expected, &strokes] {
        std::fs::create_dir_all(dir).unwrap();
    }

    let input = common::mantis_512_dir();
    let stack = common::push_no_gpu(vec![
        "stack",
        arg(&input),
        "-o",
        arg(&out),
        "--algo",
        "laplacian",
        "--no-align",
        "--no-crop",
    ]);
    run_ok(&cwd, &stack);
    let project = only_project(&out);
    let golden = input.join("golden/laplacian-zmap.tif");

    let root = listed(&cwd, &project)[0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let root_before = before.join("root.tif");
    run_ok(
        &cwd,
        &[
            "history",
            "materialize",
            arg(&project),
            "--checkpoint",
            &root,
            "-o",
            arg(&root_before),
        ],
    );

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
        let (wrapped, bare) = write_op_files(&strokes, name, &all[stroke]);
        let from = parent.map_or(root.as_str(), |p| ids[p].as_str());
        let sealed_map = before.join(format!("{name}.tif"));
        let output = run_ok(
            &cwd,
            &[
                "history",
                "apply",
                arg(&project),
                "--ops",
                arg(&wrapped),
                "--from",
                from,
                "--save",
                "-o",
                arg(&sealed_map),
                "--json",
            ],
        );
        let report: Value = serde_json::from_str(&output.stdout).unwrap();
        ids.insert(name, report["checkpoint"].as_str().unwrap().to_string());

        let prev = parent.map_or(golden.clone(), |p| expected.join(format!("{p}.tif")));
        run_ok(
            &cwd,
            &[
                "retouch",
                "--zmap",
                arg(&prev),
                "--ops",
                arg(&bare),
                "--out",
                arg(&expected.join(format!("{name}.tif"))),
            ],
        );
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

    // The edits are real: a map equal to everything is no evidence.
    assert_differ(&before.join("c1.tif"), &golden);
    assert_differ(&before.join("c3.tif"), &before.join("c2.tif"));
    assert_differ(&before.join("b.tif"), &golden);

    // Every command above is its own process; each map is read back now.
    for name in ["root", "c1", "c3", "b"] {
        let materialized = after.join(format!("{name}.tif"));
        run_ok(
            &cwd,
            &[
                "history",
                "materialize",
                arg(&project),
                "--checkpoint",
                &ids[name],
                "-o",
                arg(&materialized),
            ],
        );
        assert_identical(&materialized, &before.join(format!("{name}.tif")));
        let independent = if name == "root" {
            golden.clone()
        } else {
            expected.join(format!("{name}.tif"))
        };
        assert_identical(&materialized, &independent);
    }
}
