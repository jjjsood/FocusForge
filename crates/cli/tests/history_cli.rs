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

use focusforge_core::project::{Checkpoint, Op, Point, Project};
use focusforge_core::tile::Rect;
use serde_json::Value;

fn bin() -> Command {
    common::cli_bin("history_cli")
}

struct CliOutput {
    success: bool,
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
