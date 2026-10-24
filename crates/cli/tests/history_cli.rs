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
use focusforge_core::project::container::{ContainerWriter, Index};
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
                "--algorithm",
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

/// A brush stroke well inside a 512x512, 8-frame map -- [`shared_stack`]'s
/// own geometry.
fn one_stroke() -> Vec<Op> {
    vec![Op::BrushStroke {
        source_id: 1,
        path: vec![Point { x: 20, y: 20 }, Point { x: 24, y: 24 }],
        radius: 3.0,
        softness: 0.0,
    }]
}

#[test]
fn list_prints_one_record_per_checkpoint() {
    let (dir, project) = fresh_project("list_prints_one_record_per_checkpoint");
    let root = root_id(&project);

    let ops = write_op_file(
        &dir,
        "ops.json",
        &serde_json::to_value(one_stroke()).unwrap(),
    );
    let output = run(
        &dir,
        &[
            "retouch",
            project.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let child: Value = serde_json::from_str(&output.stdout).unwrap();
    let child_id = child["checkpoints"][0].as_str().unwrap().to_string();

    let output = run(
        &dir,
        &["history", "list", project.to_str().unwrap(), "--json"],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let doc: Value = serde_json::from_str(&output.stdout).expect("valid JSON");
    assert!(doc.get("format").is_none());
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
fn exported_zmap_matches_core_replay() {
    let (dir, project) = fresh_project("exported_zmap_matches_core_replay");
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
        &serde_json::to_value(one_stroke()).unwrap(),
    );
    let output = run(
        &dir,
        &[
            "retouch",
            project.to_str().unwrap(),
            "--ops",
            ops_a.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let child_a: Value = serde_json::from_str(&output.stdout).unwrap();
    let child_a_id = child_a["checkpoints"][0].as_str().unwrap().to_string();

    // A second seal makes `child_a` non-head.
    let ops_b = write_op_file(
        &dir,
        "ops-b.json",
        &serde_json::to_value(vec![Op::BrushStroke {
            source_id: 3,
            path: vec![Point { x: 200, y: 200 }],
            radius: 2.0,
            softness: 0.0,
        }])
        .unwrap(),
    );
    let output = run(
        &dir,
        &[
            "retouch",
            project.to_str().unwrap(),
            "--ops",
            ops_b.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);

    // Export the non-head, op-bearing checkpoint's map through the CLI, and
    // compare against `core`'s own `materialize_zmap` of the same
    // checkpoint.
    let cli_out = dir.join("export-child-a.tif");
    let output = run(
        &dir,
        &[
            "export",
            project.to_str().unwrap(),
            "--checkpoint",
            &child_a_id,
            "--zmap",
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
    let root_out = dir.join("export-root.tif");
    let output = run(
        &dir,
        &[
            "export",
            project.to_str().unwrap(),
            "--checkpoint",
            &root,
            "--zmap",
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
fn sealing_from_an_older_checkpoint_branches() {
    let (dir, project) = fresh_project("sealing_from_an_older_checkpoint_branches");
    let root = root_id(&project);

    // Two distinct op lists: sealing the same ops onto the same parent in
    // the same wall-clock second would derive the same checkpoint id.
    let ops_a = write_op_file(
        &dir,
        "ops-a.json",
        &serde_json::to_value(one_stroke()).unwrap(),
    );
    let ops_b = write_op_file(
        &dir,
        "ops-b.json",
        &serde_json::to_value(vec![Op::BrushStroke {
            source_id: 4,
            path: vec![Point { x: 300, y: 300 }],
            radius: 2.0,
            softness: 0.0,
        }])
        .unwrap(),
    );

    // Seal onto the head (the root): the head advances.
    let output = run(
        &dir,
        &[
            "retouch",
            project.to_str().unwrap(),
            "--ops",
            ops_a.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let child_a: Value = serde_json::from_str(&output.stdout).unwrap();
    let child_a_id = child_a["checkpoints"][0].as_str().unwrap().to_string();

    // Seal a second time, explicitly `--from` the root: a branch, not a
    // third generation.
    let output = run(
        &dir,
        &[
            "retouch",
            project.to_str().unwrap(),
            "--ops",
            ops_b.to_str().unwrap(),
            "--from",
            &root,
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let child_b: Value = serde_json::from_str(&output.stdout).unwrap();
    let child_b_id = child_b["checkpoints"][0].as_str().unwrap().to_string();
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

/// Appends `ops` through `retouch` under the project's head (or `from`),
/// returning the new states' ids in order.
fn retouch(dir: &Path, project: &Path, ops: &[Op], from: Option<&str>) -> Vec<String> {
    let ops = write_op_file(dir, "ops.json", &serde_json::to_value(ops).unwrap());
    let mut args = vec![
        "retouch",
        project.to_str().unwrap(),
        "--ops",
        ops.to_str().unwrap(),
    ];
    if let Some(from) = from {
        args.push("--from");
        args.push(from);
    }
    args.push("--json");
    let output = run(dir, &args);
    assert!(output.success, "stderr: {}", output.stderr);
    let report: Value = serde_json::from_str(&output.stdout).unwrap();
    report["checkpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_string())
        .collect()
}

fn listed(dir: &Path, project: &Path) -> Vec<Value> {
    let output = run(
        dir,
        &["history", "list", project.to_str().unwrap(), "--json"],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let doc: Value = serde_json::from_str(&output.stdout).unwrap();
    doc["checkpoints"].as_array().unwrap().clone()
}

#[test]
fn delete_hides_one_checkpoint_like_core() {
    let (dir, project) = fresh_project("delete_hides_one_checkpoint_like_core");
    let root = root_id(&project);
    let middle = retouch(&dir, &project, &one_stroke(), None).remove(0);
    let tip = retouch(&dir, &project, &one_stroke(), None).remove(0);
    assert_eq!(
        read_checkpoint(&project, &tip).parent.as_deref(),
        Some(middle.as_str())
    );

    let twin = dir.join("twin");
    std::fs::create_dir_all(&twin).unwrap();
    let twin_project = twin.join(project.file_name().unwrap());
    std::fs::copy(&project, &twin_project).unwrap();

    let before_map = dir.join("tip-before.tif");
    let before_export = dir.join("tip-before-export.tif");
    let project_arg = project.to_str().unwrap();
    for (flag, path) in [("--zmap", &before_map), ("-o", &before_export)] {
        let args = [
            "export",
            project_arg,
            "--checkpoint",
            &tip,
            flag,
            path.to_str().unwrap(),
        ];
        let output = run(&dir, &args);
        assert!(output.success, "{args:?} stderr: {}", output.stderr);
    }

    let output = run(
        &dir,
        &[
            "history",
            "delete",
            project.to_str().unwrap(),
            "--checkpoint",
            &middle,
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let report: Value = serde_json::from_str(&output.stdout).unwrap();
    assert_eq!(report["hidden"], middle);
    assert!(report["removed"].as_array().unwrap().is_empty());
    assert_eq!(report["head"], tip);

    project::delete_checkpoint(&twin_project, &middle).unwrap();
    assert_eq!(
        std::fs::read(&project).unwrap(),
        std::fs::read(&twin_project).unwrap(),
        "the CLI and the core call write the same project"
    );

    // The list omits the hidden one and shows the nearest visible ancestor.
    let records = listed(&dir, &project);
    let ids: Vec<&str> = records.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(ids, [tip.as_str(), root.as_str()]);
    assert_eq!(records[0]["parent"], root);
    assert!(records[1]["parent"].is_null());
    let output = run(&dir, &["history", "list", project.to_str().unwrap()]);
    assert!(output.success, "stderr: {}", output.stderr);
    assert!(!output.stdout.contains(&middle), "{}", output.stdout);
    assert!(output.stdout.contains(&format!("{tip} parent={root}")));

    // The tip still renders the same.
    let after_map = dir.join("tip-after.tif");
    let after_export = dir.join("tip-after-export.tif");
    let output = run(
        &dir,
        &[
            "export",
            project.to_str().unwrap(),
            "--checkpoint",
            &tip,
            "--zmap",
            after_map.to_str().unwrap(),
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    assert_eq!(
        std::fs::read(&before_map).unwrap(),
        std::fs::read(&after_map).unwrap()
    );
    let output = run(
        &dir,
        &[
            "export",
            project.to_str().unwrap(),
            "--checkpoint",
            &tip,
            "-o",
            after_export.to_str().unwrap(),
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    assert_eq!(
        std::fs::read(&before_export).unwrap(),
        std::fs::read(&after_export).unwrap()
    );

    // A hidden id, whole or as a prefix only it matches, resolves to nothing.
    let nowhere = dir.join("hidden.tif");
    let prefix = &middle[..middle.len() - 1];
    assert!(
        !ids.iter().any(|id| id.starts_with(prefix)),
        "the prefix names the hidden checkpoint alone"
    );
    let before = std::fs::read(&project).unwrap();
    for reference in [middle.as_str(), prefix] {
        for flag in ["--zmap", "-o"] {
            let args = [
                "export",
                project.to_str().unwrap(),
                "--checkpoint",
                reference,
                flag,
                nowhere.to_str().unwrap(),
            ];
            let output = run(&dir, &args);
            assert!(!output.success, "{args:?}");
            assert!(
                output.stderr.contains("no checkpoint named"),
                "{}",
                output.stderr
            );
            assert!(!nowhere.exists());
        }
        let output = run(
            &dir,
            &[
                "history",
                "delete",
                project.to_str().unwrap(),
                "--checkpoint",
                reference,
            ],
        );
        assert!(!output.success);
    }
    assert_eq!(std::fs::read(&project).unwrap(), before);
}

#[test]
fn delete_reports_what_it_removed_in_text() {
    let (dir, project) = fresh_project("delete_reports_what_it_removed_in_text");
    let root = root_id(&project);
    let leaf = retouch(&dir, &project, &one_stroke(), None).remove(0);
    let output = run(
        &dir,
        &[
            "history",
            "delete",
            project.to_str().unwrap(),
            "--checkpoint",
            &leaf,
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    assert!(output.stdout.contains(&format!("hidden: {leaf}")));
    assert!(output.stdout.contains(&format!("removed: {leaf}")));
    assert!(output.stdout.contains(&format!("head: {root}")));
}

#[test]
fn delete_refuses_the_last_visible_checkpoint() {
    let (dir, project) = fresh_project("delete_refuses_the_last_visible_checkpoint");
    let root = root_id(&project);
    let before = std::fs::read(&project).unwrap();

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
    assert!(
        output.stderr.contains(&root) && output.stderr.contains("last visible"),
        "stderr: {}",
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
            &root,
            "--with-descendants",
        ],
    );
    assert!(!output.success);
    assert!(
        output.stderr.contains("--with-descendants"),
        "{}",
        output.stderr
    );
    assert_eq!(std::fs::read(&project).unwrap(), before);
}

#[test]
fn unknown_checkpoint_id_is_refused() {
    let (dir, project) = fresh_project("unknown_checkpoint_id_is_refused");
    let root = root_id(&project);
    let ops = write_op_file(
        &dir,
        "ops.json",
        &serde_json::to_value(one_stroke()).unwrap(),
    );
    let output = run(
        &dir,
        &[
            "retouch",
            project.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
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
        "export",
        project.to_str().unwrap(),
        "--checkpoint",
        "not-a-real-id",
        "--zmap",
        dir.join("nope.tif").to_str().unwrap(),
    ]);
    // Ambiguous prefix.
    assert_refused(&[
        "export",
        project.to_str().unwrap(),
        "--checkpoint",
        "",
        "--zmap",
        dir.join("nope.tif").to_str().unwrap(),
    ]);
    assert_refused(&[
        "retouch",
        project.to_str().unwrap(),
        "--ops",
        ops.to_str().unwrap(),
        "--from",
        "",
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
            "export",
            project.to_str().unwrap(),
            "--checkpoint",
            &root[..8],
            "--zmap",
            dir.join("root.tif").to_str().unwrap(),
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
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
        "export",
        project.to_str().unwrap(),
        "--checkpoint",
        &root,
        "--zmap",
        dir.join("root.tif").to_str().unwrap(),
        "--json",
    ]);

    let ops = write_op_file(
        &dir,
        "ops.json",
        &serde_json::to_value(one_stroke()).unwrap(),
    );
    assert_one_document(&[
        "retouch",
        project.to_str().unwrap(),
        "--ops",
        ops.to_str().unwrap(),
        "--json",
    ]);
    assert_one_document(&["history", "list", project.to_str().unwrap(), "--json"]);

    // A refusal leaves stdout empty and names the problem on stderr.
    let output = run(
        &dir,
        &[
            "export",
            project.to_str().unwrap(),
            "--checkpoint",
            "not-a-real-id",
            "--zmap",
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
        &serde_json::to_value(vec![Op::BrushStroke {
            source_id: 1,
            path: vec![Point { x: 5, y: 5 }],
            radius: 1.0,
            softness: 0.0,
        }])
        .unwrap(),
    );
    let output = run(
        &dir,
        &[
            "retouch",
            project.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let child: Value = serde_json::from_str(&output.stdout).unwrap();
    let child_id = child["checkpoints"][0].as_str().unwrap().to_string();

    let mut combined = String::new();
    for args in [
        vec!["history", "list", project.to_str().unwrap(), "--json"],
        vec![
            "export",
            project.to_str().unwrap(),
            "--checkpoint",
            &child_id,
            "--zmap",
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

/// The live records of `path`, each with its bytes, in name order.
fn live_records(path: &Path) -> Vec<(String, Vec<u8>)> {
    let index = Index::read(path).unwrap();
    index
        .names()
        .map(|name| (name.to_string(), index.read_entry(path, name).unwrap()))
        .collect()
}

/// Appends `len` dead bytes to the project through the container.
fn append_dead_bytes(path: &Path, len: u64) {
    use std::io::Read;
    let index = Index::read(path).unwrap();
    let mut writer = ContainerWriter::append(path, &index).unwrap();
    writer
        .put("filler.bin", &mut std::io::repeat(0).take(len))
        .unwrap();
    writer.drop_entry("filler.bin");
    writer.finish().unwrap();
}

#[test]
fn compact_keeps_visible_states() {
    let (dir, project) = fresh_project("compact_keeps_visible_states");
    let root = root_id(&project);
    let first = retouch(&dir, &project, &one_stroke(), None).remove(0);
    let tip = retouch(&dir, &project, &one_stroke(), None).remove(0);
    let project_arg = project.to_str().unwrap();
    let output = run(
        &dir,
        &["history", "delete", project_arg, "--checkpoint", &tip],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    assert!(
        !output.stderr.contains("compacted"),
        "the delete left the file under the threshold: {}",
        output.stderr
    );

    let visible = listed(&dir, &project);
    let ids: Vec<&str> = visible.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&first.as_str()) && ids.contains(&root.as_str()));
    assert!(!ids.contains(&tip.as_str()));
    let records_before = live_records(&project);
    let old_len = std::fs::metadata(&project).unwrap().len();
    assert!(Index::read(&project).unwrap().dead_bytes() > 0);

    let output = run(&dir, &["history", "compact", project_arg, "--json"]);
    assert!(output.success, "stderr: {}", output.stderr);
    let report: Value = serde_json::from_str(&output.stdout).unwrap();
    assert_eq!(report["project"], project_arg);
    let new_len = std::fs::metadata(&project).unwrap().len();
    assert!(new_len < old_len);
    assert_eq!(report["reclaimed_bytes"], old_len - new_len);

    assert_eq!(listed(&dir, &project), visible);
    assert_eq!(live_records(&project), records_before);
    assert_eq!(Index::read(&project).unwrap().dead_bytes(), 0);
    assert_eq!(Project::open(&project).unwrap().checkpoint().id, first);
    assert!(
        Index::read(&project)
            .unwrap()
            .get(&format!("checkpoints/{tip}.json"))
            .is_none()
    );
    assert!(!project::partial_path(&project).exists());

    let compacted = std::fs::read(&project).unwrap();
    let output = run(&dir, &["history", "compact", project_arg]);
    assert!(output.success, "stderr: {}", output.stderr);
    assert!(
        output.stdout.contains("reclaimed: 0 bytes"),
        "{}",
        output.stdout
    );
    assert_eq!(std::fs::read(&project).unwrap(), compacted);

    let not_a_project = dir.join("notes.ffp");
    std::fs::write(&not_a_project, b"plain text, not a project").unwrap();
    let output = run(
        &dir,
        &["history", "compact", not_a_project.to_str().unwrap()],
    );
    assert_eq!(output.exit_code, Some(3), "stderr: {}", output.stderr);
}

#[test]
fn save_commands_compact_mostly_dead_files() {
    const FILLER: u64 = 16 << 20;
    let (dir, project) = fresh_project("save_commands_compact_mostly_dead_files");
    let root = root_id(&project);
    let project_arg = project.to_str().unwrap();

    // A save onto a file under the threshold does not compact.
    let kept = retouch(&dir, &project, &one_stroke(), None).remove(0);
    let run_save = |dir: &Path| {
        let ops = write_op_file(
            dir,
            "ops-again.json",
            &serde_json::to_value(one_stroke()).unwrap(),
        );
        run(
            dir,
            &[
                "retouch",
                project_arg,
                "--ops",
                ops.to_str().unwrap(),
                "--json",
            ],
        )
    };
    let output = run_save(&dir);
    assert!(output.success, "stderr: {}", output.stderr);
    assert!(!output.stderr.contains("compacted"), "{}", output.stderr);

    // A save onto a mostly dead file compacts it, after the save committed.
    append_dead_bytes(&project, FILLER);
    let output = run_save(&dir);
    assert!(output.success, "stderr: {}", output.stderr);
    assert!(output.stderr.contains("compacted"), "{}", output.stderr);
    let report: Value = serde_json::from_str(&output.stdout).unwrap();
    assert!(report["checkpoints"][0].is_string());
    assert!(std::fs::metadata(&project).unwrap().len() < FILLER);
    assert_eq!(Index::read(&project).unwrap().dead_bytes(), 0);

    // A delete does the same and prints the same report as before.
    let tip = report["checkpoints"][0].as_str().unwrap().to_string();
    append_dead_bytes(&project, FILLER);
    let output = run(
        &dir,
        &[
            "history",
            "delete",
            project_arg,
            "--checkpoint",
            &tip,
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    assert!(output.stderr.contains("compacted"), "{}", output.stderr);
    let report: Value = serde_json::from_str(&output.stdout).unwrap();
    assert_eq!(report["hidden"], tip);
    assert_eq!(report["removed"], serde_json::json!([tip]));
    assert!(std::fs::metadata(&project).unwrap().len() < FILLER);
    assert_eq!(Index::read(&project).unwrap().dead_bytes(), 0);
    let ids: Vec<String> = listed(&dir, &project)
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&root) && ids.contains(&kept) && !ids.contains(&tip));
}

#[test]
fn help_lists_every_command() {
    let dir = common::tmp_dir("help_lists_every_command");

    let output = run(&dir, &["history", "--help"]);
    assert!(output.success);
    for name in ["list", "delete", "compact"] {
        assert!(
            output.stdout.contains(name),
            "history --help missing {name}: {}",
            output.stdout
        );
    }

    let subcommand_flags: [(&str, &[&str]); 3] = [
        ("list", &["--json"]),
        ("delete", &["--checkpoint", "--json"]),
        ("compact", &["--json"]),
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
fn history_materialize_is_gone() {
    let (dir, project) = fresh_project("history_materialize_is_gone");
    let out = dir.join("zmap.tif");
    let output = run(
        &dir,
        &[
            "history",
            "materialize",
            project.to_str().unwrap(),
            "--checkpoint",
            "none",
            "--output",
            out.to_str().unwrap(),
        ],
    );
    assert!(!output.success);
    assert!(
        output.stderr.contains("unrecognized subcommand"),
        "stderr: {}",
        output.stderr
    );
    assert!(!out.exists());
    let help = run(&dir, &["history", "--help"]);
    assert!(!help.stdout.contains("materialize"), "{}", help.stdout);
}

#[test]
fn history_apply_is_gone() {
    let (dir, project) = fresh_project("history_apply_is_gone");
    let ops = write_op_file(
        &dir,
        "ops.json",
        &serde_json::to_value(one_stroke()).unwrap(),
    );
    let before = std::fs::read(&project).unwrap();
    let output = run(
        &dir,
        &[
            "history",
            "apply",
            project.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "--save",
        ],
    );
    assert!(!output.success);
    assert!(
        output.stderr.contains("unrecognized subcommand"),
        "stderr: {}",
        output.stderr
    );
    assert_eq!(std::fs::read(&project).unwrap(), before);

    let help = run(&dir, &["history", "--help"]);
    assert!(help.success);
    assert!(
        !help.stdout.contains("apply"),
        "history --help still lists apply: {}",
        help.stdout
    );
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
    Index::read(path).unwrap().read_entry(path, name).unwrap()
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
    let index = Index::read(src).unwrap();
    let mut writer = ContainerWriter::create(dst).unwrap();
    for name in index.names() {
        if drop.contains(&name) {
            continue;
        }
        let bytes = match replace.iter().find(|entry| entry.0 == name) {
            Some(entry) => entry.1.clone(),
            None => index.read_entry(src, name).unwrap(),
        };
        writer.put_bytes(name, &bytes).unwrap();
    }
    for (name, bytes) in add {
        writer.put_bytes(name, bytes).unwrap();
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

    // Exit 1: a manifest of another shape fails like any broken manifest.
    let mut manifest = read_manifest_value(&project_path);
    manifest
        .as_object_mut()
        .unwrap()
        .remove("registration")
        .expect("the sealed manifest carries a registration key");
    let reshaped = dir.join("reshaped.ffp");
    mutate_container(
        &project_path,
        &reshaped,
        &[],
        &[(
            "manifest.json".to_string(),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )],
        &[],
    );
    let output = run(&dir, &["history", "list", reshaped.to_str().unwrap()]);
    assert_eq!(output.exit_code, Some(1));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.contains("manifest.json"), "{}", output.stderr);

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
        &serde_json::to_value(vec![Op::BrushStroke {
            source_id: 1,
            path: vec![Point { x: 2, y: 2 }],
            radius: 1.0,
            softness: 0.0,
        }])
        .unwrap(),
    );
    let output = run(
        &ops_dir,
        &[
            "retouch",
            ops_project.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let child: Value = serde_json::from_str(&output.stdout).unwrap();
    let child_id = child["checkpoints"][0].as_str().unwrap().to_string();
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
    common::rewrite_changed(&source_path, |bytes| bytes[0] ^= 0xFF);

    let output = run(&dir, &["history", "list", project_path.to_str().unwrap()]);
    assert_eq!(output.exit_code, Some(9));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.contains("--accept-changed-sources"));

    let output = run(
        &dir,
        &[
            "history",
            "list",
            project_path.to_str().unwrap(),
            "--accept-changed-sources",
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

#[test]
fn accept_changed_sources_list_and_delete_changed_project() {
    let dir = common::tmp_dir("accept_changed_sources_list_and_delete");
    let project = seal_synthetic_project(&dir, 8);
    let child = retouch(
        &dir,
        &project,
        &[Op::BrushStroke {
            source_id: 1,
            path: vec![Point { x: 5, y: 5 }, Point { x: 7, y: 7 }],
            radius: 1.0,
            softness: 0.0,
        }],
        None,
    )
    .remove(0);
    assert_eq!(
        listed(&dir, &project).len(),
        2,
        "fixture needs two visible states"
    );

    let manifest = Project::open(&project).unwrap().manifest().clone();
    let source = dir.join(&manifest.sources[0].path);
    common::rewrite_changed(&source, |bytes| bytes[0] ^= 0xFF);

    let refused = run(&dir, &["history", "list", project.to_str().unwrap()]);
    assert_eq!(refused.exit_code, Some(9));
    assert!(refused.stdout.is_empty());
    assert!(refused.stderr.contains("--accept-changed-sources"));

    let before_refused_delete = std::fs::read(&project).unwrap();
    let refused_delete = run(
        &dir,
        &[
            "history",
            "delete",
            project.to_str().unwrap(),
            "--checkpoint",
            &child,
        ],
    );
    assert_eq!(refused_delete.exit_code, Some(9));
    assert!(refused_delete.stdout.is_empty());
    assert!(
        refused_delete.stderr.contains("--accept-changed-sources"),
        "{}",
        refused_delete.stderr
    );
    assert_eq!(std::fs::read(&project).unwrap(), before_refused_delete);

    let listed_changed = run(
        &dir,
        &[
            "history",
            "list",
            project.to_str().unwrap(),
            "--accept-changed-sources",
            "--json",
        ],
    );
    assert!(listed_changed.success, "stderr: {}", listed_changed.stderr);
    assert!(listed_changed.stderr.contains("warning:"));
    assert_eq!(
        serde_json::from_str::<Value>(&listed_changed.stdout).unwrap()["checkpoints"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let accepted = run(
        &dir,
        &[
            "history",
            "delete",
            project.to_str().unwrap(),
            "--checkpoint",
            &child,
            "--accept-changed-sources",
            "--json",
        ],
    );
    assert!(accepted.success, "stderr: {}", accepted.stderr);
    assert!(accepted.stderr.contains("warning:"), "{}", accepted.stderr);
    assert_eq!(
        serde_json::from_str::<Value>(&accepted.stdout).unwrap()["hidden"],
        child
    );

    let after_delete = run(
        &dir,
        &[
            "history",
            "list",
            project.to_str().unwrap(),
            "--accept-changed-sources",
            "--json",
        ],
    );
    assert!(after_delete.success, "stderr: {}", after_delete.stderr);
    assert!(after_delete.stderr.contains("warning:"));
    let remaining = serde_json::from_str::<Value>(&after_delete.stdout).unwrap()["checkpoints"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(remaining.len(), 1);
    assert!(remaining.iter().all(|checkpoint| checkpoint["id"] != child));

    let before_rejection = std::fs::read(&project).unwrap();
    for args in [
        vec![
            "history",
            "--accept-changed-sources",
            "list",
            project.to_str().unwrap(),
        ],
        vec![
            "history",
            "compact",
            project.to_str().unwrap(),
            "--accept-changed-sources",
        ],
    ] {
        let output = run(&dir, &args);
        assert_eq!(output.exit_code, Some(2), "stderr: {}", output.stderr);
        assert_eq!(std::fs::read(&project).unwrap(), before_rejection);
    }
}
