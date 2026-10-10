// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level coverage of `stack`'s grouping: the printed proposal, the
//! grouping file and its three rejection modes, the multi-group run's four
//! observable contracts, and the superset property -- a lone-group proposal
//! runs identically to an ungrouped run.
//!
//! Every tree here is **built by the test**: no committed fixture frame
//! carries a capture-time tag, so a tree copied from `fixtures/` would prove
//! nothing about the time signal. The frames come from
//! `focusforge_core::io::test_frames`, reachable here because
//! `crates/cli/Cargo.toml`'s dev-dependency already enables
//! `focusforge-core/test-algorithms`.
//!
//! `dummy-stack` everywhere a composite is actually produced: this file is
//! about the grouping seam, not about fusion numerics, and `laplacian` on a
//! real fixture costs ~25 s per run.
//!
//! Every test isolates `.scratch/` with its own per-process temp directory
//! and `.current_dir(cwd)`: `stack`'s scratch tree is cwd-relative, so
//! concurrent test runs sharing one `target/` -- or two tests in the same
//! run under cargo's default parallel execution -- must never share a
//! working directory either. The working directory a spawned process runs
//! in is never a directory a test also feeds in as input, names as `-o`, or
//! enumerates; it holds only the test's own named children and `.scratch/`.
//! A test that uses one directory as both input and `-o` keeps it as a
//! fixed-name child of its cwd, so its basename is stable.

mod common;
#[path = "../../core/tests/common/raw_dng.rs"]
mod raw_dng;

use std::path::{Path, PathBuf};
use std::process::Command;

use focusforge_core::io::TiffStack;
use focusforge_core::io::test_frames::write_frame_with_capture_time;
use raw_dng::{DngSpec, write_dng};

use common::CliOutput;

fn bin() -> Command {
    common::cli_bin("groups_cli")
}

fn run_cli_in(dir: &Path, args: &[&str]) -> CliOutput {
    common::run_in(&mut bin(), dir, args)
}

fn frame(dir: &Path, name: &str, when: Option<&str>) {
    std::fs::create_dir_all(dir).unwrap();
    write_frame_with_capture_time(&dir.join(name), 8, 8, when).expect("test frame must write");
}

/// A flat card dump holding two subjects twenty minutes apart.
fn card_dump(name: &str) -> common::TmpDir {
    let dir = common::tmp_dir(name);
    for (file, when) in [
        ("IMG_9801.tif", "2026:08:29 18:42:41"),
        ("IMG_9802.tif", "2026:08:29 18:42:42"),
        ("IMG_9910.tif", "2026:08:29 19:02:55"),
        ("IMG_9911.tif", "2026:08:29 19:02:56"),
    ] {
        frame(&dir, file, Some(when));
    }
    dir
}

fn read_whole(path: &Path) -> Vec<u16> {
    let stack = TiffStack::open_files(&[path.to_path_buf()]).expect("output must reopen");
    let (w, h) = stack.dimensions();
    stack
        .read_tile(
            0,
            focusforge_core::io::Rect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            },
        )
        .expect("read must succeed")
        .samples
}

// --- the proposal ----------------------------------------------------------

#[test]
fn grouping_is_printed_before_work_starts() {
    let dump = card_dump("groups_cli_printed_first");
    let out_dir = common::tmp_dir("groups_cli_printed_first_out");
    let cwd = common::tmp_dir("groups_cli_printed_first_cwd");
    let result = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);

    let grouping_at = result
        .stderr
        .find("grouping ")
        .expect("the proposal must be printed on stderr");
    assert!(
        result.stderr.contains("IMG_9801-9802"),
        "the proposal must name each group: {}",
        result.stderr
    );
    // The first unit of work announces itself with a `fuse ...` progress
    // line; the proposal must precede it.
    let first_work = result
        .stderr
        .find("fuse ")
        .expect("a progress line must be printed for a real run");
    assert!(
        grouping_at < first_work,
        "the grouping must be printed before any work starts:\n{}",
        result.stderr
    );
}

/// `--quiet` suppresses the group proposal; `--no-progress` is separate.
#[test]
fn grouping_is_suppressed_under_quiet() {
    let dump = card_dump("groups_cli_quiet");
    let out_dir = common::tmp_dir("groups_cli_quiet_out");
    let cwd = common::tmp_dir("groups_cli_quiet_cwd");
    let result = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--quiet",
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);
    assert!(
        !result.stderr.contains("grouping "),
        "--quiet must remove the proposal: {:?}",
        result.stderr
    );
    assert!(
        result.stderr.contains("fuse "),
        "--quiet must not remove progress: {:?}",
        result.stderr
    );
}

