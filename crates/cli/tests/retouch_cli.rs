// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli retouch`: a Z-Map and an op list in, an edited Z-Map out.
//!
//! Every case builds its own fresh Z-Map (`fixtures/mantis-512/golden/laplacian-zmap.tif`,
//! or a synthetic 37x23, 10-frame map written directly with `ZMapWriter`) so no test shares
//! mutable state with another. The oracle throughout is `focusforge_core::retouch::apply`
//! replayed over the whole input map, read with `ZMapReader` -- test-side only, never the
//! command's own code path.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use focusforge_core::project::Op;
use focusforge_core::retouch;
use focusforge_core::tile::Rect;
use focusforge_core::zmap::{ZMapInfo, ZMapReader, ZMapWriter};

fn bin(test_name: &str) -> Command {
    common::cli_bin(test_name)
}

struct CliOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

fn run_cli(dir: &Path, cmd: &mut Command, args: &[&str]) -> CliOutput {
    let output = cmd
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

/// A 37x23, 10-frame synthetic map: `(x + y) % 10` per pixel, so its content
/// is neither uniform nor holds `NO_SOURCE`.
fn synthetic_map(dir: &Path, name: &str) -> PathBuf {
    let info = ZMapInfo::new(37, 23, 10).unwrap();
    let indices: Vec<u16> = (0..info.height())
        .flat_map(|y| (0..info.width()).map(move |x| ((x + y) % info.frame_count()) as u16))
        .collect();
    let path = dir.join(name);
    let mut writer = ZMapWriter::create(&path, info).unwrap();
    writer
        .write_tile(
            Rect {
                x: 0,
                y: 0,
                width: info.width(),
                height: info.height(),
            },
            &indices,
        )
        .unwrap();
    writer.finish().unwrap();
    path
}

fn read_whole(path: &Path) -> (ZMapInfo, Vec<u16>) {
    let reader = ZMapReader::open(path).unwrap();
    let info = reader.info();
    let rect = Rect {
        x: 0,
        y: 0,
        width: info.width(),
        height: info.height(),
    };
    let mut buf = vec![0u16; info.tile_len(rect)];
    reader.read_tile_into(rect, &mut buf).unwrap();
    (info, buf)
}

fn write_ops(dir: &Path, name: &str, json: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, json).unwrap();
    path
}

const SYNTHETIC_STROKE_OPS: &str = r#"[
    {"brush-stroke": {"source_id": 3, "path": [{"x": 0, "y": 0}, {"x": 10, "y": 5}], "radius": 4.0, "softness": 0.0}},
    {"brush-stroke": {"source_id": 7, "path": [{"x": 20, "y": 15}], "radius": 3.0, "softness": 0.0}}
]"#;

const THREE_OPS: &str = r#"[
    {"brush-stroke": {"source_id": 1, "path": [{"x": 2, "y": 2}], "radius": 2.0, "softness": 0.0}},
    {"brush-stroke": {"source_id": 4, "path": [{"x": 15, "y": 10}], "radius": 2.0, "softness": 0.0}},
    {"brush-stroke": {"source_id": 9, "path": [{"x": 30, "y": 18}], "radius": 2.0, "softness": 0.0}}
]"#;

/// The output equals a whole-map replay of the list, and
/// `pixels_changed` equals the input/output diff count -- on the fixture
/// map with the checked-in stroke list, and on a synthetic map.
#[test]
fn applies_ops_and_writes_a_valid_zmap() {
    let fixture_dir = common::tmp_dir("applies_ops_fixture");
    check_applies_and_matches_replay(
        "applies_ops_fixture",
        &fixture_dir,
        &common::fixtures_dir().join("mantis-512/golden/laplacian-zmap.tif"),
        &common::fixtures_dir().join("retouch/stroke-list.json"),
    );

    let dir = common::tmp_dir("applies_ops_synthetic");
    let zmap = synthetic_map(&dir, "base.tif");
    let ops = write_ops(&dir, "ops.json", SYNTHETIC_STROKE_OPS);
    check_applies_and_matches_replay("applies_ops_synthetic", &dir, &zmap, &ops);
}

