// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level coverage of the stage cache's CLI surface -- two-run
//! reuse, the `--no-cache` bypass, `cache`, `--only` subset selection, the
//! identity `review` prints, and the unknown-`--only` refusal.
//!
//! Every test isolates `.scratch/` with its own per-process temp directory
//! and `.current_dir(dir)`: `.scratch/stage-cache/` is cwd-relative, so two
//! concurrent test runs sharing one `target/` must never share a cache
//! directory either.
//!
//! `--algorithm laplacian --align-algorithm dummy-align` is the one recipe here that
//! pays for a real fusion (a few seconds over `fixtures/mantis-512`);
//! `dummy-align` reaches [`AlignStatus::Converged`] by construction, so no
//! Default fallback keeps the run from refusing. Every other test drives
//! `dummy-stack --no-align`, milliseconds only.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use focusforge_core::io::test_frames::write_frame_with_capture_time;
use focusforge_core::job::{JobRecord, ResumeStore, StackFingerprint};
use focusforge_core::params::ParamValues;

use common::{CliOutput, mantis_512_dir, tmp_dir};

fn bin() -> Command {
    common::cli_bin("cache_cli")
}

fn run_cli_in(dir: &Path, args: &[&str]) -> CliOutput {
    common::run_in(&mut bin(), dir, args)
}

fn frame(dir: &Path, name: &str, when: Option<&str>) {
    std::fs::create_dir_all(dir).unwrap();
    write_frame_with_capture_time(&dir.join(name), 8, 8, when).expect("test frame must write");
}

/// Three subjects, twenty minutes apart -- three groups on the capture-time
/// signal alone, the exact tree `crates/cli/tests/review_cli.rs`'s
/// `three_group_dump` builds, named `IMG_9801-9802` / `IMG_9910-9911` /
/// `IMG_9950-9951` by the grouping rule.
fn three_group_dump(dir: &Path) -> PathBuf {
    for (file, when) in [
        ("IMG_9801.tif", "2026:08:29 18:42:41"),
        ("IMG_9802.tif", "2026:08:29 18:42:42"),
        ("IMG_9910.tif", "2026:08:29 19:02:55"),
        ("IMG_9911.tif", "2026:08:29 19:02:56"),
        ("IMG_9950.tif", "2026:08:29 19:22:10"),
        ("IMG_9951.tif", "2026:08:29 19:22:11"),
    ] {
        frame(dir, file, Some(when));
    }
    dir.to_path_buf()
}

/// The composite of `project`, exported to `<project>.exported.tif` inside
/// `cwd`. Two containers of the same stack are not byte-identical -- they
/// carry different `created_at` stamps -- so a byte comparison always goes
/// through `export`, `job_cli.rs`'s `export_composite` idiom.
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

fn read_pixels(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("output file must be readable")
}

/// The number of entries the align stage cache holds under
/// `<cwd>/.scratch/stage-cache/align/` -- `crates/core/src/job/stage_cache.rs`'s
/// store path, `<scratch_base>/stage-cache/<stage>/<digest16>.json`.
fn cache_entry_count(cwd: &Path) -> usize {
    std::fs::read_dir(cwd.join(".scratch").join("stage-cache").join("align"))
        .into_iter()
        .flatten()
        .flatten()
        .count()
}

/// Every staged project directory under `<cwd>/.scratch/projects/` --
/// `job_cli.rs`'s `staged_composites`, generalized to the directory rather
/// than the one `composite.tif` inside it, since this file needs to prove
/// the directory itself survives a `cache`.
fn staged_project_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(cwd.join(".scratch").join("projects"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    found.sort();
    found
}

/// Every file under `dir`, recursively, as paths relative to it --
/// `crates/cli/tests/project_cli.rs`'s `walk`.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path.strip_prefix(dir).unwrap().to_path_buf());
            }
        }
    }
    out.sort();
    out
}

