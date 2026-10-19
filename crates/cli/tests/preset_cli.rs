// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level coverage of `--preset`, `--write-preset`, `--status` and
//! `--dry-run` on `focusforge-cli stack`.
//!
//! Every refusal path runs against `dummy-stack` (registered in this crate's
//! test binary through `crates/cli/Cargo.toml`'s `[dev-dependencies]`
//! feature, as `crates/cli/tests/algorithms_cmd.rs` documents) and never
//! opens a tile. The byte-for-byte tests pay for a real `laplacian` fusion
//! over `fixtures/mantis-512` with `--no-align --no-crop`, which is the
//! cheapest invocation whose pixels actually depend on a parameter.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{CliOutput, mantis_512_dir, tmp_dir};

fn bin() -> Command {
    common::cli_bin("preset_cli")
}

fn run_cli_in(dir: &Path, args: &[&str]) -> CliOutput {
    common::run_in(&mut bin(), dir, args)
}

fn write_preset(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("preset must be writable");
    path
}

fn read_bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("output file must be readable")
}

/// The composite of `project`, exported beside it. Two containers of the
/// same stack differ in `created_at`, so byte comparisons go through `export`.
fn export_composite(cwd: &Path, project: &Path) -> PathBuf {
    let exported = project.with_extension("exported.tif");
    let out = run_cli_in(
        cwd,
        &[
            "export",
            project.to_str().unwrap(),
            "-o",
            exported.to_str().unwrap(),
        ],
    );
    assert!(out.success, "export stderr: {}", out.stderr);
    exported
}

#[test]
fn preset_file_matches_equivalent_set_flags() {
    let dir = tmp_dir("preset_cli_matches_set");
    let mantis = mantis_512_dir();
    let preset = write_preset(&dir, "p.toml", "[stack]\nlevels = 3\n");

    let preset_dir = dir.join("preset-out");
    std::fs::create_dir_all(&preset_dir).unwrap();
    let from_preset = preset_dir.join("mantis-512.ffp");
    let out = run_cli_in(
        &dir,
        &common::push_no_gpu(vec![
            "stack",
            mantis.to_str().unwrap(),
            "-o",
            from_preset.parent().unwrap().to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--preset",
            preset.to_str().unwrap(),
            "--no-align",
            "--no-crop",
        ]),
    );
    assert!(out.success, "stderr: {}", out.stderr);

    let flags_dir = dir.join("flags-out");
    std::fs::create_dir_all(&flags_dir).unwrap();
    let from_flags = flags_dir.join("mantis-512.ffp");
    let out = run_cli_in(
        &dir,
        &common::push_no_gpu(vec![
            "stack",
            mantis.to_str().unwrap(),
            "-o",
            from_flags.parent().unwrap().to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--set",
            "levels=3",
            "--no-align",
            "--no-crop",
        ]),
    );
    assert!(out.success, "stderr: {}", out.stderr);

    assert_eq!(
        read_bytes(&export_composite(&dir, &from_preset)),
        read_bytes(&export_composite(&dir, &from_flags)),
        "a preset value must produce the same composite as the equivalent --set"
    );
}