fn check_applies_and_matches_replay(test_name: &str, dir: &Path, zmap: &Path, ops_path: &Path) {
    let out = dir.join("out.tif");
    let output = run_cli(
        dir,
        &mut bin(test_name),
        &[
            "retouch",
            "--zmap",
            zmap.to_str().unwrap(),
            "--ops",
            ops_path.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(output.success, "{test_name} stderr: {}", output.stderr);

    let (info, original) = read_whole(zmap);
    let ops_text = std::fs::read_to_string(ops_path).unwrap();
    let ops: Vec<Op> = serde_json::from_str(&ops_text).unwrap();
    let whole = Rect {
        x: 0,
        y: 0,
        width: info.width(),
        height: info.height(),
    };
    let mut expected = original.clone();
    for op in &ops {
        retouch::apply(op, info, whole, &mut expected).unwrap();
    }
    let expected_changed = original
        .iter()
        .zip(&expected)
        .filter(|(a, b)| a != b)
        .count() as u64;

    let (_, actual) = read_whole(&out);
    assert_eq!(
        actual, expected,
        "{test_name}: output must equal a whole-map replay"
    );

    let report: serde_json::Value = serde_json::from_str(&output.stdout).unwrap();
    assert_eq!(
        report["pixels_changed"].as_u64().unwrap(),
        expected_changed,
        "{test_name}: pixels_changed must equal the input/output diff count"
    );
}

/// `--undo <len>` reproduces the input byte for byte with
/// `pixels_changed: 0` and `ops_applied: 0`; `--undo 2 --redo 1` equals a
/// run over the list minus its last op.
#[test]
fn undo_all_reproduces_the_input() {
    let dir = common::tmp_dir("undo_all_reproduces_the_input");
    let zmap = synthetic_map(&dir, "base.tif");
    let ops = write_ops(&dir, "ops.json", THREE_OPS);

    let out_undo_all = dir.join("undo-all.tif");
    let output = run_cli(
        &dir,
        &mut bin("undo_all_reproduces_the_input_a"),
        &[
            "retouch",
            "--zmap",
            zmap.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "--out",
            out_undo_all.to_str().unwrap(),
            "--undo",
            "3",
            "--json",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);
    let report: serde_json::Value = serde_json::from_str(&output.stdout).unwrap();
    assert_eq!(report["pixels_changed"].as_u64().unwrap(), 0);
    assert_eq!(report["ops_applied"].as_u64().unwrap(), 0);
    assert_eq!(
        std::fs::read(&zmap).unwrap(),
        std::fs::read(&out_undo_all).unwrap(),
        "--undo <len> must reproduce the input byte for byte"
    );

    let out_undo2_redo1 = dir.join("undo2-redo1.tif");
    let output = run_cli(
        &dir,
        &mut bin("undo_all_reproduces_the_input_b"),
        &[
            "retouch",
            "--zmap",
            zmap.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "--out",
            out_undo2_redo1.to_str().unwrap(),
            "--undo",
            "2",
            "--redo",
            "1",
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);

    let parsed: Vec<Op> = serde_json::from_str(THREE_OPS).unwrap();
    let minus_last = serde_json::to_string(&parsed[..parsed.len() - 1]).unwrap();
    let ops_minus_last = write_ops(&dir, "ops-minus-last.json", &minus_last);
    let out_minus_last = dir.join("minus-last.tif");
    let output = run_cli(
        &dir,
        &mut bin("undo_all_reproduces_the_input_c"),
        &[
            "retouch",
            "--zmap",
            zmap.to_str().unwrap(),
            "--ops",
            ops_minus_last.to_str().unwrap(),
            "--out",
            out_minus_last.to_str().unwrap(),
        ],
    );
    assert!(output.success, "stderr: {}", output.stderr);

    assert_eq!(
        std::fs::read(&out_undo2_redo1).unwrap(),
        std::fs::read(&out_minus_last).unwrap(),
        "--undo 2 --redo 1 must equal a run over the list minus its last op"
    );
}

/// An op the core refuses fails naming its 0-based position, and
/// writes neither `--out` nor its `.partial`.
#[test]
fn refused_op_names_its_position_and_writes_nothing() {
    let dir = common::tmp_dir("refused_op_names_its_position_and_writes_nothing");

    // source_id out of range on the 8-frame fixture map.
    {
        let zmap = common::fixtures_dir().join("mantis-512/golden/laplacian-zmap.tif");
        let ops = write_ops(
            &dir,
            "source-out-of-range.json",
            r#"[{"brush-stroke": {"source_id": 8, "path": [{"x": 10, "y": 10}], "radius": 2.0, "softness": 0.0}}]"#,
        );
        let out = dir.join("source-out-of-range-out.tif");
        let output = run_cli(
            &dir,
            &mut bin("refused_source_out_of_range"),
            &[
                "retouch",
                "--zmap",
                zmap.to_str().unwrap(),
                "--ops",
                ops.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
            ],
        );
        assert!(!output.success);
        assert!(
            output.stderr.contains("op #0: source_id 8"),
            "stderr: {}",
            output.stderr
        );
        assert!(!out.exists());
        assert!(!dir.join("source-out-of-range-out.tif.partial").exists());
    }

    // A misspelled field name leaves `radius` at its serde default, 0.0.
    {
        let zmap = synthetic_map(&dir, "typo-base.tif");
        let ops = write_ops(
            &dir,
            "typo.json",
            r#"[{"brush-stroke": {"source_id": 1, "path": [{"x": 5, "y": 5}], "radious": 2.0, "softness": 0.0}}]"#,
        );
        let out = dir.join("typo-out.tif");
        let output = run_cli(
            &dir,
            &mut bin("refused_radius_typo"),
            &[
                "retouch",
                "--zmap",
                zmap.to_str().unwrap(),
                "--ops",
                ops.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
            ],
        );
        assert!(!output.success);
        assert!(
            output
                .stderr
                .contains("op #0: radius 0 is not a positive finite value"),
            "stderr: {}",
            output.stderr
        );
        assert!(!out.exists());
        assert!(!dir.join("typo-out.tif.partial").exists());
    }

    // An unsupported op variant, not at position 0.
    {
        let zmap = synthetic_map(&dir, "unsupported-base.tif");
        let ops = write_ops(
            &dir,
            "unsupported.json",
            r#"[
                {"brush-stroke": {"source_id": 1, "path": [{"x": 5, "y": 5}], "radius": 2.0, "softness": 0.0}},
                {"flood-fill": {"seed": {"x": 1, "y": 1}, "source_id": 1, "tolerance": 0.1}}
            ]"#,
        );
        let out = dir.join("unsupported-out.tif");
        let output = run_cli(
            &dir,
            &mut bin("refused_unsupported_op"),
            &[
                "retouch",
                "--zmap",
                zmap.to_str().unwrap(),
                "--ops",
                ops.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
            ],
        );
        assert!(!output.success);
        assert!(
            output.stderr.contains("op #1: flood-fill is not supported"),
            "stderr: {}",
            output.stderr
        );
        assert!(!out.exists());
        assert!(!dir.join("unsupported-out.tif.partial").exists());
    }
}

/// Two identical runs produce byte-identical output.
#[test]
fn run_is_reproducible() {
    let dir = common::tmp_dir("run_is_reproducible");
    let zmap = common::fixtures_dir().join("mantis-512/golden/laplacian-zmap.tif");
    let ops = common::fixtures_dir().join("retouch/stroke-list.json");
    let out1 = dir.join("out1.tif");
    let out2 = dir.join("out2.tif");

    for (name, out) in [
        ("run_is_reproducible_a", &out1),
        ("run_is_reproducible_b", &out2),
    ] {
        let output = run_cli(
            &dir,
            &mut bin(name),
            &[
                "retouch",
                "--zmap",
                zmap.to_str().unwrap(),
                "--ops",
                ops.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
            ],
        );
        assert!(output.success, "stderr: {}", output.stderr);
    }

    assert_eq!(std::fs::read(&out1).unwrap(), std::fs::read(&out2).unwrap());
}

/// `--json` prints exactly one JSON document holding `ops_applied`,
/// `pixels_changed` and `wrote`, with stderr empty on success; the default
/// stdout is the human report and does not parse as JSON.
#[test]
fn stdout_json_is_one_document() {
    let dir = common::tmp_dir("stdout_json_is_one_document");
    let zmap = synthetic_map(&dir, "base.tif");
    let ops = write_ops(&dir, "ops.json", SYNTHETIC_STROKE_OPS);

    let out_json = dir.join("out-json.tif");
    let json_output = run_cli(
        &dir,
        &mut bin("stdout_json_is_one_document_json"),
        &[
            "retouch",
            "--zmap",
            zmap.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "--out",
            out_json.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(json_output.success, "stderr: {}", json_output.stderr);
    assert!(
        json_output.stderr.is_empty(),
        "stderr must be empty on success: {}",
        json_output.stderr
    );
    let mut docs =
        serde_json::Deserializer::from_str(&json_output.stdout).into_iter::<serde_json::Value>();
    let doc = docs
        .next()
        .expect("stdout must hold exactly one JSON document")
        .unwrap();
    assert!(
        docs.next().is_none(),
        "stdout must hold exactly one JSON document"
    );
    assert!(doc.get("ops_applied").is_some());
    assert!(doc.get("pixels_changed").is_some());
    assert!(doc.get("wrote").is_some());

    let out_human = dir.join("out-human.tif");
    let human_output = run_cli(
        &dir,
        &mut bin("stdout_json_is_one_document_human"),
        &[
            "retouch",
            "--zmap",
            zmap.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "--out",
            out_human.to_str().unwrap(),
        ],
    );
    assert!(human_output.success, "stderr: {}", human_output.stderr);
    assert!(
        serde_json::from_str::<serde_json::Value>(&human_output.stdout).is_err(),
        "the default human report must not parse as JSON"
    );
}

/// A malformed op list -- truncated JSON, or an unknown op tag --
/// fails naming the `--ops` file and serde's line and column, non-zero
/// exit, no output.
#[test]
fn malformed_op_list_is_a_clean_error() {
    let dir = common::tmp_dir("malformed_op_list_is_a_clean_error");
    let zmap = synthetic_map(&dir, "base.tif");

    {
        let ops = write_ops(
            &dir,
            "truncated.json",
            r#"[{"brush-stroke": {"source_id": 1"#,
        );
        let out = dir.join("truncated-out.tif");
        let output = run_cli(
            &dir,
            &mut bin("malformed_truncated"),
            &[
                "retouch",
                "--zmap",
                zmap.to_str().unwrap(),
                "--ops",
                ops.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
            ],
        );
        assert!(!output.success);
        assert!(output.stdout.is_empty());
        assert!(
            output.stderr.contains(ops.to_str().unwrap())
                && output.stderr.contains("line")
                && output.stderr.contains("column"),
            "stderr: {}",
            output.stderr
        );
        assert!(!out.exists());
    }

    {
        let ops = write_ops(&dir, "unknown-tag.json", r#"[{"paint-bucket": {}}]"#);
        let out = dir.join("unknown-tag-out.tif");
        let output = run_cli(
            &dir,
            &mut bin("malformed_unknown_tag"),
            &[
                "retouch",
                "--zmap",
                zmap.to_str().unwrap(),
                "--ops",
                ops.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
            ],
        );
        assert!(!output.success);
        assert!(output.stdout.is_empty());
        assert!(
            output.stderr.contains(ops.to_str().unwrap())
                && output.stderr.contains("line")
                && output.stderr.contains("column"),
            "stderr: {}",
            output.stderr
        );
        assert!(!out.exists());
    }
}

/// `--undo` past the list length, or `--redo` past `--undo`, is
/// refused naming that flag, and writes nothing.
#[test]
fn undo_redo_bounds_are_refused() {
    let dir = common::tmp_dir("undo_redo_bounds_are_refused");
    let zmap = synthetic_map(&dir, "base.tif");
    let ops = write_ops(&dir, "ops.json", THREE_OPS);

    {
        let out = dir.join("undo-too-large-out.tif");
        let output = run_cli(
            &dir,
            &mut bin("undo_too_large"),
            &[
                "retouch",
                "--zmap",
                zmap.to_str().unwrap(),
                "--ops",
                ops.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
                "--undo",
                "4",
            ],
        );
        assert!(!output.success);
        assert!(
            output.stderr.contains("--undo"),
            "stderr: {}",
            output.stderr
        );
        assert!(!out.exists());
    }

    {
        let out = dir.join("redo-too-large-out.tif");
        let output = run_cli(
            &dir,
            &mut bin("redo_too_large"),
            &[
                "retouch",
                "--zmap",
                zmap.to_str().unwrap(),
                "--ops",
                ops.to_str().unwrap(),
                "--out",
                out.to_str().unwrap(),
                "--undo",
                "1",
                "--redo",
                "2",
            ],
        );
        assert!(!output.success);
        assert!(
            output.stderr.contains("--redo"),
            "stderr: {}",
            output.stderr
        );
        assert!(!out.exists());
    }
}

/// `--out` naming the same file as `--zmap` -- directly, or through
/// a `./` or `..` detour -- is refused, and the input is unchanged.
#[test]
fn out_colliding_with_input_is_refused() {
    let dir = common::tmp_dir("out_colliding_with_input_is_refused");
    let zmap = synthetic_map(&dir, "base.tif");
    let ops = write_ops(&dir, "ops.json", SYNTHETIC_STROKE_OPS);
    let before = std::fs::read(&zmap).unwrap();

    {
        let output = run_cli(
            &dir,
            &mut bin("collide_exact"),
            &[
                "retouch",
                "--zmap",
                zmap.to_str().unwrap(),
                "--ops",
                ops.to_str().unwrap(),
                "--out",
                zmap.to_str().unwrap(),
            ],
        );
        assert!(!output.success);
        assert_eq!(std::fs::read(&zmap).unwrap(), before);
    }

    {
        let detour = dir.join(".").join("base.tif");
        let output = run_cli(
            &dir,
            &mut bin("collide_dot"),
            &[
                "retouch",
                "--zmap",
                zmap.to_str().unwrap(),
                "--ops",
                ops.to_str().unwrap(),
                "--out",
                detour.to_str().unwrap(),
            ],
        );
        assert!(!output.success);
        assert_eq!(std::fs::read(&zmap).unwrap(), before);
    }

    {
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let detour = sub.join("..").join("base.tif");
        let output = run_cli(
            &dir,
            &mut bin("collide_dotdot"),
            &[
                "retouch",
                "--zmap",
                zmap.to_str().unwrap(),
                "--ops",
                ops.to_str().unwrap(),
                "--out",
                detour.to_str().unwrap(),
            ],
        );
        assert!(!output.success);
        assert_eq!(std::fs::read(&zmap).unwrap(), before);
    }
}

/// `--out` reaching the input through a `..` after a symlinked directory is
/// refused: the kernel resolves that `..` against the link's target, not
/// against the path text.
#[cfg(unix)]
#[test]
fn out_reaching_input_through_a_symlinked_dir_is_refused() {
    let dir = common::tmp_dir("out_reaching_input_through_a_symlinked_dir_is_refused");
    let deep = dir.join("deep");
    std::fs::create_dir_all(deep.join("a")).unwrap();
    let zmap = synthetic_map(&deep, "base.tif");
    let ops = write_ops(&dir, "ops.json", SYNTHETIC_STROKE_OPS);
    let before = std::fs::read(&zmap).unwrap();

    // `dir/l/../base.tif` reads as `dir/base.tif`, but `l` is `deep/a`, so
    // the kernel opens `deep/base.tif`: the input.
    std::os::unix::fs::symlink(deep.join("a"), dir.join("l")).unwrap();
    let detour = dir.join("l").join("..").join("base.tif");
    let output = run_cli(
        &dir,
        &mut bin("collide_symlink_dotdot"),
        &[
            "retouch",
            "--zmap",
            zmap.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "--out",
            detour.to_str().unwrap(),
        ],
    );
    assert!(!output.success, "stdout: {}", output.stdout);
    assert_eq!(std::fs::read(&zmap).unwrap(), before);
    assert!(!dir.join("base.tif").exists());
}

/// `--zmap` given as a symlink and `--out` naming that same link is refused:
/// the link stays in place and its target is unchanged.
#[cfg(unix)]
#[test]
fn out_naming_a_symlinked_input_is_refused() {
    let dir = common::tmp_dir("out_naming_a_symlinked_input_is_refused");
    let real = dir.join("real");
    std::fs::create_dir_all(&real).unwrap();
    let target = synthetic_map(&real, "base.tif");
    let ops = write_ops(&dir, "ops.json", SYNTHETIC_STROKE_OPS);
    let before = std::fs::read(&target).unwrap();

    let link = dir.join("link.tif");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let output = run_cli(
        &dir,
        &mut bin("collide_symlinked_input"),
        &[
            "retouch",
            "--zmap",
            link.to_str().unwrap(),
            "--ops",
            ops.to_str().unwrap(),
            "--out",
            link.to_str().unwrap(),
        ],
    );
    assert!(!output.success, "stdout: {}", output.stdout);
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read(&target).unwrap(), before);
}
