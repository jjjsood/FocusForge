// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level coverage of `stack` driven through the job engine --
//! progress on stderr, SIGINT cancel/resume, `--quiet`, the scheduler
//! tuning flags and the three exit codes.
//!
//! Every interruptible run passes `--no-align --no-crop --set levels=3
//! --tile-size 160 --max-bytes-in-flight 1`, turning `fixtures/mantis-512`'s
//! 4-tile default plan into 25 single-worker units. Not `--tile-size 64`,
//! which this tree refuses outright: "tile_size 64 leaves no interior at
//! margin 24 for levels=3; use tile_size >= 160".
//! Every SIGINT test drives its kill point off a line actually read from
//! the spawned child's piped stderr -- never a sleep, since signal tests
//! are the flakiest kind of test in a suite.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
}

fn mantis_512_dir() -> PathBuf {
    common::fixtures_dir().join("mantis-512")
}

/// A fresh, empty directory, removed again when the test that made it ends
/// -- used as the spawned process's own working directory, so `.scratch/`
/// lands here and never collides with another test's.
///
/// Derefs to `Path`, so `&dir` and `dir.join(..)` read exactly as they did
/// when this was a bare `PathBuf`.
struct TmpDir {
    path: PathBuf,
}

impl std::ops::Deref for TmpDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        // A failed test keeps its evidence -- the partials, the ledger and
        // the outputs are exactly what a failure needs to be diagnosed.
        if std::thread::panicking() {
            return;
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// **Named per process, not just per test.** `CARGO_TARGET_TMPDIR` is inside
/// the shared `target/`, so concurrent test runs get the *same* path for the
/// same test name, and the `remove_dir_all` below would delete another run's
/// live `.scratch/resume/` ledger mid-run (`stack` then exits 1 with `No such
/// file or directory`). Running this binary twice at once failed 3 rounds out
/// of 3 without the pid in the name, and 0 out of 3 with it.
fn tmp_dir(name: &str) -> TmpDir {
    let path =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    TmpDir { path }
}

struct CliOutput {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_cli_in(dir: &Path, args: &[&str]) -> CliOutput {
    let output = bin()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run binary");
    CliOutput {
        success: output.status.success(),
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn spawn_piped(dir: &Path, args: &[&str]) -> Child {
    bin()
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn")
}

fn read_line(reader: &mut impl BufRead) -> String {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("failed to read a stderr line");
    line
}

/// Copies `fixtures/mantis-512/frames/*.tif` into `<dest>/frames/`, so a
/// test that mutates a frame's bytes (`resume_with_changed_inputs_fails`)
/// never touches the committed fixture.
fn copy_fixture_frames(dest: &Path) -> PathBuf {
    let frames_dir = dest.join("frames");
    std::fs::create_dir_all(&frames_dir).unwrap();
    let src_frames = mantis_512_dir().join("frames");
    for entry in std::fs::read_dir(&src_frames).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        std::fs::copy(entry.path(), frames_dir.join(&name)).unwrap();
    }
    dest.to_path_buf()
}

fn read_pixels(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("output file must be readable")
}

/// `stack <dir> -o <output> --algo laplacian --no-align --no-crop --tile-size
/// 160 --set levels=3 --max-bytes-in-flight 1` -- the single-worker recipe
/// this module's own doc comment explains (25 units against
/// `fixtures/mantis-512`'s 4-tile default plan), assembled once so every
/// interruptible-run test stops repeating it verbatim.
fn interruptible_stack_args<'a>(dir: &'a str, output: &'a str) -> Vec<&'a str> {
    vec![
        "stack",
        dir,
        "-o",
        output,
        "--algo",
        "laplacian",
        "--no-align",
        "--no-crop",
        "--tile-size",
        "160",
        "--set",
        "levels=3",
        "--max-bytes-in-flight",
        "1",
    ]
}

#[test]
fn stack_output_is_unchanged_under_the_job_engine() {
    use focusforge_core::fuse::{FusionConsumer, FusionGeometry};
    use focusforge_core::io::TiffStack;
    use focusforge_core::params::{self, ParamValues, StackContext};
    use focusforge_core::registry;
    use focusforge_core::tile::{NoopPreprocess, TileScheduler, TileSchedulerOptions};

    let dir = tmp_dir("job_cli_output_unchanged");
    let output = dir.join("out.tif");
    let out = run_cli_in(
        &dir,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--algo",
            "laplacian",
            "--no-align",
            "--no-crop",
        ],
    );
    assert!(out.success, "stderr: {}", out.stderr);

    // The in-process library oracle: the same `FusionConsumer` +
    // `TileScheduler::run` pairing `crates/cli/tests/stack_cli.rs` builds.
    let stack = TiffStack::open(mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let stack_ctx = StackContext::new(stack.len(), width, height, spp);
    let algo = registry::find_stack("laplacian").unwrap();
    let resolved = params::resolve(algo.params(), &ParamValues::new(), &stack_ctx).unwrap();
    let values = params::resolved_values(&resolved);
    let levels = algo.tile_levels(&values);
    let oracle_path = dir.join("oracle.tif");
    let options = TileSchedulerOptions {
        tile_size: 512,
        levels,
        max_bytes_in_flight: 1 << 30,
        scratch_base: dir.join("oracle-scratch"),
    };
    let scheduler = TileScheduler::new(&stack, stack_ctx, spp, &options).unwrap();
    let consumer = FusionConsumer::create(
        algo,
        values,
        &oracle_path,
        None,
        FusionGeometry {
            width,
            height,
            samples_per_pixel: spp,
            frame_count: stack.len() as u32,
            crop: None,
            description: None,
        },
    )
    .unwrap();
    scheduler.run(&NoopPreprocess, &consumer).unwrap();
    consumer.finish().unwrap();

    assert_eq!(
        read_pixels(&output),
        read_pixels(&oracle_path),
        "stack must produce the same bytes through the job engine as the library oracle"
    );
}

#[test]
fn progress_line_contains_units_stage_and_eta() {
    let dir = tmp_dir("job_cli_progress_line");
    let output = dir.join("out.tif");
    let mantis = mantis_512_dir();
    let out = run_cli_in(
        &dir,
        &interruptible_stack_args(mantis.to_str().unwrap(), output.to_str().unwrap()),
    );
    assert!(out.success, "stderr: {}", out.stderr);
    let has_line = out
        .stderr
        .lines()
        .any(|line| line.starts_with("fuse ") && line.contains('/') && line.contains("tiles"));
    assert!(has_line, "stderr: {}", out.stderr);
    assert!(
        out.stderr.contains("eta --") || out.stderr.contains("eta "),
        "stderr must carry an eta field: {}",
        out.stderr
    );
    assert!(!out.stderr.contains("eta 0s"), "stderr: {}", out.stderr);
}

/// `fuse <done>/<total> tiles <pct>% eta <...>` -> `(done, total)`, for
/// every progress line on a captured stderr.
fn fuse_counts(stderr: &str) -> Vec<(usize, usize)> {
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix("fuse "))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter_map(|counts| counts.split_once('/'))
        .filter_map(|(done, total)| Some((done.parse().ok()?, total.parse().ok()?)))
        .collect()
}

/// Progress must be **live**: an uninterrupted (single-segment) fuse still
/// reports completed units mid-run, never `0/25` until the end.
///
/// Needs a run longer than one `PLAIN_INTERVAL` (2 s) for a mid-run line to
/// exist at all, which is what the `--tile-size 160 --max-bytes-in-flight 1`
/// single-worker recipe guarantees against this fixture.
#[test]
fn progress_advances_during_an_uninterrupted_run() {
    let dir = tmp_dir("job_cli_progress_advances");
    let output = dir.join("out.tif");
    let mantis = mantis_512_dir();
    let out = run_cli_in(
        &dir,
        &interruptible_stack_args(mantis.to_str().unwrap(), output.to_str().unwrap()),
    );
    assert!(out.success, "stderr: {}", out.stderr);

    let counts = fuse_counts(&out.stderr);
    assert!(
        counts.len() >= 2,
        "expected a start line plus at least one periodic line: {}",
        out.stderr
    );
    let total = counts[0].1;
    assert!(total > 1, "this recipe must plan more than one unit");
    let mid_run: Vec<usize> = counts
        .iter()
        .map(|(done, _)| *done)
        .filter(|done| *done > 0 && *done < total)
        .collect();
    assert!(
        !mid_run.is_empty(),
        "at least one progress line must report units done climbing towards the total, \
         not a counter frozen at 0 until the run ends: {}",
        out.stderr
    );
    assert!(
        !out.stderr.contains("eta --\neta --") && !out.stderr.contains("eta 0s"),
        "a mid-run line must carry a real ETA, never `0s`: {}",
        out.stderr
    );
    // Every line that reports a completed unit must also carry a real ETA:
    // `eta --` is only honest before the first unit finishes.
    for line in out.stderr.lines().filter(|l| l.starts_with("fuse ")) {
        let done: usize = line["fuse ".len()..]
            .split('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        if done > 0 {
            assert!(
                !line.contains("eta --"),
                "a line with {done} unit(s) done must carry an ETA: {line}"
            );
        }
    }
}

#[test]
fn progress_to_a_file_has_no_escape_sequences() {
    let dir = tmp_dir("job_cli_progress_no_escapes");
    let output = dir.join("out.tif");
    let start = std::time::Instant::now();
    let mantis = mantis_512_dir();
    let out = run_cli_in(
        &dir,
        &interruptible_stack_args(mantis.to_str().unwrap(), output.to_str().unwrap()),
    );
    let elapsed = start.elapsed().as_secs_f64();
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(!out.stderr.contains('\r'), "stderr: {}", out.stderr);
    assert!(!out.stderr.contains('\x1b'), "stderr: {}", out.stderr);
    let bound = 2 + (elapsed / 2.0).ceil() as usize;
    let line_count = out.stderr.lines().count();
    assert!(
        line_count <= bound,
        "line_count={line_count} bound={bound} elapsed={elapsed} stderr: {}",
        out.stderr
    );
}

#[test]
fn quiet_prints_no_progress() {
    let dir = tmp_dir("job_cli_quiet");
    let output = dir.join("out.tif");
    let out = run_cli_in(
        &dir,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--algo",
            "laplacian",
            "--no-align",
            "--no-crop",
            "--quiet",
        ],
    );
    assert!(out.success, "stderr: {}", out.stderr);
    assert!(out.stderr.is_empty(), "stderr: {}", out.stderr);
    assert!(output.exists());
}

#[cfg(unix)]
mod unix_signals {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    fn send(pid: u32, signal: &str) {
        let status = Command::new("kill")
            .args([signal, &pid.to_string()])
            .status()
            .expect("failed to invoke kill(1)");
        assert!(status.success(), "kill {signal} {pid} failed");
    }

    #[test]
    fn sigint_cancels_cleanly() {
        let dir = tmp_dir("job_cli_sigint_cancel");
        let output = dir.join("out.tif");
        let mantis = mantis_512_dir();
        let mut child = spawn_piped(
            &dir,
            &interruptible_stack_args(mantis.to_str().unwrap(), output.to_str().unwrap()),
        );
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let first = read_line(&mut stderr);
        assert!(first.starts_with("fuse "), "first line: {first}");

        send(child.id(), "-INT");
        let status = child.wait().expect("child must exit");

        assert_eq!(status.code(), Some(130), "status: {status:?}");
        assert!(!output.exists(), "named output must not exist");
        let partial = dir.join("out.tif.partial");
        assert!(partial.exists(), "partial output must survive a cancel");
        let resume_dir = dir.join(".scratch").join("resume");
        let entries: Vec<_> = std::fs::read_dir(&resume_dir)
            .expect("resume dir must exist")
            .collect();
        assert_eq!(entries.len(), 1, "exactly one ledger must survive");

        let mut rest = String::new();
        stderr.read_to_string(&mut rest).unwrap();
        assert!(rest.contains("--resume"), "stderr: {first}{rest}");
    }

    #[test]
    fn sigint_then_resume_finishes_the_job() {
        let ref_dir = tmp_dir("job_cli_resume_reference");
        let ref_output = ref_dir.join("out.tif");
        let ref_run = run_cli_in(
            &ref_dir,
            &[
                "stack",
                mantis_512_dir().to_str().unwrap(),
                "-o",
                ref_output.to_str().unwrap(),
                "--algo",
                "laplacian",
                "--no-align",
                "--no-crop",
                "--tile-size",
                "160",
                "--set",
                "levels=3",
                "--max-bytes-in-flight",
                "1",
            ],
        );
        assert!(ref_run.success, "stderr: {}", ref_run.stderr);

        let dir = tmp_dir("job_cli_resume_interrupted");
        let output = dir.join("out.tif");
        let mantis_dir = mantis_512_dir();
        let args = interruptible_stack_args(mantis_dir.to_str().unwrap(), output.to_str().unwrap());
        let mut child = spawn_piped(&dir, &args);
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let _first = read_line(&mut stderr);
        send(child.id(), "-INT");
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(130));

        let mut resumed_args: Vec<&str> = args.to_vec();
        resumed_args.push("--resume");
        let resumed = run_cli_in(&dir, &resumed_args);
        assert!(resumed.success, "stderr: {}", resumed.stderr);
        assert!(output.exists());

        assert_eq!(
            read_pixels(&output),
            read_pixels(&ref_output),
            "a killed-then-resumed run must match an uninterrupted one byte for byte"
        );
        let resume_dir = dir.join(".scratch").join("resume");
        let remaining = std::fs::read_dir(&resume_dir)
            .map(|d| d.count())
            .unwrap_or(0);
        assert_eq!(remaining, 0, "a completed run must discard its ledger");
    }

    #[test]
    fn second_sigint_exits_immediately() {
        let dir = tmp_dir("job_cli_second_sigint");
        let output = dir.join("out.tif");
        let mantis = mantis_512_dir();
        let mut child = spawn_piped(
            &dir,
            &interruptible_stack_args(mantis.to_str().unwrap(), output.to_str().unwrap()),
        );
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let _first = read_line(&mut stderr);

        send(child.id(), "-INT");
        std::thread::sleep(std::time::Duration::from_millis(50));
        send(child.id(), "-INT");

        let status = child.wait().expect("child must exit");
        assert_eq!(status.signal(), Some(2 /* SIGINT */), "status: {status:?}");
        assert_eq!(status.code(), None, "a signal death reports no exit code");
    }

    /// Every stderr line the child wrote, each stamped with when this test
    /// observed it -- the only way to assert that progress *stopped* for a
    /// while, which a post-hoc read of the whole stream cannot show.
    type StampedLines = Arc<Mutex<Vec<(Duration, String)>>>;

    fn read_stamped(
        stderr: std::process::ChildStderr,
        start: Instant,
    ) -> (StampedLines, JoinHandle<()>) {
        let lines: StampedLines = Arc::new(Mutex::new(Vec::new()));
        let handle = {
            let lines = Arc::clone(&lines);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut buf = String::new();
                while reader.read_line(&mut buf).unwrap_or(0) > 0 {
                    lines
                        .lock()
                        .unwrap()
                        .push((start.elapsed(), buf.trim_end().to_string()));
                    buf.clear();
                }
            })
        };
        (lines, handle)
    }

    /// When the first line matching `pred` was observed, or `None` if
    /// `timeout` passed without one.
    fn wait_for(
        lines: &StampedLines,
        timeout: Duration,
        pred: impl Fn(&str) -> bool,
    ) -> Option<Duration> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some((at, _)) = lines
                .lock()
                .unwrap()
                .iter()
                .find(|(_, line)| pred(line.as_str()))
            {
                return Some(*at);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// SIGUSR1 is a pause **toggle**, Unix-only: one signal pauses the job
    /// within the unit bound, a second clears the control and runs the next
    /// segment with the same consumer.
    ///
    /// The stall is asserted against observation timestamps, not against the
    /// absence of text: the renderer's interval clock is reset by the
    /// `paused` stage line, so a job that kept running would have emitted a
    /// progress line within one `PLAIN_INTERVAL` (2 s) of it, and this test
    /// watches for three.
    #[test]
    fn sigusr1_pauses_the_run_and_a_second_one_resumes_it() {
        let dir = tmp_dir("job_cli_sigusr1_pause");
        let output = dir.join("out.tif");
        let mantis = mantis_512_dir();
        let mut child = spawn_piped(
            &dir,
            &interruptible_stack_args(mantis.to_str().unwrap(), output.to_str().unwrap()),
        );
        let start = Instant::now();
        let (lines, reader) = read_stamped(child.stderr.take().unwrap(), start);

        // Drive the signal off an observed line, never a sleep.
        wait_for(&lines, Duration::from_secs(30), |line| {
            line.starts_with("fuse ")
        })
        .expect("the fuse stage must announce itself before any signal is sent");

        send(child.id(), "-USR1");
        let paused_at = wait_for(&lines, Duration::from_secs(30), |line| {
            line.contains("paused")
        })
        .expect("SIGUSR1 must pause the job and say so on stderr");

        let stall = Duration::from_secs(3);
        std::thread::sleep(stall);
        let resume_at = start.elapsed();

        let done_at_pause = {
            let observed = lines.lock().unwrap();
            let stalled: Vec<&String> = observed
                .iter()
                .filter(|(at, line)| {
                    *at > paused_at && *at <= resume_at && line.starts_with("fuse ")
                })
                .map(|(_, line)| line)
                .collect();
            assert!(
                stalled.is_empty(),
                "units done must stop climbing while paused, but {stall:?} after the pause line \
                 these progress lines arrived: {stalled:?}"
            );
            observed
                .iter()
                .filter(|(_, line)| line.starts_with("fuse "))
                .filter_map(|(_, line)| fuse_counts(line).first().map(|(done, _)| *done))
                .max()
                .expect("at least the fuse start line must have been seen")
        };

        send(child.id(), "-USR1");
        let status = child.wait().expect("child must exit");
        reader.join().unwrap();

        assert!(
            status.success(),
            "a resumed run must finish normally: {status:?}"
        );
        assert!(output.exists(), "the resumed run must produce its output");

        let observed = lines.lock().unwrap();
        let after_resume = observed
            .iter()
            .filter(|(at, line)| *at > resume_at && line.starts_with("fuse "))
            .filter_map(|(_, line)| fuse_counts(line).first().map(|(done, _)| *done))
            .max();
        assert!(
            after_resume.is_some_and(|done| done > done_at_pause),
            "the second SIGUSR1 must resume the run from the same count and keep climbing: \
             paused at {done_at_pause}, best after resume {after_resume:?}"
        );
        assert!(
            observed.iter().any(|(_, line)| line == "stack complete"),
            "lines: {observed:?}"
        );
    }

    #[test]
    fn resume_without_state_fails_clearly() {
        let dir = tmp_dir("job_cli_resume_without_state");
        let output = dir.join("out.tif");
        let out = run_cli_in(
            &dir,
            &[
                "stack",
                mantis_512_dir().to_str().unwrap(),
                "-o",
                output.to_str().unwrap(),
                "--algo",
                "laplacian",
                "--no-align",
                "--no-crop",
                "--resume",
            ],
        );
        assert!(!out.success);
        assert_eq!(out.code, Some(1));
        assert!(out.stderr.contains(".scratch/resume"), "{}", out.stderr);
    }

    #[test]
    fn resume_with_changed_inputs_fails() {
        let dir = tmp_dir("job_cli_resume_changed_inputs");
        let stack_dir = copy_fixture_frames(&dir.join("stack"));
        let output = dir.join("out.tif");
        let args = interruptible_stack_args(stack_dir.to_str().unwrap(), output.to_str().unwrap());
        let mut child = spawn_piped(&dir, &args);
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let _first = read_line(&mut stderr);
        send(child.id(), "-INT");
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(130));

        // Change one frame's `file_size` -- the fingerprint field the
        // header-only scheme actually compares.
        let frames_dir = stack_dir.join("frames");
        let first_frame = std::fs::read_dir(&frames_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&first_frame)
            .unwrap();
        file.write_all(b"\0").unwrap();
        drop(file);

        let mut resumed_args: Vec<&str> = args.to_vec();
        resumed_args.push("--resume");
        let resumed = run_cli_in(&dir, &resumed_args);
        assert!(!resumed.success);
        assert_eq!(resumed.code, Some(1));
        assert!(
            resumed.stderr.contains("file_size changed: expected"),
            "{}",
            resumed.stderr
        );
        assert!(
            resumed
                .stderr
                .contains(first_frame.file_name().unwrap().to_str().unwrap()),
            "{}",
            resumed.stderr
        );
    }

    /// Which compute backend produced the already-written tiles is part of
    /// a ledger's identity: a run cancelled on the GPU path and resumed on
    /// the CPU one (or the reverse) would silently finish one file out of
    /// two different implementations. The default gate compiles no `gpu`
    /// feature, so both runs here genuinely resolve `"cpu"` -- the recorded
    /// value is edited between them, which is the only way to reach the
    /// refusal without a GPU-feature build, and is exactly what a resume on
    /// a differently-built binary would look like on disk.
    #[test]
    fn resume_with_a_changed_backend_fails() {
        let dir = tmp_dir("job_cli_resume_changed_backend");
        let output = dir.join("out.tif");
        let mantis_dir = mantis_512_dir();
        let args = interruptible_stack_args(mantis_dir.to_str().unwrap(), output.to_str().unwrap());
        let mut child = spawn_piped(&dir, &args);
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let _first = read_line(&mut stderr);
        send(child.id(), "-INT");
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(130));

        let resume_root = dir.join(".scratch").join("resume");
        let job_json = std::fs::read_dir(&resume_root)
            .unwrap()
            .map(|entry| entry.unwrap().path().join("job.json"))
            .find(|path| path.exists())
            .expect("the cancelled run must have left a job.json");
        let recorded = std::fs::read_to_string(&job_json).unwrap();
        let tampered = recorded.replace("\"backend\":\"cpu\"", "\"backend\":\"gpu:fake\"");
        assert_ne!(
            tampered, recorded,
            "job.json must record the backend that produced its tiles"
        );
        std::fs::write(&job_json, tampered).unwrap();

        let mut resumed_args: Vec<&str> = args.to_vec();
        resumed_args.push("--resume");
        let resumed = run_cli_in(&dir, &resumed_args);
        assert!(!resumed.success);
        assert_eq!(resumed.code, Some(1));
        assert!(
            resumed.stderr.contains("backend changed"),
            "{}",
            resumed.stderr
        );
    }

    #[test]
    fn resume_with_changed_tile_size_fails() {
        let dir = tmp_dir("job_cli_resume_changed_tile_size");
        let output = dir.join("out.tif");
        let mantis_dir = mantis_512_dir();
        let base_args = [
            "stack",
            mantis_dir.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--algo",
            "laplacian",
            "--no-align",
            "--no-crop",
            "--set",
            "levels=3",
            "--max-bytes-in-flight",
            "1",
        ];
        let mut first_args = base_args.to_vec();
        first_args.push("--tile-size");
        first_args.push("160");
        let mut child = spawn_piped(&dir, &first_args);
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let _first = read_line(&mut stderr);
        send(child.id(), "-INT");
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(130));

        let mut resumed_args = base_args.to_vec();
        resumed_args.push("--tile-size");
        resumed_args.push("224");
        resumed_args.push("--resume");
        let resumed = run_cli_in(&dir, &resumed_args);
        assert!(!resumed.success);
        assert_eq!(resumed.code, Some(1));
        assert!(
            resumed.stderr.contains("tile_size changed"),
            "{}",
            resumed.stderr
        );
    }

    #[test]
    fn exit_codes_distinguish_the_three_outcomes() {
        // Completed: 0.
        let ok_dir = tmp_dir("job_cli_exit_ok");
        let ok_output = ok_dir.join("out.tif");
        let ok = run_cli_in(
            &ok_dir,
            &[
                "stack",
                mantis_512_dir().to_str().unwrap(),
                "-o",
                ok_output.to_str().unwrap(),
                "--algo",
                "laplacian",
                "--no-align",
                "--no-crop",
            ],
        );
        assert_eq!(ok.code, Some(0), "stderr: {}", ok.stderr);

        // Failed: 1.
        let fail_dir = tmp_dir("job_cli_exit_fail");
        let fail_output = fail_dir.join("out.tif");
        let failed = run_cli_in(
            &fail_dir,
            &[
                "stack",
                mantis_512_dir().to_str().unwrap(),
                "-o",
                fail_output.to_str().unwrap(),
                "--algo",
                "does-not-exist",
            ],
        );
        assert_eq!(failed.code, Some(1), "stderr: {}", failed.stderr);

        // Cancelled: 130.
        let cancel_dir = tmp_dir("job_cli_exit_cancel");
        let cancel_output = cancel_dir.join("out.tif");
        let mantis = mantis_512_dir();
        let mut child = spawn_piped(
            &cancel_dir,
            &interruptible_stack_args(mantis.to_str().unwrap(), cancel_output.to_str().unwrap()),
        );
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let _first = read_line(&mut stderr);
        send(child.id(), "-INT");
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(130));
    }

    /// Byte equality without holding two 268 MB composites in memory at
    /// once.
    fn files_are_identical(a: &Path, b: &Path) -> bool {
        use std::io::Read;

        let (mut fa, mut fb) = (
            std::fs::File::open(a).expect("left file must open"),
            std::fs::File::open(b).expect("right file must open"),
        );
        if fa.metadata().unwrap().len() != fb.metadata().unwrap().len() {
            return false;
        }
        let (mut ba, mut bb) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
        loop {
            let n = fa.read(&mut ba).expect("left read");
            fb.read_exact(&mut bb[..n]).expect("right read");
            if n == 0 {
                return true;
            }
            if ba[..n] != bb[..n] {
                return false;
            }
        }
    }

    /// `<dir>/frames/` holding a symlink to each of the reference stack's 61
    /// `IMG_` frames -- the reference stack directory also holds
    /// `Mantis_Stacked.tif`, the reference *result*, which
    /// `focusforge-cli stack` would otherwise take for a 62nd frame.
    fn link_reference_frames(dir: &Path, stack_dir: &Path) -> PathBuf {
        let frames = dir.join("frames");
        std::fs::create_dir_all(&frames).unwrap();
        let mut count = 0;
        for entry in std::fs::read_dir(stack_dir).expect("the reference stack must be readable") {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !name.contains("IMG_") || !name.ends_with(".tif") {
                continue;
            }
            std::os::unix::fs::symlink(&path, frames.join(&name)).unwrap();
            count += 1;
        }
        assert_eq!(count, 61, "IMG_-filtered list must be the 61 source frames");
        frames
    }

    /// The reference-scale acceptance run: `stack` over the 16 GB reference
    /// stack, cancelled with SIGINT part way and resumed to completion,
    /// against an uninterrupted run of the same command.
    ///
    /// `--no-align --no-crop`, deliberately: an anchored alignment over the
    /// 61 frames takes ~240 s *before* the first controllable unit exists,
    /// so a SIGINT during it exercises the deferred pre-alignment path, not
    /// the unit-bounded cancel this test is here to measure.
    ///
    /// Prints the two numbers this pins: the observed **cancel
    /// latency** (signal sent -> process exited) and the resume's
    /// **skipped-unit count** (the first progress line of the resumed run,
    /// which starts at the units the killed run had already logged).
    #[test]
    #[ignore = "reads the 16 GB reference stack"]
    fn stack_reference_with_cancel_and_resume() {
        let Some(stack_dir) = common::reference_stack_dir() else {
            eprintln!("reference stack not found; skipping");
            return;
        };

        let dir = tmp_dir("job_cli_reference_cancel_resume");
        let frames = link_reference_frames(&dir, &stack_dir);
        let frames_arg = frames.to_str().unwrap().to_string();

        // An uninterrupted run first, as the oracle.
        let reference_output = dir.join("reference.tif");
        let reference_start = Instant::now();
        let reference = run_cli_in(
            &dir,
            &[
                "stack",
                &frames_arg,
                "-o",
                reference_output.to_str().unwrap(),
                "--algo",
                "laplacian",
                "--no-align",
                "--no-crop",
            ],
        );
        assert!(reference.success, "stderr: {}", reference.stderr);
        let reference_elapsed = reference_start.elapsed();

        // The interrupted run: SIGINT once a progress line shows the fuse
        // stage genuinely under way -- never a sleep.
        let output = dir.join("out.tif");
        let args = [
            "stack",
            &frames_arg,
            "-o",
            output.to_str().unwrap(),
            "--algo",
            "laplacian",
            "--no-align",
            "--no-crop",
        ];
        let mut child = spawn_piped(&dir, &args);
        let start = Instant::now();
        let (lines, reader) = read_stamped(child.stderr.take().unwrap(), start);
        wait_for(&lines, Duration::from_secs(1800), |line| {
            fuse_counts(line).first().is_some_and(|(done, _)| *done > 0)
        })
        .expect("a unit must complete before the cancel is requested");

        let signalled = Instant::now();
        send(child.id(), "-INT");
        let status = child.wait().expect("child must exit");
        let cancel_latency = signalled.elapsed();
        reader.join().unwrap();

        assert_eq!(status.code(), Some(130), "status: {status:?}");
        assert!(!output.exists(), "the named output must not exist");
        assert!(
            dir.join("out.tif.partial").exists(),
            "the partial output is the resumable state and must survive"
        );
        let ledgers = std::fs::read_dir(dir.join(".scratch").join("resume"))
            .expect("resume dir must exist")
            .count();
        assert_eq!(ledgers, 1, "exactly one ledger must survive the cancel");

        // The resume: the same argv plus `--resume`, nothing else.
        let mut resumed_args: Vec<&str> = args.to_vec();
        resumed_args.push("--resume");
        let resume_start = Instant::now();
        let resumed = run_cli_in(&dir, &resumed_args);
        assert!(resumed.success, "stderr: {}", resumed.stderr);
        let resume_elapsed = resume_start.elapsed();

        let resumed_counts = fuse_counts(&resumed.stderr);
        let (skipped, unit_total) = *resumed_counts
            .first()
            .expect("the resumed run must print a fuse start line");
        assert!(
            skipped > 0,
            "a resume must start above zero -- it skips what the killed run already completed: \
             {}",
            resumed.stderr
        );
        assert!(skipped < unit_total, "the killed run did not finish");
        assert!(
            files_are_identical(&output, &reference_output),
            "a cancelled-then-resumed run must match an uninterrupted one byte for byte"
        );
        assert_eq!(
            std::fs::read_dir(dir.join(".scratch").join("resume"))
                .map(|d| d.count())
                .unwrap_or(0),
            0,
            "a completed run must discard its ledger"
        );

        eprintln!(
            "stack_reference_with_cancel_and_resume: uninterrupted {reference_elapsed:.2?} \
             ({unit_total} tiles), cancel latency {cancel_latency:.2?}, resume skipped \
             {skipped} of {unit_total} tiles in {resume_elapsed:.2?}"
        );
    }

    #[test]
    fn resume_skips_a_recorded_alignment() {
        // Not compared against a separate uninterrupted run: `pyramidal-ecc`
        // reduces in parallel, so two processes can differ in a transform's
        // last digits. Instead, the interrupted run's recorded `alignment.json`
        // is compared against the resumed run's `--json` report -- both drawn
        // from the same recorded `StackAlignment`, never two ECC solves.
        let dir = tmp_dir("job_cli_align_resume");
        let output = dir.join("out.tif");
        let mantis_dir = mantis_512_dir();
        let args = [
            "stack",
            mantis_dir.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--algo",
            "laplacian",
            "--no-crop",
            "--lax",
            "--align-algo",
            "pyramidal-ecc",
            "--tile-size",
            "160",
            "--set",
            "levels=3",
            "--max-bytes-in-flight",
            "1",
        ];
        let mut child = spawn_piped(&dir, &args);
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let align_line = read_line(&mut stderr);
        assert!(align_line.starts_with("align "), "{align_line}");
        let fuse_line = read_line(&mut stderr);
        assert!(fuse_line.starts_with("fuse "), "{fuse_line}");
        send(child.id(), "-INT");
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(130));

        let resume_dir = dir.join(".scratch").join("resume");
        let ledger_dir = std::fs::read_dir(&resume_dir)
            .expect("resume dir must exist")
            .next()
            .expect("exactly one ledger must survive the cancel")
            .unwrap()
            .path();
        let recorded_text = std::fs::read_to_string(ledger_dir.join("alignment.json"))
            .expect("the cancelled run must have recorded its alignment");
        let recorded: serde_json::Value =
            serde_json::from_str(&recorded_text).expect("alignment.json must parse");
        let recorded_frames = recorded["frames"]
            .as_array()
            .expect("alignment.json must carry a frames array");

        let mut resumed_args: Vec<&str> = args.to_vec();
        resumed_args.push("--resume");
        resumed_args.push("--json");
        let resumed = run_cli_in(&dir, &resumed_args);
        assert!(resumed.success, "stderr: {}", resumed.stderr);
        assert!(
            !resumed.stderr.contains("align ") || !resumed.stderr.contains("frames (no unit"),
            "resumed stderr must carry no align stage line: {}",
            resumed.stderr
        );
        let resumed_report: serde_json::Value =
            serde_json::from_str(&resumed.stdout).expect("--json must parse");
        let resumed_alignments = resumed_report["align"]["alignments"]
            .as_array()
            .expect("align.alignments must be an array");
        assert_eq!(resumed_alignments.len(), recorded_frames.len());
        for (resumed_entry, recorded_frame) in resumed_alignments.iter().zip(recorded_frames) {
            assert_eq!(
                resumed_entry["transform"], recorded_frame["transform"],
                "a resumed run must reuse the ledger's exact recorded transform, \
                 never a freshly re-solved one"
            );
        }
    }
}