/// Leaves a resume ledger on disk without ever discarding it -- real
/// product code (`ResumeStore::begin`), never a hand-planted file --
/// simulating a run that was cancelled rather than completed: a cancel
/// leaves its resumable state on disk. Returns the ledger's own directory.
fn leave_a_cancelled_run_ledger(cwd: &Path) -> PathBuf {
    let record = JobRecord {
        fingerprint: StackFingerprint { frames: Vec::new() },
        width: 8,
        height: 8,
        samples_per_pixel: 3,
        frame_count: 1,
        tile_size: 64,
        levels: 1,
        stack_algorithm: "dummy-stack".to_string(),
        stack_params: ParamValues::new(),
        backend: "cpu".to_string(),
        align: None,
        crop: None,
        equalization: None,
        composite_output: PathBuf::from("leftover.ffp"),
        zmap_output: None,
    };
    let store =
        ResumeStore::open(&cwd.join(".scratch")).expect("resume store must open under .scratch");
    let ledger = store
        .begin(&record)
        .expect("begin must start a fresh ledger");
    ledger.dir().to_path_buf()
}

// --- reuse ------------------------------------------------------------

/// Two `stack` runs over `fixtures/mantis-512` differing only in a fusion
/// parameter (`--set levels=…`): the second reuses the cached alignment
/// (`align_cached: true`, no computed `align … frames` stage line), and the
/// reused alignment fuses to a composite byte-identical to
/// what a cold `--no-cache` recompute of the very same parameters produces
/// -- the correctness half of "reuses the cached alignment", since a cache
/// hit that merely runs fast but fuses wrong is still a bug.
#[test]
fn fusion_parameter_change_reuses_alignment() {
    let cwd = tmp_dir("cache_cli_fusion_reuse");
    let input = mantis_512_dir();
    let out_a = cwd.join("out_a");
    let out_b = cwd.join("out_b");
    let out_c = cwd.join("out_c");
    for dir in [&out_a, &out_b, &out_c] {
        std::fs::create_dir_all(dir).unwrap();
    }

    let a = run_cli_in(
        &cwd,
        &common::push_no_gpu(vec![
            "stack",
            input.to_str().unwrap(),
            "-o",
            out_a.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--align-algorithm",
            "dummy-align",
            "--set",
            "levels=3",
            "--json",
        ]),
    );
    assert!(a.success, "stderr: {}", a.stderr);
    let a_report: serde_json::Value = serde_json::from_str(&a.stdout).expect("--json");
    assert_eq!(
        a_report["align_cached"], false,
        "the first run must compute, not reuse"
    );
    assert!(
        a.stderr.lines().any(|l| l == "align 8 frames"),
        "stderr: {}",
        a.stderr
    );

    let b = run_cli_in(
        &cwd,
        &common::push_no_gpu(vec![
            "stack",
            input.to_str().unwrap(),
            "-o",
            out_b.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--align-algorithm",
            "dummy-align",
            "--set",
            "levels=5",
            "--json",
        ]),
    );
    assert!(b.success, "stderr: {}", b.stderr);
    let b_report: serde_json::Value = serde_json::from_str(&b.stdout).expect("--json");
    assert_eq!(
        b_report["align_cached"], true,
        "changing only a fusion parameter must reuse the cached alignment: {b_report}"
    );
    assert!(
        !b.stderr.lines().any(|l| l == "align 8 frames"),
        "a cache hit must not print the computed-alignment stage line: {}",
        b.stderr
    );
    assert!(
        b.stderr
            .contains("align 8 frames (reused from the stage cache)"),
        "a cache hit must state the reuse, not merely omit the other line: {}",
        b.stderr
    );

    // Control: the same parameters as `b`, forced to a cold recompute.
    let c = run_cli_in(
        &cwd,
        &common::push_no_gpu(vec![
            "stack",
            input.to_str().unwrap(),
            "-o",
            out_c.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--align-algorithm",
            "dummy-align",
            "--set",
            "levels=5",
            "--no-cache",
            "--json",
        ]),
    );
    assert!(c.success, "stderr: {}", c.stderr);
    let c_report: serde_json::Value = serde_json::from_str(&c.stdout).expect("--json");
    assert_eq!(c_report["align_cached"], false);

    let b_composite = export_composite(&cwd, &out_b.join("mantis-512.ffp"));
    let c_composite = export_composite(&cwd, &out_c.join("mantis-512.ffp"));
    assert_eq!(
        read_pixels(&b_composite),
        read_pixels(&c_composite),
        "a cache-reused alignment must fuse to the same composite a cold recompute of the same \
         parameters produces"
    );
}

