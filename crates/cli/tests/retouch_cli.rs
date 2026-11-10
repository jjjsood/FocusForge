// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli retouch`: an op list appended to a project as one state
//! per op. One `dummy-stack` run over `fixtures/mantis-512` is shared through
//! a `OnceLock`; every test copies the sealed project into its own temp
//! directory before appending to it.

mod common;

use common::only_project;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use focusforge_core::project::{self, Project};
use serde_json::Value;

fn run(dir: &Path, args: &[&str]) -> common::CliOutput {
    common::run_in(&mut common::cli_bin("retouch_cli"), dir, args)
}

fn arg(path: &Path) -> &str {
    path.to_str().expect("test paths are UTF-8")
}

struct SharedStack {
    _cwd: common::TmpDir,
    project: PathBuf,
}

/// One `dummy-stack` run over `fixtures/mantis-512`. Each test process that
/// reads it runs it once.
fn shared_stack() -> &'static SharedStack {
    static ONCE: OnceLock<SharedStack> = OnceLock::new();
    ONCE.get_or_init(|| {
        let cwd = common::tmp_dir("retouch_cli_shared_stack");
        let out = cwd.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let input = common::mantis_512_dir();
        let output = run(
            &cwd,
            &[
                "stack",
                arg(&input),
                "-o",
                arg(&out),
                "--algorithm",
                "dummy-stack",
                "--no-align",
                "--no-crop",
            ],
        );
        assert!(output.success, "stack: {}", output.stderr);
        let project = only_project(&out);
        SharedStack { _cwd: cwd, project }
    })
}

/// A fresh copy of the shared project, in its own temp directory.
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

/// The three strokes of `fixtures/retouch/stroke-list.json`.
fn fixture_strokes() -> Vec<Value> {
    let path = common::fixtures_dir().join("retouch/stroke-list.json");
    let strokes: Vec<Value> = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(strokes.len(), 3, "the fixture holds three strokes");
    strokes
}

fn write_json(dir: &Path, name: &str, body: &Value) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body.to_string()).unwrap();
    path
}

fn stroke(source_id: u32, radius: f64, softness: f64, path: &[(i32, i32)]) -> Value {
    let path: Vec<Value> = path
        .iter()
        .map(|&(x, y)| serde_json::json!({"x": x, "y": y}))
        .collect();
    serde_json::json!({"brush-stroke": {
        "source_id": source_id, "path": path, "radius": radius, "softness": softness,
    }})
}

/// Runs `retouch --json` and returns the new ids in order.
fn retouch_ok(dir: &Path, project: &Path, ops: &Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["retouch", arg(project), "--ops", arg(ops), "--json"];
    args.extend_from_slice(extra);
    let output = run(dir, &args);
    assert!(output.success, "{args:?} stderr: {}", output.stderr);
    let report: Value = serde_json::from_str(&output.stdout).unwrap();
    report["checkpoints"]
        .as_array()
        .expect("the report lists the new ids")
        .iter()
        .map(|id| id.as_str().unwrap().to_string())
        .collect()
}

/// The samples `export --checkpoint` writes for `id`.
fn exported(dir: &Path, project: &Path, id: &str) -> Vec<u16> {
    let out = dir.join(format!("export-{id}.tif"));
    let output = run(
        dir,
        &["export", arg(project), "--checkpoint", id, "-o", arg(&out)],
    );
    assert!(output.success, "export {id}: {}", output.stderr);
    common::read_whole(&out).0
}

/// The samples `project::render_checkpoint` writes for `id`.
fn rendered(dir: &Path, project: &Path, id: &str) -> Vec<u16> {
    let out = dir.join(format!("render-{id}.tif"));
    let mut open = Project::open(project).unwrap();
    project::render_checkpoint(&mut open, id, &out, None).unwrap();
    common::read_whole(&out).0
}

