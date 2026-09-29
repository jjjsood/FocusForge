// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli history`: one `dummy-stack` run over `fixtures/mantis-512`
//! is shared through a `OnceLock` (fast: no pyramid, no alignment, no
//! crop); every test copies the sealed project into its own temp directory
//! before mutating it, so a `--save` or `delete` in one test never reaches
//! another.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use focusforge_core::align::Registration;
use focusforge_core::io::{TiffSpec, TiffWriter};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::project::{self, Checkpoint, Metrics, Op, Point, Project, SealRequest};
use focusforge_core::tile::Rect;
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};
use serde_json::Value;

fn bin() -> Command {
    common::cli_bin("history_cli")
}

struct CliOutput {
    success: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(dir: &Path, args: &[&str]) -> CliOutput {
    let output = bin()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run binary");
    CliOutput {
        success: output.status.success(),
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
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

struct SharedStack {
    _cwd: common::TmpDir,
    project: PathBuf,
}

/// One `dummy-stack` run over `fixtures/mantis-512`, paid once for this
/// whole binary.
fn shared_stack() -> &'static SharedStack {
    static ONCE: OnceLock<SharedStack> = OnceLock::new();
    ONCE.get_or_init(|| {
        let cwd = common::tmp_dir("history_cli_shared_stack");
        let out = cwd.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let input = common::fixtures_dir().join("mantis-512");
        let output = run(
            &cwd,
            &[
                "stack",
                input.to_str().unwrap(),
                "-o",
                out.to_str().unwrap(),
                "--algo",
                "dummy-stack",
                "--no-align",
                "--no-crop",
            ],
        );
        assert!(
            output.success,
            "stack must succeed -- stderr: {}",
            output.stderr
        );
        let project = only_project(&out);
        SharedStack { _cwd: cwd, project }
    })
}

/// A fresh copy of the shared project's `.ffp`, in its own temp directory.
fn fresh_project(name: &str) -> (common::TmpDir, PathBuf) {
    let dir = common::tmp_dir(name);
    let source = &shared_stack().project;
    let dest = dir.join(source.file_name().unwrap());
    std::fs::copy(source, &dest).unwrap();
    (dir, dest)
}

fn root_id(project: &Path) -> String {
    Project::open(project).unwrap().checkpoint().id.clone()
}

fn read_checkpoint(project: &Path, id: &str) -> Checkpoint {
    let project = Project::open(project).unwrap();
    project.history().get(id).unwrap().clone()
}

fn write_op_file(dir: &Path, name: &str, body: &Value) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_string(body).unwrap()).unwrap();
    path
}

/// Two brush strokes and a small reset, well inside a 512x512, 8-frame map --
/// [`shared_stack`]'s own geometry.
fn three_strokes() -> Vec<Op> {
    vec![
        Op::BrushStroke {
            source_id: 1,
            path: vec![Point { x: 20, y: 20 }, Point { x: 24, y: 24 }],
            radius: 3.0,
            softness: 0.0,
        },
        Op::BrushStroke {
            source_id: 2,
            path: vec![Point { x: 100, y: 100 }],
            radius: 2.0,
            softness: 0.0,
        },
        Op::ResetRegion {
            rect: Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
        },
    ]
}

fn op_file(body: &Value) -> Value {
    serde_json::json!({"format": 1, "ops": body})
}