/// Every input to the alignment misses the cache when it changes: an
/// alignment parameter, the failure policy and the frame set; the unchanged
/// rerun hits. Runs over a temp copy of `fixtures/mantis-512`'s frames.
#[test]
fn an_alignment_input_change_misses_the_cache() {
    let cwd = tmp_dir("cache_cli_alignment_input_miss");
    let frames = cwd.join("frames");
    std::fs::create_dir_all(&frames).unwrap();
    let mut sources: Vec<PathBuf> = std::fs::read_dir(mantis_512_dir().join("frames"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    sources.sort();
    for source in &sources {
        std::fs::copy(source, frames.join(source.file_name().unwrap())).unwrap();
    }

    let cached = |label: &str, input: &Path, extra: &[&str]| -> bool {
        let out = cwd.join(format!("out_{label}"));
        std::fs::create_dir_all(&out).unwrap();
        let mut args = vec![
            "stack",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--align-algorithm",
            "dummy-align",
            "--json",
        ];
        args.extend_from_slice(extra);
        let run = run_cli_in(&cwd, &common::push_no_gpu(args));
        assert!(run.success, "{label} stderr: {}", run.stderr);
        let report: serde_json::Value = serde_json::from_str(&run.stdout).expect("--json");
        report["align_cached"]
            .as_bool()
            .expect("align_cached is a bool")
    };

    assert!(!cached("first", &frames, &[]), "the first run computes");
    assert!(cached("again", &frames, &[]), "the unchanged rerun hits");
    assert!(
        !cached("param", &frames, &["--align-set", "max-shift=8"]),
        "an alignment parameter change misses"
    );
    assert!(
        !cached("strict", &frames, &["--strict"]),
        "a failure policy change misses"
    );
    std::fs::remove_file(frames.join(sources.last().unwrap().file_name().unwrap())).unwrap();
    assert!(!cached("fewer", &frames, &[]), "a changed frame set misses");
}

/// A stage-cache hit still *enters* the align stage (a fingerprint lookup
/// and a JSON decode of the cached transforms), so it must still report a
/// number there -- never `null`, which would read as "alignment was
/// skipped" rather than "alignment was reused". `align_source` is what
/// tells the two apart from a genuine (slow) compute.
#[test]
fn stage_timings_survive_a_cache_hit() {
    let cwd = tmp_dir("cache_cli_stage_timings_survive_hit");
    let input = mantis_512_dir();
    let out_a = cwd.join("out_a");
    let out_b = cwd.join("out_b");
    for dir in [&out_a, &out_b] {
        std::fs::create_dir_all(dir).unwrap();
    }

    let a = run_cli_in(
        &cwd,
        &common::push_no_gpu(vec![
            "stack",
            input.to_str().unwrap(),
            "-o",
            out_a.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--align-algorithm",
            "dummy-align",
            "--set",
            "levels=3",
            "--json",
        ]),
    );
    assert!(a.success, "stderr: {}", a.stderr);

    let b = run_cli_in(
        &cwd,
        &common::push_no_gpu(vec![
            "stack",
            input.to_str().unwrap(),
            "-o",
            out_b.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--align-algorithm",
            "dummy-align",
            "--set",
            "levels=5",
            "--json",
        ]),
    );
    assert!(b.success, "stderr: {}", b.stderr);
    let b_report: serde_json::Value = serde_json::from_str(&b.stdout).expect("--json");
    assert_eq!(
        b_report["align_cached"], true,
        "this run must be the cache hit: {b_report}"
    );
    assert!(
        b_report["stages"]["align"].is_number(),
        "a cache hit still enters the align stage, and must report a number, not null: {b_report}"
    );
    assert_eq!(
        b_report["align_source"], "cached",
        "align_source is what explains the small number: {b_report}"
    );
}

/// `--no-cache` recomputes even when a valid entry exists, and leaves no
/// entry behind.
#[test]
fn no_cache_flag_forces_a_recompute() {
    let cwd = tmp_dir("cache_cli_no_cache");
    let input = mantis_512_dir();
    let out_a = cwd.join("out_a");
    let out_b = cwd.join("out_b");
    std::fs::create_dir_all(&out_a).unwrap();
    std::fs::create_dir_all(&out_b).unwrap();

    let common_args = [
        "--algorithm",
        "dummy-stack",
        "--align-algorithm",
        "dummy-align",
        "--json",
    ];

    let mut a_args = vec![
        "stack",
        input.to_str().unwrap(),
        "-o",
        out_a.to_str().unwrap(),
    ];
    a_args.extend_from_slice(&common_args);
    let a = run_cli_in(&cwd, &a_args);
    assert!(a.success, "stderr: {}", a.stderr);
    let entries_after_a = cache_entry_count(&cwd);
    assert_eq!(
        entries_after_a, 1,
        "the first run must populate the cache exactly once"
    );

    let mut b_args = vec![
        "stack",
        input.to_str().unwrap(),
        "-o",
        out_b.to_str().unwrap(),
    ];
    b_args.extend_from_slice(&common_args);
    b_args.push("--no-cache");
    let b = run_cli_in(&cwd, &b_args);
    assert!(b.success, "stderr: {}", b.stderr);
    let b_report: serde_json::Value = serde_json::from_str(&b.stdout).expect("--json");
    assert_eq!(
        b_report["align_cached"], false,
        "--no-cache must force a recompute even though a valid entry exists"
    );
    assert!(
        b.stderr.lines().any(|l| l == "align 8 frames"),
        "stderr: {}",
        b.stderr
    );

    assert_eq!(
        cache_entry_count(&cwd),
        entries_after_a,
        "--no-cache must leave no entry behind"
    );

    let a_composite = export_composite(&cwd, &out_a.join("mantis-512.ffp"));
    let b_composite = export_composite(&cwd, &out_b.join("mantis-512.ffp"));
    assert_eq!(
        read_pixels(&a_composite),
        read_pixels(&b_composite),
        "recomputing under --no-cache with the same inputs must reproduce the same composite"
    );
}

// --- cache clear --------------------------------------------------------

#[test]
fn cache_without_action_deletes_nothing() {
    let cwd = tmp_dir("cache_cli_without_action");
    let entry = cwd
        .join(".scratch")
        .join("stage-cache")
        .join("align")
        .join("sentinel.json");
    std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
    std::fs::write(&entry, b"stage cache").unwrap();
    let resume = cwd.join(".scratch").join("resume").join("sentinel");
    std::fs::create_dir_all(resume.parent().unwrap()).unwrap();
    std::fs::write(&resume, b"resume").unwrap();

    for args in [["cache"].as_slice(), ["cache", "--json"].as_slice()] {
        let output = run_cli_in(&cwd, args);
        assert_eq!(output.code, Some(2), "stderr: {}", output.stderr);
        assert!(output.stderr.contains("clear"), "stderr: {}", output.stderr);
        assert!(output.stdout.is_empty(), "stdout: {}", output.stdout);
        assert_eq!(std::fs::read(&entry).unwrap(), b"stage cache");
        assert_eq!(std::fs::read(&resume).unwrap(), b"resume");
    }
}

/// `cache clear` empties the stage cache and touches nothing else: a
/// cancelled run's resume ledger, the sealed project, the run manifest and
/// the tile scheduler's staging directory under `.scratch/projects/` all
/// survive byte for byte (or, for a directory, simply survive).
#[test]
fn cache_clear_removes_entries_and_nothing_else() {
    let cwd = tmp_dir("cache_cli_clear");
    let input = mantis_512_dir();
    let out = cwd.join("out");
    std::fs::create_dir_all(&out).unwrap();

    let run = run_cli_in(
        &cwd,
        &[
            "stack",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--align-algorithm",
            "dummy-align",
            "--json",
        ],
    );
    assert!(run.success, "stderr: {}", run.stderr);
    assert_eq!(
        cache_entry_count(&cwd),
        1,
        "the run must have written one cache entry"
    );

    let ledger_dir = leave_a_cancelled_run_ledger(&cwd);
    let ledger_job_before = std::fs::read(ledger_dir.join("job.json")).unwrap();

    let project_before = std::fs::read(out.join("mantis-512.ffp")).unwrap();
    let manifest_before = std::fs::read(out.join("manifest.json")).unwrap();

    // A completed run removes its staging directory, so build the one an
    // interrupted run leaves behind directly.
    let leftover_staging = cwd
        .join(".scratch")
        .join("projects")
        .join("leftover-abc123");
    std::fs::create_dir_all(&leftover_staging).unwrap();
    std::fs::write(leftover_staging.join("composite.tif"), b"leftover").unwrap();
    let staging_before = staged_project_dirs(&cwd);
    assert!(
        !staging_before.is_empty(),
        "the leftover staging directory must be visible before the clear"
    );

    let clear = run_cli_in(&cwd, &["cache", "clear", "--json"]);
    assert!(clear.success, "stderr: {}", clear.stderr);
    let clear_report: serde_json::Value = serde_json::from_str(&clear.stdout).expect("--json");
    assert_eq!(clear_report["removed"], 1);

    assert_eq!(
        cache_entry_count(&cwd),
        0,
        "cache clear must empty the stage cache"
    );

    let empty = run_cli_in(&cwd, &["cache", "clear"]);
    assert!(empty.success, "stderr: {}", empty.stderr);
    assert!(
        empty.stdout.contains("removed 0"),
        "stdout: {}",
        empty.stdout
    );

    let align_cache = cwd.join(".scratch").join("stage-cache").join("align");
    std::fs::create_dir_all(&align_cache).unwrap();
    std::fs::write(align_cache.join("first.json"), b"first").unwrap();
    std::fs::write(align_cache.join("second.json"), b"second").unwrap();
    let multiple = run_cli_in(&cwd, &["cache", "clear", "--json"]);
    assert!(multiple.success, "stderr: {}", multiple.stderr);
    let multiple_report: serde_json::Value =
        serde_json::from_str(&multiple.stdout).expect("--json");
    assert_eq!(multiple_report["removed"], 2);
    assert_eq!(cache_entry_count(&cwd), 0);

    assert_eq!(
        std::fs::read(ledger_dir.join("job.json")).unwrap(),
        ledger_job_before,
        "a cancelled run's resume ledger must survive a cache clear byte for byte"
    );
    assert_eq!(
        std::fs::read(out.join("mantis-512.ffp")).unwrap(),
        project_before,
        "the sealed project must survive a cache clear byte for byte"
    );
    assert_eq!(
        std::fs::read(out.join("manifest.json")).unwrap(),
        manifest_before,
        "the run manifest must survive a cache clear byte for byte"
    );
    let staging_after = staged_project_dirs(&cwd);
    assert_eq!(
        staging_after, staging_before,
        "the tile scheduler's staging directories under .scratch/projects/ must survive a cache clear"
    );
}

// --- --only subset selection --------------------------------------------

/// Naming a subset re-runs only that result: the other groups' projects are
/// left untouched (mtime included), and one `skipped (not named by --only)`
/// line is printed per unselected group.
#[test]
fn only_runs_the_named_results() {
    let cwd = tmp_dir("cache_cli_only_runs");
    let dump = three_group_dump(&cwd.join("dump"));
    let out = cwd.join("out");
    std::fs::create_dir_all(&out).unwrap();

    let first = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(first.success, "stderr: {}", first.stderr);

    let projects: Vec<PathBuf> = std::fs::read_dir(&out)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "ffp"))
        .collect();
    assert_eq!(projects.len(), 3, "one project per proposed group");
    let mtimes_before: Vec<(PathBuf, std::time::SystemTime)> = projects
        .iter()
        .map(|p| (p.clone(), std::fs::metadata(p).unwrap().modified().unwrap()))
        .collect();

    // Coarse filesystem mtime resolution (some platforms round to whole
    // seconds) needs real elapsed time between the two runs, not merely two
    // process starts, to make "unchanged" and "changed" distinguishable.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let second = run_cli_in(
        &cwd,
        &[
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
        ],
    );
    assert!(second.success, "stderr: {}", second.stderr);
    let skipped_lines: Vec<&str> = second
        .stderr
        .lines()
        .filter(|l| l.starts_with("skipped (not named by --only)"))
        .collect();
    assert_eq!(
        skipped_lines.len(),
        2,
        "one skipped line per unselected group: {}",
        second.stderr
    );

    for (path, before) in &mtimes_before {
        let after = std::fs::metadata(path).unwrap().modified().unwrap();
        let name = path.file_name().unwrap().to_str().unwrap();
        if name.starts_with("IMG_9801-9802") {
            assert_ne!(
                after, *before,
                "the named result must have been re-run: {name}"
            );
        } else {
            assert_eq!(
                after, *before,
                "an unselected group's project must be left untouched: {name}"
            );
        }
    }
}

