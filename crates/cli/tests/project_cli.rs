// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level coverage of what a `stack` run writes: **a project, not
//! an image**.
//!
//! Every run here is over `fixtures/mantis-64` (8 frames at 64x64, a real
//! `laplacian` fusion in milliseconds) or over a **copy** of it in
//! `CARGO_TARGET_TMPDIR`. Never the committed fixture tree as an output
//! directory: `tools/check-fixtures.sh` diffs the present file list against
//! `MANIFEST.sha256`, so one stray `.ffp` under `fixtures/` fails the
//! gate.
//!
//! Every test isolates `.scratch/` with its own per-process temp directory and
//! `.current_dir(cwd)` through [`run_cli_in`]: `.scratch/resume` and
//! `.scratch/projects` are resolved relative to the process's cwd, so tests
//! sharing one cwd share one ledger store and race each other's cleanup -- and
//! concurrent test runs sharing one `target/` must never share a working
//! directory either.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::CliOutput;

fn bin() -> Command {
    common::cli_bin("project_cli")
}

/// Runs the binary with `cwd` as its working directory.
fn run_cli_in(cwd: &Path, args: &[&str]) -> CliOutput {
    common::run_in(&mut bin(), cwd, args)
}

/// Every file under `dir`, recursively, as paths relative to it.
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

/// Every staged composite under `<cwd>/.scratch/projects/`. The staging key
/// is the group name plus a digest of the project path, so a test looks for
/// the planes rather than spelling the key.
fn staged_composites(cwd: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(cwd.join(".scratch").join("projects"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("composite.tif"))
        .filter(|p| p.is_file())
        .collect();
    found.sort();
    found
}

fn extensions<'a>(files: &'a [PathBuf], ext: &str) -> Vec<&'a PathBuf> {
    files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == ext))
        .collect()
}

/// A writable copy of `fixtures/mantis-64`, so a beside-the-frames run has
/// somewhere legal to land.
///
/// Returns the guard alongside the target path: dropping the guard deletes
/// the tree, so the caller must hold it for as long as the path is in use.
fn fixture_copy(name: &str) -> (common::TmpDir, PathBuf) {
    let dir = common::tmp_dir(name);
    let target = dir.join("mantis-64");
    let frames = target.join("frames");
    std::fs::create_dir_all(&frames).unwrap();
    let source = common::fixtures_dir().join("mantis-64").join("frames");
    for entry in std::fs::read_dir(&source).unwrap().flatten() {
        std::fs::copy(entry.path(), frames.join(entry.file_name())).unwrap();
    }
    (dir, target)
}

fn stack_args<'a>(dir: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "stack",
        dir,
        "--algorithm",
        "laplacian",
        "--no-align",
        "--no-crop",
    ];
    args.extend_from_slice(extra);
    common::push_no_gpu(args)
}

#[test]
fn stack_writes_a_project_and_no_image() {
    let cwd = common::tmp_dir("project_cli_no_image_cwd");
    let out = common::tmp_dir("project_cli_no_image");
    let input = common::fixtures_dir().join("mantis-64");
    let result = run_cli_in(
        &cwd,
        &stack_args(input.to_str().unwrap(), &["-o", out.to_str().unwrap()]),
    );
    assert!(result.success, "stderr: {}", result.stderr);

    let files = walk(&out);
    let projects = extensions(&files, "ffp");
    assert_eq!(projects.len(), 1, "exactly one project expected: {files:?}");
    assert!(
        extensions(&files, "tif").is_empty() && extensions(&files, "tiff").is_empty(),
        "a run must write no image at all: {files:?}"
    );
    assert!(
        !files
            .iter()
            .any(|p| p.to_string_lossy().contains("partial")),
        "a temp file survived: {files:?}"
    );

    // The staged planes live under `.scratch/projects/<group name>/` and
    // that directory is gone once the run completes. Scoped to **this**
    // group's key, never the whole tree: `cancel_keeps_the_staged_planes_for_resume`
    // runs beside this test and deliberately leaves its own group's staging
    // behind.
    assert!(
        staged_composites(&cwd).is_empty(),
        "the staging directory survived a completed run: {:?}",
        staged_composites(&cwd)
    );
}