#[test]
fn set_flag_overrides_preset_file() {
    let dir = tmp_dir("preset_cli_set_overrides");
    let preset = write_preset(&dir, "p.toml", "[stack]\nthreshold = 0.25\n");
    let out = run_cli_in(
        &dir,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--preset",
            preset.to_str().unwrap(),
            "--set",
            "threshold=0.75",
            "--json",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(out.success, "stderr: {}", out.stderr);

    let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("--json must parse");
    let threshold = report["params"]
        .as_array()
        .expect("params must be an array")
        .iter()
        .find(|p| p["id"] == "threshold")
        .expect("threshold must be reported")
        .clone();
    assert_eq!(threshold["value"], 0.75, "the flag must beat the file");
    assert_eq!(
        threshold["provenance"], "Explicit",
        "an overridden value stays Explicit"
    );
}

#[test]
fn invalid_preset_value_is_refused() {
    let dir = tmp_dir("preset_cli_invalid_value");
    let mantis = mantis_512_dir();

    // Each pair: a preset table, and the `--set` that must be refused with
    // the same `ParamViolation` rendering.
    let cases = [
        ("[stack]\nthreshold = 9.5\n", "threshold=9.5"),
        ("[stack]\nnope = 1\n", "nope=1"),
        ("[stack]\nmode = \"zzz\"\n", "mode=zzz"),
    ];
    for (i, (table, set)) in cases.iter().enumerate() {
        let preset = write_preset(&dir, &format!("p{i}.toml"), table);
        let from_preset = run_cli_in(
            &dir,
            &[
                "stack",
                mantis.to_str().unwrap(),
                "-o",
                dir.to_str().unwrap(),
                "--algorithm",
                "dummy-stack",
                "--preset",
                preset.to_str().unwrap(),
            ],
        );
        assert_eq!(from_preset.code, Some(1), "preset case {i} must exit 1");

        let from_flag = run_cli_in(
            &dir,
            &[
                "stack",
                mantis.to_str().unwrap(),
                "-o",
                dir.to_str().unwrap(),
                "--algorithm",
                "dummy-stack",
                "--set",
                set,
            ],
        );
        assert_eq!(
            from_preset.stderr, from_flag.stderr,
            "case {i}: a preset violation must render exactly as the equivalent --set does"
        );
    }

    // A type mismatch: the schema declares `mode` as an enum, the preset
    // hands it a bool. `--set mode=true` cannot reach this violation (every
    // enum accepts arbitrary text and fails as `UnknownVariant` instead), so
    // this case is asserted on its own variant rather than against a twin.
    let preset = write_preset(&dir, "mistyped.toml", "[stack]\nmode = true\n");
    let out = run_cli_in(
        &dir,
        &[
            "stack",
            mantis.to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--preset",
            preset.to_str().unwrap(),
        ],
    );
    assert_eq!(out.code, Some(1));
    assert!(
        out.stderr.contains("expected enum, found bool"),
        "stderr: {}",
        out.stderr
    );
}

/// Both layers a wrong TOML type can be refused at, and **which** one
/// catches each is a property of `ParamValue`'s `#[serde(untagged)]`
/// declaration, not of the preset format:
///
/// - a *quoted number* (`threshold = "0.25"`) deserializes cleanly, as
///   `ParamValue::Enum("0.25")` -- the untagged enum's `Enum(String)`
///   variant accepts every TOML string -- and is refused one layer later by
///   the schema check, as `TypeMismatch`, never as `Unparsable`;
/// - a value **no** `ParamValue` variant can hold (an array, a table, a
///   datetime) is refused by the `toml` parser itself, before any schema
///   check runs, naming the file.
///
/// Both exit 1 and both name the offending key.
#[test]
fn preset_type_error_is_refused_by_the_parser() {
    let dir = tmp_dir("preset_cli_type_error");
    let mantis = mantis_512_dir();

    let quoted = write_preset(&dir, "quoted.toml", "[stack]\nthreshold = \"0.25\"\n");
    let out = run_cli_in(
        &dir,
        &[
            "stack",
            mantis.to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--preset",
            quoted.to_str().unwrap(),
        ],
    );
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("threshold: expected float, found enum"),
        "stderr: {}",
        out.stderr
    );

    let array = write_preset(&dir, "array.toml", "[stack]\nthreshold = [1, 2]\n");
    let out = run_cli_in(
        &dir,
        &[
            "stack",
            mantis.to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--preset",
            array.to_str().unwrap(),
        ],
    );
    assert_eq!(out.code, Some(1), "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("array.toml"),
        "the parser's refusal must name the file: {}",
        out.stderr
    );
    // The parser's own rejection, before any schema check: a schema
    // violation would name the declared kind instead.
    assert!(
        !out.stderr.contains("expected float, found"),
        "a value no ParamValue variant can hold must be refused by the parser: {}",
        out.stderr
    );
}