/// `--only` accepts the identity `review --json` prints, not merely a
/// group's plain name -- exercised on a single-group run, where the two
/// spellings differ: the identity is the input directory as the user wrote it, while the
/// group's own name falls back to the directory's basename.
#[test]
fn only_accepts_the_identity_review_prints() {
    let cwd = tmp_dir("cache_cli_only_identity");
    let dump = cwd.join("solo");
    frame(&dump, "00.tif", Some("2026:08:29 18:42:41"));
    frame(&dump, "01.tif", Some("2026:08:29 18:42:42"));
    let out = cwd.join("out");
    std::fs::create_dir_all(&out).unwrap();

    let first = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(first.success, "stderr: {}", first.stderr);

    let review = run_cli_in(&cwd, &["review", out.to_str().unwrap(), "--json"]);
    assert!(review.success, "stderr: {}", review.stderr);
    let review_report: serde_json::Value = serde_json::from_str(&review.stdout).expect("--json");
    let identity = review_report["results"][0]["identity"]
        .as_str()
        .expect("review must print an identity")
        .to_string();
    assert_eq!(
        identity,
        dump.to_str().unwrap(),
        "a single-group run's identity is the input directory as the user wrote it"
    );

    let second = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--only",
            &identity,
        ],
    );
    assert!(second.success, "stderr: {}", second.stderr);
    assert!(
        !second.stderr.contains("skipped (not named by --only)"),
        "the identity review prints must select the one group, not skip it: {}",
        second.stderr
    );
}