#[test]
fn retouch_appends_states_and_exports_them() {
    let (dir, project) = fresh_project("retouch_appends_states_and_exports_them");
    let root = root_id(&project);
    let strokes = fixture_strokes();
    let ops = write_json(&dir, "ops.json", &Value::Array(strokes.clone()));

    let chain = retouch_ok(&dir, &project, &ops, &["--name", "strokes"]);
    assert_eq!(chain.len(), 3, "one state per op");

    let third = write_json(&dir, "third.json", &Value::Array(vec![strokes[2].clone()]));
    let prefix = &chain[0][..8];
    let branch = retouch_ok(
        &dir,
        &project,
        &third,
        &["--from", prefix, "--name", "side"],
    );
    assert_eq!(branch.len(), 1);

    let open = Project::open(&project).unwrap();
    let history = open.history();
    let mut parent = root.clone();
    for id in &chain {
        let checkpoint = history.get(id).unwrap();
        assert_eq!(checkpoint.parent.as_deref(), Some(parent.as_str()));
        assert_eq!(checkpoint.ops.len(), 1);
        parent = id.clone();
    }
    let labels: Vec<Option<&str>> = chain
        .iter()
        .map(|id| history.get(id).unwrap().label.as_deref())
        .collect();
    assert_eq!(
        labels,
        [None, None, Some("strokes")],
        "--name labels the last state"
    );
    let side = history.get(&branch[0]).unwrap();
    assert_eq!(
        side.parent.as_deref(),
        Some(chain[0].as_str()),
        "--from branches"
    );
    assert_eq!(side.label.as_deref(), Some("side"));
    assert_eq!(
        history.get(&chain[1]).unwrap().parent.as_deref(),
        Some(chain[0].as_str())
    );
    assert_eq!(
        history.children(&chain[0]).len(),
        2,
        "the original child stays"
    );
    assert_eq!(
        open.checkpoint().id,
        branch[0],
        "the head follows the last append"
    );
    drop(open);

    let root_export = exported(&dir, &project, &root);
    for id in chain.iter().chain(&branch) {
        let export = exported(&dir, &project, id);
        assert_eq!(
            export,
            rendered(&dir, &project, id),
            "{id}: export is the render"
        );
        assert_ne!(export, root_export, "{id}: strokes change the image");
    }
    // Stroke 2 runs past the right edge: clipped, not refused, and it paints.
    assert_ne!(
        exported(&dir, &project, &chain[1]),
        exported(&dir, &project, &chain[0]),
        "the clipped stroke changes the image"
    );
    assert_ne!(
        exported(&dir, &project, &chain[2]),
        exported(&dir, &project, &chain[1]),
        "the last stroke is blended over the second"
    );
}

/// Runs `retouch` over `body`, expects a refusal that wrote nothing, and
/// returns its stderr.
fn refuse(dir: &Path, project: &Path, name: &str, body: &Value) -> String {
    let ops = write_json(dir, name, body);
    let before = std::fs::read(project).unwrap();
    let output = run(dir, &["retouch", arg(project), "--ops", arg(&ops)]);
    assert!(!output.success, "{name} must be refused");
    assert!(output.stdout.is_empty(), "{name}: stdout {}", output.stdout);
    assert!(
        !output.stderr.contains("panicked"),
        "{name}: {}",
        output.stderr
    );
    assert_eq!(
        std::fs::read(project).unwrap(),
        before,
        "{name}: project untouched"
    );
    output.stderr
}

#[test]
fn retouch_refuses_bad_ops_and_leaves_project_untouched() {
    let (dir, project) = fresh_project("retouch_refuses_bad_ops");
    let good = stroke(1, 4.0, 0.2, &[(40, 40), (60, 50)]);
    let cases: [(&str, Value, &str); 5] = [
        ("source", stroke(8, 4.0, 0.2, &[(40, 40)]), "source_id"),
        ("radius-zero", stroke(1, 0.0, 0.2, &[(40, 40)]), "radius"),
        (
            "radius-negative",
            stroke(1, -3.0, 0.2, &[(40, 40)]),
            "radius",
        ),
        ("softness", stroke(1, 4.0, 1.5, &[(40, 40)]), "softness"),
        ("path", stroke(1, 4.0, 0.2, &[]), "path"),
    ];
    for (name, bad, field) in cases {
        for position in [0usize, 2] {
            let mut list = vec![good.clone(); 3];
            list[position] = bad.clone();
            let file = format!("{name}-{position}.json");
            let stderr = refuse(&dir, &project, &file, &Value::Array(list));
            assert!(
                stderr.contains(&format!("op #{position}")) && stderr.contains(field),
                "{name} at {position}: {stderr}"
            );
        }
    }
    refuse(&dir, &project, "empty.json", &serde_json::json!([]));
    refuse(
        &dir,
        &project,
        "wrapped.json",
        &serde_json::json!({"ops": [good]}),
    );
}