#[test]
fn project_lands_beside_the_frames() {
    let cwd = common::tmp_dir("project_cli_beside_cwd");
    let (_dir, target) = fixture_copy("project_cli_beside");
    let before = walk(&target);
    let result = run_cli_in(&cwd, &stack_args(target.to_str().unwrap(), &[]));
    assert!(result.success, "stderr: {}", result.stderr);

    let after = walk(&target);
    let projects = extensions(&after, "ffp");
    assert_eq!(
        projects.len(),
        1,
        "one project beside the frames: {after:?}"
    );
    assert_eq!(
        projects[0].parent(),
        Some(Path::new("")),
        "the project belongs beside `frames/`, never inside it: {:?}",
        projects[0]
    );
    // Every frame is exactly as it was.
    for frame in &before {
        assert!(after.contains(frame), "a frame disappeared: {frame:?}");
    }
    assert!(
        extensions(&after, "tif").len() == before.len(),
        "a run wrote an image beside the frames: {after:?}"
    );
    assert!(
        result.stdout.contains(".ffp") || result.stderr.contains(".ffp"),
        "the report must name the project it wrote -- stdout: {}",
        result.stdout
    );
}

#[test]
fn output_dir_collects_the_project() {
    let cwd = common::tmp_dir("project_cli_collect_cwd");
    let (_dir, target) = fixture_copy("project_cli_collect");
    let out = common::tmp_dir("project_cli_collect_out");
    let before = walk(&target);
    let result = run_cli_in(
        &cwd,
        &stack_args(target.to_str().unwrap(), &["-o", out.to_str().unwrap()]),
    );
    assert!(result.success, "stderr: {}", result.stderr);
    assert_eq!(extensions(&walk(&out), "ffp").len(), 1);
    assert_eq!(
        walk(&target),
        before,
        "the frame directory must be untouched when -o is given"
    );
}

#[test]
fn refuses_a_missing_output_directory() {
    let cwd = common::tmp_dir("project_cli_missing_out_cwd");
    // Bound, not a temporary: `common::tmp_dir` returns a guard that deletes
    // its directory on drop, so `.join(..)` on an unbound temporary would
    // have the tree gone before the CLI ever saw it.
    let base = common::tmp_dir("project_cli_missing_out");
    let missing = base.join("nope");
    let input = common::fixtures_dir().join("mantis-64");
    let result = run_cli_in(
        &cwd,
        &stack_args(input.to_str().unwrap(), &["-o", missing.to_str().unwrap()]),
    );
    assert!(!result.success);
    assert_eq!(result.code, Some(1));
    assert!(
        result.stderr.contains("is not one") || result.stderr.contains("existing directory"),
        "stderr: {}",
        result.stderr
    );
    assert!(!missing.exists(), "the refusal created the directory");
}

#[test]
fn refuses_an_output_file_path_by_name() {
    let cwd = common::tmp_dir("project_cli_output_file_cwd");
    // Bound, not a temporary -- see `refuses_a_missing_output_directory`.
    let base = common::tmp_dir("project_cli_output_file");
    let out = base.join("out.tif");
    let input = common::fixtures_dir().join("mantis-64");
    let result = run_cli_in(
        &cwd,
        &stack_args(input.to_str().unwrap(), &["-o", out.to_str().unwrap()]),
    );
    assert!(!result.success);
    assert!(
        result.stderr.contains("export"),
        "an -o <file> refusal must name `export`: {}",
        result.stderr
    );
    assert!(!out.exists());
}

#[test]
fn one_project_per_group_and_a_failure_spares_the_others() {
    let root = common::tmp_dir("project_cli_multi_group");
    let good = root.join("good");
    let bad = root.join("bad");
    std::fs::create_dir_all(good.join("frames")).unwrap();
    std::fs::create_dir_all(&bad).unwrap();
    let source = common::fixtures_dir().join("mantis-64").join("frames");
    let mut names: Vec<_> = std::fs::read_dir(&source)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    names.sort();
    for name in &names {
        std::fs::copy(source.join(name), good.join("frames").join(name)).unwrap();
    }
    // A second group of deliberately non-uniform frames: one truncated
    // member makes the group fail while the first group still completes.
    for name in names.iter().take(2) {
        std::fs::copy(source.join(name), bad.join(name)).unwrap();
    }
    let victim = bad.join(&names[1]);
    let bytes = std::fs::read(&victim).unwrap();
    std::fs::write(&victim, &bytes[..bytes.len() / 2]).unwrap();

    let cwd = common::tmp_dir("project_cli_multi_group_cwd");
    let out = common::tmp_dir("project_cli_multi_group_out");
    let result = run_cli_in(
        &cwd,
        &stack_args(root.to_str().unwrap(), &["-o", out.to_str().unwrap()]),
    );
    assert!(!result.success, "one group failed, so the run failed");
    let projects = extensions(&walk(&out), "ffp").len();
    assert_eq!(
        projects, 1,
        "the healthy group's project must survive its neighbour's failure"
    );
}