/// Naming an identity that does not exist fails with a message listing the
/// identities that do, at a non-zero exit, before any frame is opened.
#[test]
fn unknown_only_value_fails_clearly() {
    let cwd = tmp_dir("cache_cli_only_unknown");
    let dump = three_group_dump(&cwd.join("dump"));
    let out = cwd.join("out");
    std::fs::create_dir_all(&out).unwrap();

    let result = run_cli_in(
        &cwd,
        &[
            "stack",
            dump.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--only",
            "no-such-identity",
        ],
    );
    assert!(!result.success);
    assert_eq!(result.code, Some(1), "never clap's generic exit 2");
    assert!(
        result.stderr.contains("no-such-identity"),
        "the refusal must name the value that matched nothing: {}",
        result.stderr
    );
    for known in ["IMG_9801-9802", "IMG_9910-9911", "IMG_9950-9951"] {
        assert!(
            result.stderr.contains(known),
            "the refusal must list the known identities, missing {known}: {}",
            result.stderr
        );
    }
    assert!(
        !result.stderr.contains("fuse "),
        "the refusal must fire before any frame is opened: {}",
        result.stderr
    );
    assert!(
        std::fs::read_dir(&out).unwrap().next().is_none(),
        "no project may be written before the refusal"
    );
}

// --- scratch containment -------------------------------------------------