/// `stroke` with `field` set inside the brush-stroke body.
fn with_field(mut op: Value, field: &str, value: Value) -> Value {
    op["brush-stroke"][field] = value;
    op
}

#[test]
fn mode_opacity_bad_values_are_refused_with_their_position() {
    let (dir, project) = fresh_project("retouch_mode_opacity_refused");
    let good = stroke(1, 4.0, 0.2, &[(40, 40), (60, 50)]);
    let opacity = |value: f64| with_field(good.clone(), "opacity", serde_json::json!(value));

    let list = Value::Array(vec![good.clone(), good.clone(), opacity(0.0)]);
    let stderr = refuse(&dir, &project, "opacity-zero.json", &list);
    assert!(
        stderr.contains("op #2") && stderr.contains("opacity"),
        "{stderr}"
    );
    for value in [-0.5, 1.5] {
        let list = Value::Array(vec![good.clone(), opacity(value), good.clone()]);
        let stderr = refuse(&dir, &project, "opacity-range.json", &list);
        assert!(
            stderr.contains("op #1") && stderr.contains("opacity"),
            "{value}: {stderr}"
        );
    }

    let blur = with_field(good.clone(), "mode", serde_json::json!("blur"));
    let list = Value::Array(vec![good.clone(), blur, good.clone()]);
    let stderr = refuse(&dir, &project, "mode-blur.json", &list);
    assert!(
        stderr.contains("op #1") && stderr.contains("blur"),
        "{stderr}"
    );
}

#[test]
fn mode_opacity_valid_and_absent_values_append_one_state_per_op() {
    let (dir, project) = fresh_project("retouch_mode_opacity_valid");
    let good = stroke(1, 4.0, 0.2, &[(40, 40), (60, 50)]);
    let list = Value::Array(vec![
        good.clone(),
        with_field(good.clone(), "opacity", serde_json::json!(0.5)),
        with_field(good.clone(), "mode", serde_json::json!("normal")),
        with_field(good.clone(), "opacity", serde_json::json!(1e-9)),
        with_field(good.clone(), "mode", serde_json::json!("lighten")),
    ]);
    let ops = write_json(&dir, "valid.json", &list);
    assert_eq!(retouch_ok(&dir, &project, &ops, &[]).len(), 5);
}

#[test]
fn malformed_op_list_is_a_clean_error() {
    let (dir, project) = fresh_project("retouch_malformed_op_list");
    let bad_json = dir.join("not-json.json");
    std::fs::write(&bad_json, "[{\"brush-stroke\": ").unwrap();
    let before = std::fs::read(&project).unwrap();
    let output = run(&dir, &["retouch", arg(&project), "--ops", arg(&bad_json)]);
    assert!(!output.success);
    assert!(output.stderr.contains("not-json.json"), "{}", output.stderr);
    assert!(!output.stderr.contains("panicked"), "{}", output.stderr);
    assert_eq!(std::fs::read(&project).unwrap(), before);

    let unknown = serde_json::json!([
        {"flood-fill": {"seed": {"x": 1, "y": 1}, "source_id": 1, "tolerance": 0.1}}
    ]);
    let stderr = refuse(&dir, &project, "flood-fill.json", &unknown);
    assert!(
        stderr.contains("unknown variant") && stderr.contains("flood-fill"),
        "{stderr}"
    );

    let missing = dir.join("absent.json");
    let output = run(&dir, &["retouch", arg(&project), "--ops", arg(&missing)]);
    assert!(!output.success);
    assert!(output.stderr.contains("absent.json"), "{}", output.stderr);
}