#[test]
fn only_skips_are_suppressed_under_quiet() {
    let dump = card_dump("groups_cli_only_quiet");
    let cwd = common::tmp_dir("groups_cli_only_quiet_cwd");
    for (quiet, name) in [(false, "normal"), (true, "quiet")] {
        let out = cwd.join(name);
        std::fs::create_dir(&out).unwrap();
        let mut args = vec![
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--only",
            "IMG_9801-9802",
        ];
        if quiet {
            args.push("--quiet");
        }
        let result = run_cli_in(&cwd, &args);
        assert!(result.success, "stderr: {}", result.stderr);
        assert_eq!(
            result
                .stderr
                .contains("skipped (not named by --only): IMG_9910-9911"),
            !quiet,
            "stderr: {}",
            result.stderr
        );
    }
}

// --- the superset property -------------------------------------------------

/// A single stack runs identically to ungrouped frames -- compared byte for
/// byte against a composite produced from the same frames named
/// individually, the path that never went through grouping at all.
#[test]
fn single_stack_directory_matches_the_plain_invocation() {
    let cwd = common::tmp_dir("groups_cli_single_stack");
    // Input and `-o` are deliberately the same directory here -- the whole
    // property under test -- so it is a fixed-name child of the cwd, never
    // the cwd itself.
    let dir = cwd.join("groups_cli_single_stack");
    std::fs::create_dir_all(&dir).unwrap();
    for (file, when) in [
        ("00.tif", "2026:08:29 18:42:41"),
        ("01.tif", "2026:08:29 18:42:42"),
        ("02.tif", "2026:08:29 18:42:43"),
    ] {
        frame(&dir, file, Some(when));
    }
    // The project is named after the **group**, and this directory's frames
    // share no basename range, so the naming rule falls back to the
    // directory's own basename.
    let grouped = dir.join("groups_cli_single_stack.ffp");
    let result = run_cli_in(
        &cwd,
        &[
            "stack",
            dir.to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--json",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);

    let report: serde_json::Value =
        serde_json::from_str(&result.stdout).expect("--json must parse as one document");
    assert_eq!(report["frame_count"], 3);
    assert_eq!(report["input"], dir.to_str().unwrap());
    assert_eq!(
        report["outputs"][0],
        grouped.to_str().unwrap(),
        "a one-group run writes its project into -o exactly as a many-group run does"
    );
    assert!(
        report.get("groups").is_none(),
        "a one-group run's document is a StackReport, never the wrapper: {report}"
    );

    // The library oracle: the same three frames, fused directly.
    let plain = dir.join("plain.tif");
    let stack =
        TiffStack::open(&dir).expect("the same directory must open through the plain enumeration");
    let algo = focusforge_core::registry::find_stack("dummy-stack").expect("registered");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let ctx = focusforge_core::params::StackContext::new(stack.len(), width, height, spp);
    let resolved = focusforge_core::params::resolve(
        algo.params(),
        &focusforge_core::params::ParamValues::new(),
        &ctx,
    )
    .expect("resolve");
    let values = focusforge_core::params::resolved_values(&resolved);
    // The scheduler's own in-process scratch, distinct from `.scratch/` the
    // spawned CLI process writes -- a child of this test's cwd, not the
    // shared `target/`.
    let options = focusforge_core::tile::TileSchedulerOptions::new(cwd.join("scratch"));
    let scheduler = focusforge_core::tile::TileScheduler::new(&stack, ctx, spp, &options)
        .expect("scheduler must build");
    let consumer = focusforge_core::fuse::FusionConsumer::create(
        algo,
        values,
        &plain,
        None,
        focusforge_core::fuse::FusionGeometry {
            width,
            height,
            samples_per_pixel: spp,
            frame_count: stack.len() as u32,
            crop: None,
            description: None,
        },
    )
    .expect("consumer must create");
    scheduler
        .run(&focusforge_core::tile::NoopPreprocess, &consumer)
        .expect("run must succeed");
    consumer.finish().expect("finish must succeed");

    assert_eq!(
        read_whole(&export_composite(&cwd, &grouped)),
        read_whole(&plain),
        "a grouped one-stack run must produce the same composite, byte for byte"
    );
}

/// The composite of `project`, exported beside it -- the comparison a
/// container makes necessary: what must be preserved is the payload, never
/// the envelope.
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

// --- the grouping file -----------------------------------------------------

#[test]
fn write_groups_then_groups_round_trips() {
    let dump = card_dump("groups_cli_round_trip");
    let out_dir = common::tmp_dir("groups_cli_round_trip_out");
    let cwd = common::tmp_dir("groups_cli_round_trip_cwd");
    let file = out_dir.join("groups.toml");
    let first = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--write-groups",
            file.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert!(first.success, "stderr: {}", first.stderr);

    let text = std::fs::read_to_string(&file).expect("--write-groups must write the file");
    assert!(!text.contains("format ="), "{text}");
    assert!(text.contains("[[group]]"), "{text}");
    assert!(text.contains("IMG_9801.tif"), "{text}");

    let second = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--groups",
            file.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert!(second.success, "stderr: {}", second.stderr);

    let proposed = grouping_lines(&first.stderr);
    let from_file = grouping_lines(&second.stderr);
    assert_eq!(
        proposed[1..],
        from_file[1..],
        "feeding back the unedited file must reproduce the proposal exactly"
    );
    assert!(
        from_file[0].contains("from "),
        "the header line names the file it came from: {}",
        from_file[0]
    );
}

#[test]
fn mixed_raw_tiff_groups_round_trip_with_names_order_and_count() {
    let input = common::tmp_dir("groups_cli_mixed_round_trip_input");
    let out_dir = common::tmp_dir("groups_cli_mixed_round_trip_out");
    let cwd = common::tmp_dir("groups_cli_mixed_round_trip_cwd");
    let raw = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![64; 64]),
        true,
    );
    std::fs::copy(raw.path(), input.join("00.dng")).expect("copy RAW frame");
    frame(&input, "01.tif", Some("2026:08:29 18:42:41"));
    let groups = out_dir.join("mixed.toml");
    let first = run_cli_in(
        &cwd,
        &[
            "stack",
            input.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--write-groups",
            groups.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert!(first.success, "stderr: {}", first.stderr);
    let group_text = std::fs::read_to_string(&groups).expect("written groups");
    assert_eq!(group_text.matches("[[group]]").count(), 2, "{group_text}");
    assert!(group_text.contains("name = \"00\""), "{group_text}");
    assert!(group_text.contains("name = \"01\""), "{group_text}");
    assert!(
        group_text.find("00.dng").expect("RAW frame")
            < group_text.find("01.tif").expect("TIFF frame"),
        "RAW partition must remain first: {group_text}"
    );

    let second = run_cli_in(
        &cwd,
        &[
            "stack",
            input.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--groups",
            groups.to_str().unwrap(),
            "--dry-run",
        ],
    );
    assert!(second.success, "stderr: {}", second.stderr);
    let proposed = grouping_lines(&first.stderr);
    let from_file = grouping_lines(&second.stderr);
    assert_eq!(proposed[1..], from_file[1..]);
    assert_eq!(proposed.len(), 3, "one header and two groups: {proposed:?}");
    assert!(proposed[1].contains("raw/"), "{proposed:?}");
    assert!(proposed[2].contains("tiff/"), "{proposed:?}");
}

/// The proposal block of a captured stderr: the `grouping ` line and every
/// indented line under it.
fn grouping_lines(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .skip_while(|l| !l.starts_with("grouping "))
        .take_while(|l| l.starts_with("grouping ") || l.starts_with("  "))
        .map(str::to_string)
        .collect()
}

#[test]
fn groups_file_with_a_format_key_is_refused() {
    let dump = card_dump("groups_cli_format_key");
    let out_dir = common::tmp_dir("groups_cli_format_key_out");
    let cwd = common::tmp_dir("groups_cli_format_key_cwd");
    let file = out_dir.join("groups.toml");
    std::fs::write(
        &file,
        "format = 1\n\n[[group]]\nname = \"everything\"\nframes = [\"IMG_9801.tif\", \"IMG_9802.tif\", \"IMG_9910.tif\", \"IMG_9911.tif\"]\n",
    )
    .unwrap();

    let result = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--groups",
            file.to_str().unwrap(),
        ],
    );
    assert!(!result.success);
    assert_eq!(result.code, Some(1));
    assert!(
        result.stderr.contains(file.to_str().unwrap()),
        "{}",
        result.stderr
    );
    assert!(result.stderr.contains("format"), "{}", result.stderr);
}