#[test]
fn tuning_flags_reach_the_scheduler() {
    let default_dir = tmp_dir("job_cli_tuning_default");
    let default_output = default_dir.join("out.tif");
    let default_run = run_cli_in(
        &default_dir,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            default_output.to_str().unwrap(),
            "--algo",
            "laplacian",
            "--no-align",
            "--no-crop",
            "--json",
        ],
    );
    assert!(default_run.success, "stderr: {}", default_run.stderr);
    let default_report: serde_json::Value = serde_json::from_str(&default_run.stdout).unwrap();

    let tuned_dir = tmp_dir("job_cli_tuning_tuned");
    let tuned_output = tuned_dir.join("out.tif");
    let mantis = mantis_512_dir();
    let mut tuned_args =
        interruptible_stack_args(mantis.to_str().unwrap(), tuned_output.to_str().unwrap());
    tuned_args.push("--json");
    let tuned_run = run_cli_in(&tuned_dir, &tuned_args);
    assert!(tuned_run.success, "stderr: {}", tuned_run.stderr);
    let tuned_report: serde_json::Value = serde_json::from_str(&tuned_run.stdout).unwrap();

    assert_eq!(tuned_report["tile_size"], 160);
    assert_eq!(tuned_report["in_flight_limit"], 1);
    assert!(
        tuned_report["tiles"].as_u64().unwrap() > default_report["tiles"].as_u64().unwrap(),
        "a smaller tile size must plan more tiles: tuned={} default={}",
        tuned_report["tiles"],
        default_report["tiles"]
    );
}