#[test]
fn stdout_json_is_one_document() {
    let (dir, project) = fresh_project("retouch_stdout_json");
    let root = root_id(&project);
    let ops = write_json(&dir, "ops.json", &Value::Array(fixture_strokes()));
    let output = run(
        &dir,
        &["retouch", arg(&project), "--ops", arg(&ops), "--json"],
    );
    assert!(output.success, "{}", output.stderr);
    let report: Value = serde_json::from_str(&output.stdout).expect("stdout is one JSON document");
    assert_eq!(report["from"], root.as_str());
    assert_eq!(report["ops"], 3);
    assert_eq!(report["checkpoints"].as_array().unwrap().len(), 3);
    assert_eq!(report["project"], arg(&project));
}

/// A brush stroke named by a `source` record, `{"frame": n}` or
/// `{"checkpoint": "<id prefix>"}`.
fn with_source(source: Value, radius: f64, softness: f64, path: &[(i32, i32)]) -> Value {
    let mut op = stroke(0, radius, softness, path);
    let body = op["brush-stroke"].as_object_mut().unwrap();
    body.remove("source_id");
    body.insert("source".into(), source);
    op
}

fn visible_ids(project: &Path) -> Vec<String> {
    let open = Project::open(project).unwrap();
    let ids = open.history().visible();
    ids.into_iter().map(|c| c.id.clone()).collect()
}

/// The shortest prefix of `id`, at least 4 characters, that no other visible
/// id shares.
fn shortest_unique_prefix(project: &Path, id: &str) -> String {
    let visible = visible_ids(project);
    (4..=id.len())
        .map(|n| &id[..n])
        .find(|prefix| visible.iter().filter(|v| v.starts_with(prefix)).count() == 1)
        .expect("a full id is unique")
        .to_string()
}

/// The checkpoint source of the one stroke of state `id`.
fn stored_source(project: &Path, id: &str) -> Option<String> {
    let open = Project::open(project).unwrap();
    let checkpoint = open.history().get(id).unwrap();
    assert_eq!(checkpoint.ops.len(), 1);
    checkpoint.ops[0].checkpoint_source().map(str::to_string)
}