#[test]
fn groups_file_overrides_the_proposal() {
    let dump = card_dump("groups_cli_override");
    let out_dir = common::tmp_dir("groups_cli_override_out");
    let cwd = common::tmp_dir("groups_cli_override_cwd");
    let file = out_dir.join("groups.toml");
    // One group holding all four frames -- the opposite of what the
    // heuristic proposes for this tree.
    std::fs::write(
        &file,
        "[[group]]\nname = \"everything\"\nframes = [\"IMG_9801.tif\", \
         \"IMG_9802.tif\", \"IMG_9910.tif\", \"IMG_9911.tif\"]\n",
    )
    .unwrap();

    let output = out_dir.join("everything.ffp");
    let result = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--groups",
            file.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);
    assert!(
        result.stderr.contains("everything") && result.stderr.contains("1 group ("),
        "the file's grouping must be obeyed and printed: {}",
        result.stderr
    );
    let report: serde_json::Value = serde_json::from_str(&result.stdout).expect("--json");
    assert_eq!(report["frame_count"], 4, "the file's one group ran as one");
    assert!(output.is_file());
}

#[test]
fn groups_file_with_a_missing_frame_is_refused() {
    let dump = card_dump("groups_cli_missing_frame");
    let out_dir = common::tmp_dir("groups_cli_missing_frame_out");
    let cwd = common::tmp_dir("groups_cli_missing_frame_cwd");
    let file = out_dir.join("groups.toml");
    std::fs::write(
        &file,
        "[[group]]\nname = \"a\"\nframes = [\"IMG_9801.tif\", \"IMG_9802.tif\", \
         \"IMG_9910.tif\", \"IMG_9911.tif\", \"IMG_0000.tif\"]\n",
    )
    .unwrap();

    let result = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--groups",
            file.to_str().unwrap(),
        ],
    );
    assert!(!result.success);
    assert_eq!(result.code, Some(1));
    assert!(
        result.stderr.contains("IMG_0000.tif"),
        "the refusal must name the frame: {}",
        result.stderr
    );
}