#[test]
fn report_names_the_project_and_no_image() {
    let cwd = common::tmp_dir("project_cli_report_cwd");
    let out = common::tmp_dir("project_cli_report");
    let input = common::fixtures_dir().join("mantis-64");
    let result = run_cli_in(
        &cwd,
        &stack_args(
            input.to_str().unwrap(),
            &["-o", out.to_str().unwrap(), "--json"],
        ),
    );
    assert!(result.success, "stderr: {}", result.stderr);
    let report: serde_json::Value = serde_json::from_str(&result.stdout).expect("--json parses");
    let outputs: Vec<String> = report["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(outputs.len(), 1, "one project, no image: {outputs:?}");
    assert!(outputs[0].ends_with(".ffp"), "{outputs:?}");

    // The human rendering says the same thing.
    let human = run_cli_in(
        &cwd,
        &stack_args(input.to_str().unwrap(), &["-o", out.to_str().unwrap()]),
    );
    assert!(human.stdout.contains(".ffp"), "{}", human.stdout);
    assert!(
        !human.stdout.contains("wrote: ") || !human.stdout.contains(".tif\n"),
        "the report names no image path: {}",
        human.stdout
    );

    // `--dry-run` promises exactly that project and nothing else.
    let plan = run_cli_in(
        &cwd,
        &stack_args(
            input.to_str().unwrap(),
            &["-o", out.to_str().unwrap(), "--dry-run", "--json"],
        ),
    );
    assert!(plan.success, "stderr: {}", plan.stderr);
    let plan: serde_json::Value = serde_json::from_str(&plan.stdout).expect("--json parses");
    let would: Vec<String> = plan["would_write"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(would.len(), 1, "{would:?}");
    assert!(would[0].ends_with(".ffp"), "{would:?}");
    assert_eq!(would[0], outputs[0]);
}

/// The **cancel** half of the failed-versus-cancelled distinction: a
/// cancelled run deliberately keeps its staged planes and its ledger so
/// `--resume` can continue, where a failed one removes both.
///
/// Sends a SIGINT once the fuse stage has actually started, rather than
/// sleeping for a fixed duration.
#[cfg(unix)]
#[test]
fn cancel_keeps_the_staged_planes_for_resume() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    let cwd = common::tmp_dir("project_cli_cancel_cwd");
    let out = common::tmp_dir("project_cli_cancel");
    let input = common::fixtures_dir().join("mantis-512");
    let mut child = bin()
        .current_dir(&*cwd)
        .args(common::push_no_gpu(vec![
            "stack",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--no-align",
            "--no-crop",
        ]))
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");

    let stderr = BufReader::new(child.stderr.take().unwrap());
    let mut lines = stderr.lines();
    // The fuse-stage entry line is written the instant the job starts.
    for line in lines.by_ref() {
        let line = line.unwrap();
        if line.contains("eta") || line.contains("tile") {
            break;
        }
    }
    send_sigint(child.id());
    let status = child.wait().expect("wait");
    let rest: Vec<String> = lines.map_while(Result::ok).collect();
    let text = rest.join("\n");

    if status.code() != Some(130) {
        // The run finished before the signal landed -- a fast machine, not
        // a regression. Nothing to assert about cancellation then.
        eprintln!("run completed before the SIGINT landed; skipping");
        return;
    }
    assert!(
        text.contains("resume with") || text.contains("cancelled"),
        "a cancel must say how to resume: {text}"
    );
    assert!(
        extensions(&walk(&out), "ffp").is_empty(),
        "a cancelled run writes no project"
    );
    let staged = staged_composites(&cwd);
    assert_eq!(
        staged.len(),
        1,
        "a cancelled run must keep its staged planes for --resume: {staged:?}"
    );
}

/// `kill -INT <pid>`, the same spelling `job_cli.rs`'s own signal tests use
/// -- no `libc` dependency, and the workspace denies `unsafe_code` anyway.
#[cfg(unix)]
fn send_sigint(pid: u32) {
    let status = Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status()
        .expect("failed to invoke kill(1)");
    assert!(status.success(), "kill -INT {pid} failed");
}