#[test]
fn retouch_checkpoint_sources() {
    let (dir, project) = fresh_project("retouch_checkpoint_sources");
    let root = root_id(&project);
    let at = [(100, 100)];
    let frame = |n: u32| with_source(serde_json::json!({ "frame": n }), 20.0, 0.0, &at);
    let from_checkpoint =
        |prefix: &str| with_source(serde_json::json!({ "checkpoint": prefix }), 30.0, 0.0, &at);

    // Painting from the base at full strength restores the area bit-identically.
    let root_prefix = shortest_unique_prefix(&project, &root);
    let restore = write_json(
        &dir,
        "restore.json",
        &Value::Array(vec![frame(1), from_checkpoint(&root_prefix)]),
    );
    let painted = retouch_ok(&dir, &project, &restore, &[]);
    assert_eq!(painted.len(), 2, "restore: one state per op");
    let base = exported(&dir, &project, &root);
    assert_ne!(
        exported(&dir, &project, &painted[0]),
        base,
        "restore: the frame stroke changes the image"
    );
    assert_eq!(
        exported(&dir, &project, &painted[1]),
        base,
        "restore: painting from the base restores it"
    );
    assert_eq!(
        stored_source(&project, &painted[1]).as_deref(),
        Some(root.as_str())
    );

    // A mixed list: a legacy frame stroke, the full id, the shortest unique
    // prefix and a `frame` record; each stored reference is the full id.
    let painted_from = painted[0].clone();
    let short = shortest_unique_prefix(&project, &painted_from);
    assert!(
        short.len() >= 4 && short.len() <= painted_from.len(),
        "{short}"
    );
    let mixed = write_json(
        &dir,
        "mixed.json",
        &Value::Array(vec![
            stroke(1, 20.0, 0.0, &at),
            from_checkpoint(&painted_from),
            from_checkpoint(&short),
            frame(2),
        ]),
    );
    let chain = retouch_ok(&dir, &project, &mixed, &["--from", &root]);
    assert_eq!(chain.len(), 4, "mixed: one state per op");
    let sources: Vec<Option<String>> = chain.iter().map(|id| stored_source(&project, id)).collect();
    assert_eq!(
        sources,
        [
            None,
            Some(painted_from.clone()),
            Some(painted_from.clone()),
            None
        ],
        "mixed: stored references are full ids"
    );
    let open = Project::open(&project).unwrap();
    assert_eq!(
        open.history().get(&chain[0]).unwrap().ops,
        open.history().get(&painted[0]).unwrap().ops,
        "mixed: source_id n is the frame n record"
    );
    drop(open);

    // `--from` and a checkpoint source resolve one prefix the same way.
    let one_frame = write_json(&dir, "one-frame.json", &Value::Array(vec![frame(1)]));
    let unique = shortest_unique_prefix(&project, &root);
    let via_from = retouch_ok(&dir, &project, &one_frame, &["--from", &unique]);
    let parent = Project::open(&project)
        .unwrap()
        .history()
        .get(&via_from[0])
        .unwrap()
        .parent
        .clone();
    assert_eq!(parent.as_deref(), Some(root.as_str()), "--from resolves it");
    let one_source = write_json(
        &dir,
        "one-source.json",
        &Value::Array(vec![from_checkpoint(&unique)]),
    );
    let via_source = retouch_ok(&dir, &project, &one_source, &[]);
    assert_eq!(
        stored_source(&project, &via_source[0]).as_deref(),
        Some(root.as_str()),
        "a source resolves the same prefix to the same checkpoint"
    );

    // A deleted checkpoint that strokes still paint from stays stored, hidden.
    let output = run(
        &dir,
        &[
            "history",
            "delete",
            arg(&project),
            "--checkpoint",
            &painted_from,
        ],
    );
    assert!(output.success, "history delete: {}", output.stderr);
    let hidden = Project::open(&project)
        .unwrap()
        .history()
        .get(&painted_from)
        .unwrap()
        .hidden;
    assert!(hidden, "the deleted checkpoint is kept, hidden");

    let visible = visible_ids(&project);
    let count = format!("matches {} checkpoints", visible.len());
    let mut ambiguous = vec![count.as_str()];
    ambiguous.extend(visible.iter().map(String::as_str));
    let named = format!("no checkpoint named {painted_from}");
    let cases: [(&str, &str, Vec<&str>); 3] = [
        ("unknown", "zz", vec!["no checkpoint named zz"]),
        ("ambiguous", "", ambiguous),
        ("hidden", &painted_from, vec![named.as_str()]),
    ];
    for (name, prefix, parts) in cases {
        for position in [0usize, 2] {
            let mut list = vec![frame(1); 3];
            list[position] = from_checkpoint(prefix);
            let file = format!("{name}-{position}.json");
            let stderr = refuse(&dir, &project, &file, &Value::Array(list));
            let lead = format!("op #{position}: ");
            assert!(stderr.contains(&lead), "{name} at {position}: {stderr}");
            for part in &parts {
                assert!(stderr.contains(part), "{name} at {position}: {stderr}");
            }
        }
    }
    for prefix in ["zz", ""] {
        let args = [
            "retouch",
            arg(&project),
            "--ops",
            arg(&one_frame),
            "--from",
            prefix,
        ];
        let from = run(&dir, &args);
        let ops = write_json(
            &dir,
            "bad-source.json",
            &Value::Array(vec![from_checkpoint(prefix)]),
        );
        let source = run(&dir, &["retouch", arg(&project), "--ops", arg(&ops)]);
        assert!(!from.success && !source.success, "{prefix:?}");
        let (_, after) = source
            .stderr
            .split_once("op #0: ")
            .expect("the op position leads");
        assert_eq!(
            from.stderr.trim_start_matches("error: ").trim_end(),
            after.trim_end(),
            "{prefix:?}: --from and a source say the same"
        );
    }
}

#[test]
fn retouch_help_names_both_sources() {
    let dir = common::tmp_dir("retouch_help_sources");
    let output = run(&dir, &["retouch", "--help"]);
    assert!(output.success, "{}", output.stderr);
    for part in [
        r#"{"frame": n}"#,
        r#"{"checkpoint": "<id prefix>"}"#,
        r#""source_id": n` reads as frame n"#,
        "unique prefix",
    ] {
        assert!(
            output.stdout.contains(part),
            "`{part}` missing: {}",
            output.stdout
        );
    }
}