#[test]
fn groups_file_duplicate_or_orphan_frame_is_refused() {
    let dump = card_dump("groups_cli_duplicate_frame");
    let out_dir = common::tmp_dir("groups_cli_duplicate_frame_out");
    let cwd = common::tmp_dir("groups_cli_duplicate_frame_cwd");

    let duplicate = out_dir.join("duplicate.toml");
    std::fs::write(
        &duplicate,
        "[[group]]\nname = \"a\"\nframes = [\"IMG_9801.tif\", \"IMG_9802.tif\", \
         \"IMG_9910.tif\", \"IMG_9911.tif\"]\n\n[[group]]\nname = \"b\"\nframes = \
         [\"IMG_9801.tif\"]\n",
    )
    .unwrap();
    let result = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--groups",
            duplicate.to_str().unwrap(),
        ],
    );
    assert!(!result.success);
    assert!(
        result.stderr.contains("IMG_9801.tif") && result.stderr.contains("twice"),
        "the refusal must name the doubly-listed frame: {}",
        result.stderr
    );

    let orphan = out_dir.join("orphan.toml");
    std::fs::write(
        &orphan,
        "[[group]]\nname = \"a\"\nframes = [\"IMG_9801.tif\", \"IMG_9802.tif\", \
         \"IMG_9910.tif\"]\n",
    )
    .unwrap();
    let result = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--groups",
            orphan.to_str().unwrap(),
        ],
    );
    assert!(!result.success);
    assert!(
        result.stderr.contains("IMG_9911.tif") && result.stderr.contains("in no group"),
        "the refusal must name the ungrouped frame: {}",
        result.stderr
    );
}

#[test]
fn empty_directory_fails_clearly() {
    let cwd = common::tmp_dir("groups_cli_empty");
    // Input and `-o` are deliberately the same directory here, so it is a
    // fixed-name child of the cwd, never the cwd itself.
    let dir = cwd.join("groups_cli_empty");
    std::fs::create_dir_all(dir.join("nothing-here")).unwrap();
    let result = run_cli_in(
        &cwd,
        &[
            "stack",
            dir.to_str().unwrap(),
            "-o",
            dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
        ],
    );
    assert!(!result.success, "stdout: {}", result.stdout);
    assert_eq!(result.code, Some(1));
    assert!(
        result.stderr.contains("no TIFF frames") || result.stderr.contains("no frames"),
        "a tree with no frames must fail clearly rather than report success: {}",
        result.stderr
    );
    assert!(result.stdout.is_empty(), "no document for a failed run");
}