/// Cache state never enters the project format or a user-visible output
/// directory: no file under `-o` names the cache, and no byte sequence
/// naming the cache appears inside the sealed `*.ffp`. The store sits at
/// `.scratch/stage-cache/`.
#[test]
fn the_cache_stays_in_the_scratch_directory() {
    let cwd = tmp_dir("cache_cli_stays_in_scratch");
    let input = mantis_512_dir();
    let out = cwd.join("out");
    std::fs::create_dir_all(&out).unwrap();

    let run = run_cli_in(
        &cwd,
        &[
            "stack",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--align-algorithm",
            "dummy-align",
            "--json",
        ],
    );
    assert!(run.success, "stderr: {}", run.stderr);

    assert!(
        cwd.join(".scratch").join("stage-cache").is_dir(),
        "the store must sit at .scratch/stage-cache/"
    );
    assert_eq!(cache_entry_count(&cwd), 1);

    for entry in walk(&out) {
        assert!(
            !entry.to_string_lossy().contains("stage-cache"),
            "the cache must never leak into -o: {}",
            entry.display()
        );
    }

    let project_bytes = std::fs::read(out.join("mantis-512.ffp")).unwrap();
    let needle = b"stage-cache";
    assert!(
        !project_bytes
            .windows(needle.len())
            .any(|window| window == needle),
        "no byte sequence naming the cache may appear inside the sealed project"
    );
}