#[test]
fn list_prints_one_record_per_checkpoint() {
    let (dir, project) = fresh_project("list_prints_one_record_per_checkpoint");
    let root = root_id(&project);

    let ops = write_op_file(
        &dir,
        "ops.json",
        &op_file(&serde_json::to_value(three_strokes()).unwrap()),
    );
    let output = run(
        &dir,
        &[
            "history",
            "apply",
            project.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "--save",
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let child: Value = serde_json::from_str(&output.stdout).unwrap();
    let child_id = child["checkpoint"].as_str().unwrap().to_string();

    let output = run(
        &dir,
        &["history", "list", project.to_str().unwrap(), "--json"],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let doc: Value = serde_json::from_str(&output.stdout).expect("valid JSON");
    assert_eq!(doc["format"], 1);
    let checkpoints = doc["checkpoints"].as_array().unwrap();
    assert_eq!(checkpoints.len(), 2);
    // Newest first: the just-sealed child before its parent.
    assert_eq!(checkpoints[0]["id"], child_id);
    assert_eq!(checkpoints[0]["parent"], root.clone());
    assert_eq!(checkpoints[1]["id"], root);
    assert!(checkpoints[1]["parent"].is_null());
    assert_eq!(checkpoints[1]["algorithm"], "dummy-stack");
}

#[test]
fn materialize_matches_core_replay() {
    let (dir, project) = fresh_project("materialize_matches_core_replay");
    let root = root_id(&project);

    let before_root_zmap = dir.join("root-before.tif");
    let output = run(
        &dir,
        &[
            "export",
            project.to_str().unwrap(),
            "--zmap",
            before_root_zmap.to_str().unwrap(),
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);

    let ops_a = write_op_file(
        &dir,
        "ops-a.json",
        &op_file(&serde_json::to_value(three_strokes()).unwrap()),
    );
    let output = run(
        &dir,
        &[
            "history",
            "apply",
            project.to_str().unwrap(),
            "--ops",
            ops_a.to_str().unwrap(),
            "--save",
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let child_a: Value = serde_json::from_str(&output.stdout).unwrap();
    let child_a_id = child_a["checkpoint"].as_str().unwrap().to_string();

    // A second seal makes `child_a` non-head.
    let ops_b = write_op_file(
        &dir,
        "ops-b.json",
        &op_file(
            &serde_json::to_value(vec![Op::BrushStroke {
                source_id: 3,
                path: vec![Point { x: 200, y: 200 }],
                radius: 2.0,
                softness: 0.0,
            }])
            .unwrap(),
        ),
    );
    let output = run(
        &dir,
        &[
            "history",
            "apply",
            project.to_str().unwrap(),
            "--ops",
            ops_b.to_str().unwrap(),
            "--save",
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);

    // Materialize the non-head, op-bearing checkpoint through the CLI, and
    // compare against `core`'s own `materialize_zmap` of the same
    // checkpoint.
    let cli_out = dir.join("materialize-child-a.tif");
    let output = run(
        &dir,
        &[
            "history",
            "materialize",
            project.to_str().unwrap(),
            "--checkpoint",
            &child_a_id,
            "-o",
            cli_out.to_str().unwrap(),
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);

    let oracle_out = dir.join("oracle-child-a.tif");
    let mut oracle_project = Project::open(&project).unwrap();
    let checkpoint = oracle_project.history().get(&child_a_id).unwrap().clone();
    focusforge_core::project::materialize_zmap(&mut oracle_project, &checkpoint, &oracle_out)
        .unwrap();
    assert_eq!(
        std::fs::read(&cli_out).unwrap(),
        std::fs::read(&oracle_out).unwrap()
    );

    // The root's own map never moved.
    let root_out = dir.join("materialize-root.tif");
    let output = run(
        &dir,
        &[
            "history",
            "materialize",
            project.to_str().unwrap(),
            "--checkpoint",
            &root,
            "-o",
            root_out.to_str().unwrap(),
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    assert_eq!(
        std::fs::read(&root_out).unwrap(),
        std::fs::read(&before_root_zmap).unwrap()
    );
}

#[test]
fn apply_and_seal_creates_a_child() {
    let (dir, project) = fresh_project("apply_and_seal_creates_a_child");
    let root = root_id(&project);
    let root_checkpoint_before = read_checkpoint(&project, &root);

    let ops = write_op_file(
        &dir,
        "ops.json",
        &op_file(&serde_json::to_value(three_strokes()).unwrap()),
    );

    // `-o` alone writes the edited map and leaves the project untouched.
    let before_bytes = std::fs::read(&project).unwrap();
    let unsaved_out = dir.join("unsaved.tif");
    let output = run(
        &dir,
        &[
            "history",
            "apply",
            project.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "-o",
            unsaved_out.to_str().unwrap(),
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    assert_eq!(
        std::fs::read(&project).unwrap(),
        before_bytes,
        "-o alone must not touch the project"
    );
    assert!(unsaved_out.exists());

    // `--save` seals a child of the head (the root, here).
    let output = run(
        &dir,
        &[
            "history",
            "apply",
            project.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "--save",
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let report: Value = serde_json::from_str(&output.stdout).unwrap();
    assert_eq!(report["from"], root);
    let child_id = report["checkpoint"].as_str().unwrap().to_string();
    assert_ne!(child_id, root);

    // The root's own checkpoint entry is unchanged.
    assert_eq!(root_checkpoint_before, read_checkpoint(&project, &root));

    // The sealed child's own map equals what the unsaved `-o` run wrote.
    let materialized = dir.join("materialized-child.tif");
    let output = run(
        &dir,
        &[
            "history",
            "materialize",
            project.to_str().unwrap(),
            "--checkpoint",
            &child_id,
            "-o",
            materialized.to_str().unwrap(),
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    assert_eq!(
        std::fs::read(&materialized).unwrap(),
        std::fs::read(&unsaved_out).unwrap()
    );
}

#[test]
fn sealing_from_an_older_checkpoint_branches() {
    let (dir, project) = fresh_project("sealing_from_an_older_checkpoint_branches");
    let root = root_id(&project);

    // Two distinct op lists: sealing the same ops onto the same parent in
    // the same wall-clock second would derive the same checkpoint id.
    let ops_a = write_op_file(
        &dir,
        "ops-a.json",
        &op_file(&serde_json::to_value(three_strokes()).unwrap()),
    );
    let ops_b = write_op_file(
        &dir,
        "ops-b.json",
        &op_file(
            &serde_json::to_value(vec![Op::BrushStroke {
                source_id: 4,
                path: vec![Point { x: 300, y: 300 }],
                radius: 2.0,
                softness: 0.0,
            }])
            .unwrap(),
        ),
    );

    // Seal onto the head (the root): the head advances.
    let output = run(
        &dir,
        &[
            "history",
            "apply",
            project.to_str().unwrap(),
            "--ops",
            ops_a.to_str().unwrap(),
            "--save",
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let child_a: Value = serde_json::from_str(&output.stdout).unwrap();
    let child_a_id = child_a["checkpoint"].as_str().unwrap().to_string();

    // Seal a second time, explicitly `--from` the root: a branch, not a
    // third generation.
    let output = run(
        &dir,
        &[
            "history",
            "apply",
            project.to_str().unwrap(),
            "--ops",
            ops_b.to_str().unwrap(),
            "--from",
            &root,
            "--save",
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let child_b: Value = serde_json::from_str(&output.stdout).unwrap();
    let child_b_id = child_b["checkpoint"].as_str().unwrap().to_string();
    assert_eq!(child_b["from"], root);
    assert_ne!(child_a_id, child_b_id);

    let output = run(
        &dir,
        &["history", "list", project.to_str().unwrap(), "--json"],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let doc: Value = serde_json::from_str(&output.stdout).unwrap();
    let checkpoints = doc["checkpoints"].as_array().unwrap();
    assert_eq!(checkpoints.len(), 3);
    let parent_of = |id: &str| -> Value {
        checkpoints
            .iter()
            .find(|c| c["id"] == id)
            .unwrap_or_else(|| panic!("{id} missing from list"))["parent"]
            .clone()
    };
    assert_eq!(parent_of(&child_a_id), Value::String(root.clone()));
    assert_eq!(parent_of(&child_b_id), Value::String(root));
}

#[test]
fn delete_with_descendants_needs_confirmation() {
    let (dir, project) = fresh_project("delete_with_descendants_needs_confirmation");
    let root = root_id(&project);

    let ops = write_op_file(
        &dir,
        "ops.json",
        &op_file(&serde_json::to_value(three_strokes()).unwrap()),
    );
    let seal = |dir: &Path, project: &Path, from: Option<&str>| -> String {
        let mut args = vec![
            "history",
            "apply",
            project.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
        ];
        if let Some(from) = from {
            args.push("--from");
            args.push(from);
        }
        args.push("--save");
        args.push("--json");
        let output = run(dir, &args);
        assert!(output.success, "stderr: {}", output.stderr);
        let report: Value = serde_json::from_str(&output.stdout).unwrap();
        report["checkpoint"].as_str().unwrap().to_string()
    };

    let child = seal(&dir, &project, None);
    let grandchild = seal(&dir, &project, None);

    // Deleting a leaf succeeds and reports it.
    let output = run(
        &dir,
        &[
            "history",
            "delete",
            project.to_str().unwrap(),
            "--checkpoint",
            &grandchild,
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let report: Value = serde_json::from_str(&output.stdout).unwrap();
    assert_eq!(
        report["removed"].as_array().unwrap(),
        &[Value::String(grandchild.clone())]
    );

    // `child` now has no descendants; reseal one so the next delete has
    // exactly one to refuse over.
    let grandchild_2 = seal(&dir, &project, Some(&child));

    let before = std::fs::read(&project).unwrap();
    let output = run(
        &dir,
        &[
            "history",
            "delete",
            project.to_str().unwrap(),
            "--checkpoint",
            &child,
        ],
    );
    assert!(!output.success);
    assert!(
        output.stderr.contains('1'),
        "stderr must name the descendant count: {}",
        output.stderr
    );
    assert_eq!(std::fs::read(&project).unwrap(), before);

    let output = run(
        &dir,
        &[
            "history",
            "delete",
            project.to_str().unwrap(),
            "--checkpoint",
            &child,
            "--with-descendants",
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let report: Value = serde_json::from_str(&output.stdout).unwrap();
    let removed: Vec<String> = report["removed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(removed.contains(&child));
    assert!(removed.contains(&grandchild_2));

    let output = run(
        &dir,
        &["history", "list", project.to_str().unwrap(), "--json"],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let doc: Value = serde_json::from_str(&output.stdout).unwrap();
    let ids: Vec<String> = doc["checkpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect();
    assert!(!ids.contains(&child));
    assert!(!ids.contains(&grandchild_2));
    assert!(ids.contains(&root));

    // The root is always refused.
    let output = run(
        &dir,
        &[
            "history",
            "delete",
            project.to_str().unwrap(),
            "--checkpoint",
            &root,
        ],
    );
    assert!(!output.success);
}

#[test]
fn unknown_checkpoint_id_is_refused() {
    let (dir, project) = fresh_project("unknown_checkpoint_id_is_refused");
    let root = root_id(&project);
    let ops = write_op_file(
        &dir,
        "ops.json",
        &op_file(&serde_json::to_value(three_strokes()).unwrap()),
    );
    let output = run(
        &dir,
        &[
            "history",
            "apply",
            project.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "--save",
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    // Two checkpoints now share the project, so "" is an ambiguous prefix
    // of every one of them.

    let assert_refused = |args: &[&str]| {
        let before = std::fs::read(&project).unwrap();
        let output = run(&dir, args);
        assert!(!output.success, "expected refusal for {args:?}");
        assert!(output.stdout.is_empty(), "stdout: {}", output.stdout);
        assert_eq!(std::fs::read(&project).unwrap(), before);
    };

    // Unknown id.
    assert_refused(&[
        "history",
        "materialize",
        project.to_str().unwrap(),
        "--checkpoint",
        "not-a-real-id",
        "-o",
        dir.join("nope.tif").to_str().unwrap(),
    ]);
    // Ambiguous prefix.
    assert_refused(&[
        "history",
        "materialize",
        project.to_str().unwrap(),
        "--checkpoint",
        "",
        "-o",
        dir.join("nope.tif").to_str().unwrap(),
    ]);
    assert_refused(&[
        "history",
        "apply",
        project.to_str().unwrap(),
        "--ops",
        ops.to_str().unwrap(),
        "--from",
        "",
        "--save",
    ]);
    assert_refused(&[
        "history",
        "delete",
        project.to_str().unwrap(),
        "--checkpoint",
        "",
    ]);

    // A unique prefix is accepted.
    let output = run(
        &dir,
        &[
            "history",
            "materialize",
            project.to_str().unwrap(),
            "--checkpoint",
            &root[..8],
            "-o",
            dir.join("root.tif").to_str().unwrap(),
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
}

#[test]
fn invalid_op_file_is_refused_before_writing() {
    let (dir, project) = fresh_project("invalid_op_file_is_refused_before_writing");

    let bad_source_id = op_file(
        &serde_json::to_value(vec![Op::BrushStroke {
            source_id: 8,
            path: vec![Point { x: 1, y: 1 }],
            radius: 1.0,
            softness: 0.0,
        }])
        .unwrap(),
    );
    let bad_rect = op_file(
        &serde_json::to_value(vec![Op::ResetRegion {
            rect: Rect {
                x: 100_000,
                y: 100_000,
                width: 10,
                height: 10,
            },
        }])
        .unwrap(),
    );
    let bare_array = serde_json::to_value(vec![Op::ResetRegion {
        rect: Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        },
    }])
    .unwrap();
    let bad_format = serde_json::json!({"format": 2, "ops": []});

    for (name, body) in [
        ("bad-source-id.json", bad_source_id),
        ("bad-rect.json", bad_rect),
        ("bare-array.json", bare_array),
        ("bad-format.json", bad_format),
    ] {
        let ops_path = write_op_file(&dir, name, &body);
        let out = dir.join(format!("{name}.out.tif"));
        let partial = dir.join(format!("{name}.out.tif.partial"));
        let before = std::fs::read(&project).unwrap();

        let output = run(
            &dir,
            &[
                "history",
                "apply",
                project.to_str().unwrap(),
                "--ops",
                ops_path.to_str().unwrap(),
                "--save",
                "-o",
                out.to_str().unwrap(),
            ],
        );
        assert!(!output.success, "{name} must be refused");
        assert!(
            output.stderr.contains("error:"),
            "{name} stderr: {}",
            output.stderr
        );
        assert_eq!(std::fs::read(&project).unwrap(), before, "{name}");
        assert!(!out.exists(), "{name} left {out:?}");
        assert!(!partial.exists(), "{name} left {partial:?}");
        let staging = project.with_extension("ffp.ops-staging");
        assert!(!staging.exists(), "{name} left {staging:?}");
    }
}

#[test]
fn records_on_stdout_progress_on_stderr() {
    let (dir, project) = fresh_project("records_on_stdout_progress_on_stderr");
    let root = root_id(&project);

    let assert_one_document = |args: &[&str]| {
        let output = run(&dir, args);
        assert!(output.success, "{args:?} stderr: {}", output.stderr);
        let mut de = serde_json::Deserializer::from_str(&output.stdout).into_iter::<Value>();
        assert!(de.next().is_some(), "{args:?} stdout empty");
        assert!(de.next().is_none(), "{args:?} stdout: {}", output.stdout);
    };

    assert_one_document(&["history", "list", project.to_str().unwrap(), "--json"]);
    assert_one_document(&[
        "history",
        "materialize",
        project.to_str().unwrap(),
        "--checkpoint",
        &root,
        "-o",
        dir.join("root.tif").to_str().unwrap(),
        "--json",
    ]);

    let ops = write_op_file(
        &dir,
        "ops.json",
        &op_file(&serde_json::to_value(three_strokes()).unwrap()),
    );
    assert_one_document(&[
        "history",
        "apply",
        project.to_str().unwrap(),
        "--ops",
        ops.to_str().unwrap(),
        "--save",
        "--json",
    ]);
    assert_one_document(&["history", "list", project.to_str().unwrap(), "--json"]);

    // A refusal leaves stdout empty and names the problem on stderr.
    let output = run(
        &dir,
        &[
            "history",
            "materialize",
            project.to_str().unwrap(),
            "--checkpoint",
            "not-a-real-id",
            "-o",
            dir.join("nope.tif").to_str().unwrap(),
            "--json",
        ],
    );
    assert!(!output.success);
    assert!(output.stdout.is_empty(), "stdout: {}", output.stdout);
    assert!(
        output.stderr.contains("error:"),
        "stderr: {}",
        output.stderr
    );
}

#[test]
fn output_does_not_reveal_storage_kind() {
    // `fresh_project`'s own directory name lands in every report's
    // `project` field, so it must avoid every forbidden needle itself.
    let (dir, project) = fresh_project("no_storage_metadata_leak");
    let root = root_id(&project);

    // One small edit next to the root's own full map: a sparse delta is the
    // cheaper way to store it.
    let ops = write_op_file(
        &dir,
        "ops.json",
        &op_file(
            &serde_json::to_value(vec![Op::BrushStroke {
                source_id: 1,
                path: vec![Point { x: 5, y: 5 }],
                radius: 1.0,
                softness: 0.0,
            }])
            .unwrap(),
        ),
    );
    let output = run(
        &dir,
        &[
            "history",
            "apply",
            project.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "--save",
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let child: Value = serde_json::from_str(&output.stdout).unwrap();
    let child_id = child["checkpoint"].as_str().unwrap().to_string();

    let mut combined = String::new();
    for args in [
        vec!["history", "list", project.to_str().unwrap(), "--json"],
        vec![
            "history",
            "materialize",
            project.to_str().unwrap(),
            "--checkpoint",
            &child_id,
            "-o",
            dir.join("child.tif").to_str().unwrap(),
            "--json",
        ],
        vec![
            "history",
            "delete",
            project.to_str().unwrap(),
            "--checkpoint",
            &child_id,
            "--json",
        ],
    ] {
        let output = run(&dir, &args);
        assert!(output.success, "{args:?} stderr: {}", output.stderr);
        combined.push_str(&output.stdout);
        combined.push_str(&output.stderr);
    }
    let _ = root;
    for needle in ["kind", "delta", "snapshot", "sparse", "full"] {
        assert!(
            !combined.to_lowercase().contains(needle),
            "output reveals storage kind via {needle:?}: {combined}"
        );
    }
}

#[test]
fn help_lists_every_command() {
    let dir = common::tmp_dir("help_lists_every_command");

    let output = run(&dir, &["history", "--help"]);
    assert!(output.success);
    for name in ["list", "materialize", "apply", "delete"] {
        assert!(
            output.stdout.contains(name),
            "history --help missing {name}: {}",
            output.stdout
        );
    }

    let subcommand_flags: [(&str, &[&str]); 4] = [
        ("list", &["--json"]),
        ("materialize", &["--checkpoint", "--output", "--json"]),
        (
            "apply",
            &["--ops", "--from", "--save", "--output", "--json"],
        ),
        ("delete", &["--checkpoint", "--with-descendants", "--json"]),
    ];
    for (sub, flags) in subcommand_flags {
        let output = run(&dir, &["history", sub, "--help"]);
        assert!(output.success, "history {sub} --help failed");
        for flag in flags {
            assert!(
                output.stdout.contains(flag),
                "history {sub} --help missing {flag}: {}",
                output.stdout
            );
        }
    }
}

#[test]
fn output_naming_the_project_is_refused() {
    let (dir, project) = fresh_project("output_naming_the_project_is_refused");
    let root = root_id(&project);
    let before = std::fs::read(&project).unwrap();

    let output = run(
        &dir,
        &[
            "history",
            "materialize",
            project.to_str().unwrap(),
            "--checkpoint",
            &root,
            "-o",
            project.to_str().unwrap(),
        ],
    );
    assert!(!output.success);
    assert_eq!(std::fs::read(&project).unwrap(), before);

    let ops = write_op_file(
        &dir,
        "ops.json",
        &op_file(&serde_json::to_value(three_strokes()).unwrap()),
    );
    let output = run(
        &dir,
        &[
            "history",
            "apply",
            project.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "-o",
            project.to_str().unwrap(),
        ],
    );
    assert!(!output.success);
    assert_eq!(std::fs::read(&project).unwrap(), before);
}

/// One small project sealed directly under `dir`, its three source frames
/// with it -- `Relative` sources, so a test can edit one on disk. `seed`
/// varies the composite, Z-Map and source content without changing their
/// declared shape.
fn seal_synthetic_project(dir: &Path, seed: u16) -> PathBuf {
    let full = |w: u32, h: u32| Rect {
        x: 0,
        y: 0,
        width: w,
        height: h,
    };
    let (w, h, spp, frames) = (16u32, 12u32, 3u32, 3usize);

    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let composite_path = staging.join("composite.tif");
    let mut writer = TiffWriter::create(
        &composite_path,
        TiffSpec {
            width: w,
            height: h,
            samples_per_pixel: spp,
            description: Some("staged composite".to_string()),
        },
    )
    .unwrap();
    let composite_samples: Vec<u16> = (0..(w * h * spp) as usize)
        .map(|i| ((i as u16).wrapping_add(seed)) % 65_535)
        .collect();
    writer.write_tile(full(w, h), &composite_samples).unwrap();
    writer.finish().unwrap();

    let zmap_path = staging.join("zmap.tif");
    let mut map =
        ZMapWriter::create(&zmap_path, ZMapInfo::new(w, h, frames as u32).unwrap()).unwrap();
    let zmap_samples: Vec<u16> = (0..(w * h) as usize)
        .map(|i| ((i as u16).wrapping_add(seed) as usize % frames) as u16)
        .collect();
    map.write_tile(full(w, h), &zmap_samples).unwrap();
    map.finish().unwrap();

    let mut sources = Vec::new();
    for i in 0..frames {
        let path = dir.join(format!("frame-{i:02}.tif"));
        let mut source_writer = TiffWriter::create(
            &path,
            TiffSpec {
                width: w,
                height: h,
                samples_per_pixel: spp,
                description: None,
            },
        )
        .unwrap();
        let samples: Vec<u16> = (0..(w * h * spp) as usize)
            .map(|j| ((j as u16).wrapping_add(seed).wrapping_add(i as u16 * 17)) % 65_535)
            .collect();
        source_writer.write_tile(full(w, h), &samples).unwrap();
        source_writer.finish().unwrap();
        sources.push(path);
    }

    let mut values = ParamValues::new();
    values.insert("levels".to_string(), ParamValue::Int(5));

    let project_path = dir.join(format!("synthetic-{seed}.ffp"));
    project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite_path,
        staged_zmap: &zmap_path,
        sources: &sources,
        algorithm: "laplacian",
        params: &values,
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");
    project_path
}

fn read_entry_bytes(path: &Path, name: &str) -> Vec<u8> {
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut bytes = Vec::new();
    archive
        .by_name(name)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

fn read_manifest_value(path: &Path) -> Value {
    serde_json::from_slice(&read_entry_bytes(path, "manifest.json")).unwrap()
}

fn read_checkpoint_json(path: &Path, id: &str) -> Value {
    serde_json::from_slice(&read_entry_bytes(path, &format!("checkpoints/{id}.json"))).unwrap()
}

/// Copies `src` to `dst`, dropping the entries named in `drop`, replacing
/// the named entries in `replace` and appending the entries in `add` -- a
/// raw rewrite of a real container, so a test can inject a malformed link,
/// a mismatched blob or a missing entry the production writer would never
/// produce.
fn mutate_container(
    src: &Path,
    dst: &Path,
    drop: &[&str],
    replace: &[(String, Vec<u8>)],
    add: &[(String, Vec<u8>)],
) {
    use std::io::{Read, Write};
    let mut archive = zip::ZipArchive::new(std::fs::File::open(src).unwrap()).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    let mut writer = zip::write::ZipWriter::new(std::fs::File::create(dst).unwrap());
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for name in &names {
        if drop.contains(&name.as_str()) {
            continue;
        }
        let mut bytes = Vec::new();
        archive
            .by_name(name)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        if let Some(entry) = replace.iter().find(|entry| &entry.0 == name) {
            bytes = entry.1.clone();
        }
        writer.start_file(name.as_str(), options).unwrap();
        writer.write_all(&bytes).unwrap();
    }
    for (name, bytes) in add {
        writer.start_file(name.as_str(), options).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap();
}

/// Every named refusal exits with its own code, a changed source refuses at
/// exit 9 naming the flag that accepts it, the flag turns it into exit 0
/// plus a warning, unresolved sources warn at exit 0, and stdout stays
/// empty on every refusal.
#[test]
fn validation_states_are_reported() {
    let dir = common::tmp_dir("validation_states_are_reported");
    let project_path = seal_synthetic_project(&dir, 0);

    // Exit 3: not a FocusForge project at all.
    let foreign = dir.join("foreign.ffp");
    std::fs::write(&foreign, b"not a zip, not a project").unwrap();
    let output = run(&dir, &["history", "list", foreign.to_str().unwrap()]);
    assert_eq!(output.exit_code, Some(3));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.contains("is not a FocusForge project"));

    // Exit 4: truncated.
    let bytes = std::fs::read(&project_path).unwrap();
    let truncated = dir.join("truncated.ffp");
    std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
    let output = run(&dir, &["history", "list", truncated.to_str().unwrap()]);
    assert_eq!(output.exit_code, Some(4));
    assert!(output.stdout.is_empty());

    // Exit 5: a format_version newer than this build understands.
    let mut manifest = read_manifest_value(&project_path);
    manifest["format_version"] = serde_json::json!(9999);
    let future = dir.join("future.ffp");
    mutate_container(
        &project_path,
        &future,
        &[],
        &[(
            "manifest.json".to_string(),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )],
        &[],
    );
    let output = run(&dir, &["history", "list", future.to_str().unwrap()]);
    assert_eq!(output.exit_code, Some(5));
    assert!(output.stdout.is_empty());

    // Exit 6: a blob a record names, absent from the container.
    let head_hash = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .zmap_blob
        .clone();
    let missing_entry = dir.join("missing-entry.ffp");
    let missing_blob = format!("blobs/{head_hash}.zst");
    mutate_container(
        &project_path,
        &missing_entry,
        &[missing_blob.as_str()],
        &[],
        &[],
    );
    let output = run(&dir, &["history", "list", missing_entry.to_str().unwrap()]);
    assert_eq!(output.exit_code, Some(6));
    assert!(output.stdout.is_empty());

    // Exit 7: a blob whose bytes no longer hash to its entry name -- a real
    // zstd stream, from an independent project of the same map shape, so
    // only the hash check (never a truncated-frame decode error) can catch
    // it.
    let other_dir = common::tmp_dir("validation_states_are_reported_other");
    let other_project = seal_synthetic_project(&other_dir, 1);
    let other_hash = Project::open(&other_project)
        .unwrap()
        .checkpoint()
        .zmap_blob
        .clone();
    let swapped_bytes = read_entry_bytes(&other_project, &format!("blobs/{other_hash}.zst"));
    let tampered = dir.join("tampered.ffp");
    mutate_container(
        &project_path,
        &tampered,
        &[],
        &[(format!("blobs/{head_hash}.zst"), swapped_bytes)],
        &[],
    );
    let output = run(&dir, &["history", "list", tampered.to_str().unwrap()]);
    assert_eq!(output.exit_code, Some(7));
    assert!(output.stdout.is_empty());

    // Exit 8: a structural link failure -- a dangling parent.
    let root_id = root_id(&project_path);
    let mut orphan = read_checkpoint_json(&project_path, &root_id);
    orphan["id"] = serde_json::json!("orphan");
    orphan["parent"] = serde_json::json!("ghost");
    let dangling = dir.join("dangling.ffp");
    mutate_container(
        &project_path,
        &dangling,
        &[],
        &[],
        &[(
            "checkpoints/orphan.json".to_string(),
            serde_json::to_vec_pretty(&orphan).unwrap(),
        )],
    );
    let output = run(&dir, &["history", "list", dangling.to_str().unwrap()]);
    assert_eq!(output.exit_code, Some(8));
    assert!(output.stdout.is_empty());

    // Exit 8: also an op-bearing checkpoint with no parent at all -- its
    // own project, so the head sealed here never disturbs `project_path`.
    let ops_dir = common::tmp_dir("validation_states_are_reported_ops");
    let ops_project = seal_synthetic_project(&ops_dir, 2);
    let ops = write_op_file(
        &ops_dir,
        "orphan-ops.json",
        &op_file(
            &serde_json::to_value(vec![Op::BrushStroke {
                source_id: 1,
                path: vec![Point { x: 2, y: 2 }],
                radius: 1.0,
                softness: 0.0,
            }])
            .unwrap(),
        ),
    );
    let output = run(
        &ops_dir,
        &[
            "history",
            "apply",
            ops_project.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "--save",
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let child: Value = serde_json::from_str(&output.stdout).unwrap();
    let child_id = child["checkpoint"].as_str().unwrap().to_string();
    let mut orphan_ops = read_checkpoint_json(&ops_project, &child_id);
    orphan_ops["parent"] = Value::Null;
    let ops_no_parent = dir.join("ops-no-parent.ffp");
    mutate_container(
        &ops_project,
        &ops_no_parent,
        &[],
        &[(
            format!("checkpoints/{child_id}.json"),
            serde_json::to_vec_pretty(&orphan_ops).unwrap(),
        )],
        &[],
    );
    let output = run(&dir, &["history", "list", ops_no_parent.to_str().unwrap()]);
    assert_eq!(output.exit_code, Some(8));
    assert!(output.stdout.is_empty());

    // Exit 9: a changed source, refused by default, naming the flag; the
    // flag turns it into exit 0 plus its own warning.
    let manifest = Project::open(&project_path).unwrap().manifest().clone();
    let source_path = dir.join(&manifest.sources[0].path);
    let mut frame_bytes = std::fs::read(&source_path).unwrap();
    frame_bytes[0] ^= 0xFF;
    std::fs::write(&source_path, &frame_bytes).unwrap();

    let output = run(&dir, &["history", "list", project_path.to_str().unwrap()]);
    assert_eq!(output.exit_code, Some(9));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.contains("--accept-changed-sources"));

    let output = run(
        &dir,
        &[
            "history",
            "--accept-changed-sources",
            "list",
            project_path.to_str().unwrap(),
        ],
    );
    assert!(output.success);
    assert!(!output.stdout.is_empty());
    assert!(output.stderr.contains("warning:"));

    // Unresolved sources: exit 0 with a counted warning, never a refusal.
    // `shared_stack`'s project records its frames `AsGiven` (what an
    // `-o <dir>` run produces), so it always carries this warning.
    let unresolved_project = &shared_stack().project;
    let output = run(
        &dir,
        &["history", "list", unresolved_project.to_str().unwrap()],
    );
    assert!(output.success);
    assert!(!output.stdout.is_empty());
    assert!(output.stderr.contains("warning:"));
    assert!(output.stderr.contains("unresolved"));
}