// --- the multi-group run ---------------------------------------------------

#[test]
fn status_and_manifest_have_one_entry_per_group() {
    let dump = card_dump("groups_cli_status_manifest");
    let out_dir = common::tmp_dir("groups_cli_status_manifest_out");
    let cwd = common::tmp_dir("groups_cli_status_manifest_cwd");
    let status = out_dir.join("status.ndjson");
    let result = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--status",
            status.to_str().unwrap(),
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);

    let records: Vec<serde_json::Value> = std::fs::read_to_string(&status)
        .expect("--status must be written")
        .lines()
        .map(|l| serde_json::from_str(l).expect("each line must be one JSON record"))
        .collect();
    assert_eq!(records.len(), 2, "one record per group: {records:?}");
    assert_eq!(records[0]["identity"], "IMG_9801-9802");
    assert_eq!(records[1]["identity"], "IMG_9910-9911");
    for record in &records {
        assert_eq!(record["outcome"], "completed");
        assert_eq!(record["command"], "stack");
    }

    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(out_dir.join("manifest.json"))
            .expect("a multi-group run writes one manifest into the -o directory"),
    )
    .expect("the manifest must parse");
    assert!(manifest.get("format").is_none());
    assert_eq!(manifest["groups"].as_array().unwrap().len(), 2);
    assert_eq!(manifest["groups"][0]["name"], "IMG_9801-9802");
    assert_eq!(manifest["groups"][0]["frame_count"], 2);
    assert_eq!(manifest["groups"][0]["outcome"], "completed");
    assert!(
        out_dir.join("IMG_9801-9802.ffp").is_file() && out_dir.join("IMG_9910-9911.ffp").is_file(),
        "each group's project is named after the group"
    );
}

/// A failed stack is a **listed failure**, never a missing file: the run
/// carries on, records the failure, and exits 1.
#[test]
fn a_failing_group_does_not_abort_the_run() {
    let dump = card_dump("groups_cli_failing_group");
    // A frame of a different size in the second group: the strict
    // uniformity guard refuses that group and only that group.
    write_frame_with_capture_time(
        &dump.join("IMG_9912.tif"),
        16,
        8,
        Some("2026:08:29 19:02:57"),
    )
    .unwrap();
    let out_dir = common::tmp_dir("groups_cli_failing_group_out");
    let cwd = common::tmp_dir("groups_cli_failing_group_cwd");
    let status = out_dir.join("status.ndjson");
    let result = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--status",
            status.to_str().unwrap(),
        ],
    );
    assert!(!result.success, "one group failed, so the run exits 1");
    assert_eq!(result.code, Some(1));
    assert!(
        out_dir.join("IMG_9801-9802.ffp").is_file(),
        "the healthy group must still have been written"
    );

    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(out_dir.join("manifest.json"))
            .expect("a manifest must be written"),
    )
    .unwrap();
    let groups = manifest["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "every attempted group is listed");
    assert_eq!(groups[0]["outcome"], "completed");
    assert_eq!(groups[1]["outcome"], "failed");
    assert!(
        groups[1]["error"].as_str().is_some_and(|e| !e.is_empty()),
        "the failure is named, not merely flagged: {}",
        groups[1]
    );

    let records = std::fs::read_to_string(&status).unwrap();
    assert_eq!(records.lines().count(), 2, "one record per attempted group");
}

/// A schema-level refusal fires **once**, before the first group runs.
#[test]
fn a_bad_set_refuses_before_any_group_runs() {
    let dump = card_dump("groups_cli_bad_set");
    let out_dir = common::tmp_dir("groups_cli_bad_set_out");
    let cwd = common::tmp_dir("groups_cli_bad_set_cwd");
    let status = out_dir.join("status.ndjson");
    let result = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--set",
            "no-such-parameter=1",
            "--no-align",
            "--status",
            status.to_str().unwrap(),
        ],
    );
    assert!(!result.success);
    assert_eq!(result.code, Some(1));
    assert!(
        result.stderr.contains("no-such-parameter"),
        "stderr: {}",
        result.stderr
    );
    assert!(
        !result.stderr.contains("grouping "),
        "a schema refusal fires before the grouping is even computed: {}",
        result.stderr
    );
    let records = std::fs::read_to_string(&status).expect("one record even for a refusal");
    assert_eq!(
        records.lines().count(),
        1,
        "exactly one refusal, not one per group: {records}"
    );
}