#[test]
fn preset_supplies_the_algorithm_id() {
    let dir = tmp_dir("preset_cli_algo_from_preset");
    let mantis = mantis_512_dir();

    let preset = write_preset(&dir, "p.toml", "algo = \"dummy-stack\"\n");
    let out = run_cli_in(
        &dir,
        &[
            "stack",
            mantis.to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--preset",
            preset.to_str().unwrap(),
            "--json",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(out.success, "stderr: {}", out.stderr);
    let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("--json must parse");
    assert_eq!(report["algorithm"], "dummy-stack");

    // An explicit `--algorithm` still wins over the file.
    let out = run_cli_in(
        &dir,
        &common::push_no_gpu(vec![
            "stack",
            mantis.to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--preset",
            preset.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--json",
            "--no-align",
            "--no-crop",
        ]),
    );
    assert!(out.success, "stderr: {}", out.stderr);
    let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("--json must parse");
    assert_eq!(report["algorithm"], "laplacian");

    // Neither flag nor file: the refusal names the preset key.
    let empty = write_preset(&dir, "empty.toml", "[stack]\n");
    let out = run_cli_in(
        &dir,
        &[
            "stack",
            mantis.to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--preset",
            empty.to_str().unwrap(),
        ],
    );
    assert_eq!(out.code, Some(1));
    assert!(
        out.stderr.contains("--algorithm is required"),
        "stderr: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("preset"),
        "the refusal must name the preset key too: {}",
        out.stderr
    );
}

#[test]
fn written_preset_round_trips() {
    let mantis = mantis_512_dir();
    // Each method with a value off its default, one of them with equalization on.
    let cases: [(&str, &[&str]); 3] = [
        ("laplacian", &["levels=3"]),
        (
            "weighted-average",
            &["weight-exponent=3.5", "equalize=true"],
        ),
        ("depth-map", &["score-radius=2"]),
    ];
    for (algo, sets) in cases {
        let dir = tmp_dir(&format!("preset_cli_write_round_trip_{algo}"));
        let written = dir.join("written.toml");
        let first_dir = dir.join("first-out");
        std::fs::create_dir_all(&first_dir).unwrap();
        let first = first_dir.join("mantis-512.ffp");
        let mut args = vec![
            "stack",
            mantis.to_str().unwrap(),
            "-o",
            first_dir.to_str().unwrap(),
            "--algorithm",
            algo,
            "--write-preset",
            written.to_str().unwrap(),
            "--no-align",
            "--no-crop",
        ];
        for set in sets {
            args.extend(["--set", set]);
        }
        let out = run_cli_in(&dir, &common::push_no_gpu(args));
        assert!(out.success, "{algo}: stderr: {}", out.stderr);

        let text = std::fs::read_to_string(&written).expect("--write-preset must write the file");
        let algo_line = text.find("algo =").expect("algo must be written");
        let first_table = text.find('[').expect("a [stack] table must be written");
        assert!(
            algo_line < first_table,
            "{algo}: the scalar keys must precede the table headers:\n{text}"
        );
        for set in sets {
            let line = set.replace('=', " = ");
            assert!(text.contains(&line), "{algo}: written preset:\n{text}");
        }

        let second_dir = dir.join("second-out");
        std::fs::create_dir_all(&second_dir).unwrap();
        let second = second_dir.join("mantis-512.ffp");
        let out = run_cli_in(
            &dir,
            &common::push_no_gpu(vec![
                "stack",
                mantis.to_str().unwrap(),
                "-o",
                second_dir.to_str().unwrap(),
                "--preset",
                written.to_str().unwrap(),
                "--no-align",
                "--no-crop",
            ]),
        );
        assert!(out.success, "{algo}: stderr: {}", out.stderr);
        assert_eq!(
            read_bytes(&export_composite(&dir, &first)),
            read_bytes(&export_composite(&dir, &second)),
            "{algo}: a written preset must reproduce the composite it came from"
        );
    }
}

#[test]
fn dry_run_writes_nothing_and_succeeds() {
    let dir = tmp_dir("preset_cli_dry_run_writes_nothing");
    let output = dir.join("mantis-512.ffp");
    let status = dir.join("status.ndjson");
    let out = run_cli_in(
        &dir,
        &common::push_no_gpu(vec![
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            output.parent().unwrap().to_str().unwrap(),
            "--algorithm",
            "laplacian",
            // Two alignment algorithms are registered in this test binary
            // (`dummy-align` and the real one), so the id is explicit here
            // -- a dry run resolves the alignment half without running it.
            "--align-algorithm",
            "pyramidal-ecc",
            "--dry-run",
            "--status",
            status.to_str().unwrap(),
        ]),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);

    // Every resolved parameter, with its provenance, in `render`'s own line
    // shape -- a dry run and a real run print the same sentence for the same
    // fact. `levels` has an `auto` closure and no explicit value here.
    assert!(
        out.stdout.contains("levels = 5 (Auto)"),
        "stdout: {}",
        out.stdout
    );
    for needle in [
        "tile_size=",
        "levels=",
        "margin=",
        "tiles=",
        "in_flight_limit=",
    ] {
        assert!(out.stdout.contains(needle), "stdout: {}", out.stdout);
    }
    assert!(
        out.stdout.contains(output.to_str().unwrap()),
        "the plan must name the output it would write: {}",
        out.stdout
    );

    assert!(!output.exists(), "a dry run writes no project");
    assert!(
        !dir.join("mantis-512.ffp.partial").exists(),
        "a dry run creates no .partial"
    );
    assert!(
        !dir.join(".scratch").join("projects").exists(),
        "a dry run stages no planes"
    );
    assert!(
        !dir.join(".scratch").join("resume").exists(),
        "a dry run opens no resume ledger"
    );

    let records = read_records(&status);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["outcome"], "dry-run");
}

#[test]
fn dry_run_stops_before_alignment() {
    let dir = tmp_dir("preset_cli_dry_run_no_alignment");
    let out = run_cli_in(
        &dir,
        &common::push_no_gpu(vec![
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--align-algorithm",
            "pyramidal-ecc",
            "--dry-run",
        ]),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);

    // The two facts a dry run refuses to promise, stated rather than
    // omitted: a plan that silently left them out would read as "no crop".
    assert!(
        out.stdout.contains("crop: not computed"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("output size: not computed"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        !out.stderr.contains("align 8 frames"),
        "alignment must not run: {}",
        out.stderr
    );
}

#[test]
fn dry_run_json_is_not_a_stack_report() {
    let dir = tmp_dir("preset_cli_dry_run_json");
    let output = dir.join("mantis-512.ffp");
    let out = run_cli_in(
        &dir,
        &common::push_no_gpu(vec![
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            output.parent().unwrap().to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--align-algorithm",
            "pyramidal-ecc",
            "--tile-size",
            "512",
            "--dry-run",
            "--json",
        ]),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);

    let plan: serde_json::Value =
        serde_json::from_str(&out.stdout).expect("stdout must be one JSON document");
    assert_eq!(
        plan["dry_run"], true,
        "the marker that tells a plan from a run: {}",
        out.stdout
    );
    assert_eq!(plan["would_write"][0], output.to_str().unwrap());
    // `stack --json`'s contract is that a document on stdout means a
    // completed run. A plan must not be mistakable for one.
    for absent in ["outputs", "wall_seconds", "crop", "output_width", "frames"] {
        assert!(
            plan.get(absent).is_none(),
            "a DryRunPlan must not carry StackReport's {absent:?}: {}",
            out.stdout
        );
    }
    assert!(plan["params"].is_array(), "{}", out.stdout);
    assert_eq!(plan["tiles"].as_u64(), Some(4), "{}", out.stdout);
}

#[test]
fn dry_run_refuses_to_resume() {
    let dir = tmp_dir("preset_cli_dry_run_resume");
    let out = run_cli_in(
        &dir,
        &common::push_no_gpu(vec![
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--dry-run",
            "--resume",
        ]),
    );
    assert_eq!(out.code, Some(1), "stdout: {}", out.stdout);
    assert!(
        out.stderr.contains("--dry-run") && out.stderr.contains("--resume"),
        "stderr: {}",
        out.stderr
    );
}

/// One preset file serves both subcommands. `align` reads `align-algo` and
/// `[align]`; the `[stack]` half and the top-level `algo` are unused, and say
/// so on stderr rather than disappearing -- a `[stack]`/`[align]` typo must be
/// visible, not a quiet no-op.
#[test]
fn preset_align_uses_only_the_align_half() {
    let dir = tmp_dir("preset_cli_align_half");
    let preset = write_preset(
        &dir,
        "shared.toml",
        "algo = \"laplacian\"\nalign-algo = \"pyramidal-ecc\"\n\n\
         [stack]\nlevels = 4\n\n[align]\nmax-iterations = 40\n",
    );
    let out = run_cli_in(
        &dir,
        &[
            "align",
            mantis_512_dir().to_str().unwrap(),
            "--preset",
            preset.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("preset: [stack] table not used by align"),
        "stderr: {}",
        out.stderr
    );
    assert!(
        out.stderr.contains("preset: algo key not used by align"),
        "stderr: {}",
        out.stderr
    );

    let report: serde_json::Value = serde_json::from_str(&out.stdout).expect("--json must parse");
    assert_eq!(report["algorithm"], "pyramidal-ecc");
    let iterations = report["params"]
        .as_array()
        .expect("params must be an array")
        .iter()
        .find(|p| p["id"] == "max-iterations")
        .expect("max-iterations must be reported")
        .clone();
    assert_eq!(iterations["value"], 40);
    assert_eq!(iterations["provenance"], "Explicit");
}

fn read_records(path: &Path) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(path).expect("the status file must exist");
    text.lines()
        .map(|line| serde_json::from_str(line).expect("every status line must be one JSON object"))
        .collect()
}

#[test]
fn status_record_names_identity_outcome_and_path() {
    let dir = tmp_dir("preset_cli_status_completed");
    let mantis = mantis_512_dir();
    let status = dir.join("status.ndjson");
    let output = dir.join("mantis-512.ffp");
    let out = run_cli_in(
        &dir,
        &[
            "stack",
            mantis.to_str().unwrap(),
            "-o",
            output.parent().unwrap().to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--status",
            status.to_str().unwrap(),
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(out.success, "stderr: {}", out.stderr);

    let records = read_records(&status);
    assert_eq!(
        records.len(),
        1,
        "exactly one record per stack: {records:?}"
    );
    let record = &records[0];
    assert!(record.get("format").is_none());
    assert_eq!(record["identity"], mantis.to_str().unwrap());
    assert_eq!(record["command"], "stack");
    assert_eq!(record["outcome"], "completed");
    assert_eq!(record["algorithm"], "dummy-stack");
    assert_eq!(record["outputs"][0], output.to_str().unwrap());
    assert_eq!(record["error"], serde_json::Value::Null);
    assert!(
        record["wall_seconds"].as_f64().expect("a duration") >= 0.0,
        "{record:?}"
    );
}

#[test]
fn status_record_written_on_refusal() {
    let dir = tmp_dir("preset_cli_status_refusal");
    let status = dir.join("status.ndjson");
    let out = run_cli_in(
        &dir,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--set",
            "threshold=9.5",
            "--status",
            status.to_str().unwrap(),
        ],
    );
    assert_eq!(out.code, Some(1));

    let records = read_records(&status);
    assert_eq!(
        records.len(),
        1,
        "a failure is a record, never a missing line: {records:?}"
    );
    assert_eq!(records[0]["outcome"], "failed");
    assert!(
        records[0]["error"]
            .as_str()
            .expect("the refusal text")
            .contains("threshold"),
        "{:?}",
        records[0]
    );
    assert_eq!(records[0]["outputs"].as_array().expect("an array").len(), 0);
}

#[test]
fn status_stream_is_separate_from_progress_and_stdout() {
    let dir = tmp_dir("preset_cli_status_streams");
    let status = dir.join("status.ndjson");
    let out = run_cli_in(
        &dir,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--status",
            status.to_str().unwrap(),
            "--json",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(out.success, "stderr: {}", out.stderr);

    // stdout: `--json`'s own pretty document, and only that.
    let report: serde_json::Value =
        serde_json::from_str(&out.stdout).expect("stdout must be one JSON document");
    assert!(report["outputs"].is_array(), "stdout: {}", out.stdout);

    // stderr: progress output, never a record.
    assert!(out.stderr.contains("fuse "), "stderr: {}", out.stderr);
    assert!(
        !out.stderr.contains("\"outcome\""),
        "the status record must not leak onto stderr: {}",
        out.stderr
    );

    // the status file: NDJSON only -- one line, no progress text.
    let text = std::fs::read_to_string(&status).expect("the status file must exist");
    assert_eq!(text.lines().count(), 1, "status file: {text}");
    assert!(text.ends_with('\n'), "each record is newline-terminated");
    assert!(!text.contains("fuse "), "status file: {text}");
    let record: serde_json::Value =
        serde_json::from_str(text.trim_end()).expect("the record must parse");
    assert_eq!(record["outcome"], "completed");
}

/// Only values the user actually chose are frozen. `levels` left to
/// `auto_levels` stays live, so the same preset reused on a differently-sized
/// stack still resolves its own depth instead of silently pinning this
/// fixture's.
#[test]
fn written_preset_holds_explicit_values_only() {
    let dir = tmp_dir("preset_cli_write_explicit_only");
    let written = dir.join("written.toml");
    let out = run_cli_in(
        &dir,
        &common::push_no_gpu(vec![
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--write-preset",
            written.to_str().unwrap(),
            "--no-align",
            "--no-crop",
        ]),
    );
    assert!(out.success, "stderr: {}", out.stderr);

    let text = std::fs::read_to_string(&written).expect("--write-preset must write the file");
    assert!(
        text.contains("algo = \"laplacian\""),
        "written preset:\n{text}"
    );
    assert!(
        !text.contains("levels"),
        "an Auto-resolved value must not be frozen into the preset:\n{text}"
    );
}

/// A preset naming `depth-map` resolves that algorithm's own schema: an
/// explicit value is reported as such and an untouched one keeps its default.
#[test]
fn a_preset_naming_depth_map_runs_its_schema() {
    let dir = tmp_dir("preset_cli_depth_map_schema");
    let preset = write_preset(
        &dir,
        "p.toml",
        "algo = \"depth-map\"\n[stack]\nscore-radius = 9\n",
    );
    let out = run_cli_in(
        &dir,
        &common::push_no_gpu(vec![
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--preset",
            preset.to_str().unwrap(),
            "--align-algorithm",
            "pyramidal-ecc",
            "--dry-run",
        ]),
    );
    assert_eq!(out.code, Some(0), "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("score-radius = 9 (Explicit)"),
        "stdout: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("smoothing-radius = 8"),
        "stdout: {}",
        out.stdout
    );
}