#[test]
fn json_shape_is_unchanged_for_one_group_and_wrapped_for_many() {
    let dump = card_dump("groups_cli_json_shape");
    let out_dir = common::tmp_dir("groups_cli_json_shape_out");
    let cwd = common::tmp_dir("groups_cli_json_shape_cwd");
    let many = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--json",
        ],
    );
    assert!(many.success, "stderr: {}", many.stderr);
    let wrapped: serde_json::Value =
        serde_json::from_str(&many.stdout).expect("one document, never N concatenated");
    assert!(wrapped.get("format").is_none());
    let groups = wrapped["groups"].as_array().expect("a groups array");
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0]["frame_count"], 2);
    assert!(
        wrapped.get("frame_count").is_none(),
        "the wrapper is not itself a StackReport: {wrapped}"
    );

    // Input and `-o` are deliberately the same directory here too, so it is
    // a fixed-name child of the shared cwd, never the cwd itself.
    let single = cwd.join("groups_cli_json_shape_single");
    frame(&single, "00.tif", Some("2026:08:29 18:42:41"));
    frame(&single, "01.tif", Some("2026:08:29 18:42:42"));
    let one = run_cli_in(
        &cwd,
        &[
            "stack",
            single.to_str().unwrap(),
            "-o",
            single.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--json",
        ],
    );
    assert!(one.success, "stderr: {}", one.stderr);
    let report: serde_json::Value = serde_json::from_str(&one.stdout).unwrap();
    assert_eq!(report["frame_count"], 2);
    assert!(
        report.get("groups").is_none() && report.get("format").is_none(),
        "a one-group run's document is exactly today's StackReport: {report}"
    );
}

/// `-o` is a project-writing **directory** for every proposal size, and its
/// refusal fires before the grouping is computed.
#[test]
fn an_output_file_is_refused_by_name() {
    let dump = card_dump("groups_cli_output_placement");
    let out_dir = common::tmp_dir("groups_cli_output_placement_out");
    let cwd = common::tmp_dir("groups_cli_output_placement_cwd");

    let as_file = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out_dir.join("out.tif").to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
        ],
    );
    assert!(!as_file.success);
    assert_eq!(as_file.code, Some(1), "never clap's exit 2");
    assert!(
        as_file.stderr.contains("existing directory"),
        "the refusal must say what -o has to be: {}",
        as_file.stderr
    );
    assert!(
        as_file.stderr.contains("focusforge-cli export"),
        "an -o <file> refusal must name the replacement: {}",
        as_file.stderr
    );
    assert!(
        !as_file.stderr.contains("grouping "),
        "the refusal fires before the grouping is computed: {}",
        as_file.stderr
    );
}

/// The committed fixture tree: one group, on the directory signal alone (no
/// fixture frame carries a date tag), collected into `-o` as one project --
/// and a manifest too, single-group included.
#[test]
fn a_one_group_run_writes_a_manifest_too() {
    let out_dir = common::tmp_dir("groups_cli_fixture_single");
    let cwd = common::tmp_dir("groups_cli_fixture_single_cwd");
    let result = run_cli_in(
        &cwd,
        &[
            "stack",
            common::fixtures_dir().join("mantis-512").to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);
    assert!(
        result.stderr.contains("1 group ("),
        "a `frames/` layout is one stack, `golden/` beside it is not a second: {}",
        result.stderr
    );
    assert!(
        out_dir.join("mantis-512.ffp").is_file(),
        "the project is named after the group, which falls back to the directory basename"
    );
    assert!(
        std::fs::read_dir(&*out_dir)
            .unwrap()
            .flatten()
            .all(|e| e.path().extension().is_none_or(|x| x != "tif")),
        "a run writes no image at all"
    );
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(out_dir.join("manifest.json"))
            .expect("a one-group run writes a manifest too"),
    )
    .expect("the manifest must parse");
    assert!(manifest.get("format").is_none());
    let groups = manifest["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "exactly one entry for the one group");
    assert_eq!(groups[0]["outcome"], "completed");
}
