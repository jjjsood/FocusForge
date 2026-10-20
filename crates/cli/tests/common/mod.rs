// SPDX-License-Identifier: GPL-3.0-or-later

//! Path helpers shared by this crate's integration tests, resolving upward
//! from `CARGO_MANIFEST_DIR` -- never a bare relative string, since a test
//! binary's own cwd is not the workspace root.

use std::path::{Path, PathBuf};
use std::process::Command;

use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter};

#[path = "../../../core/tests/common/reference_stack.rs"]
mod reference_stack;
#[allow(unused_imports)]
pub use reference_stack::reference_stack_dir;

/// `crates/cli/../..` -- the workspace root.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// `crates/cli/../../fixtures` -- the workspace's committed fixture trees.
///
/// `#[allow(dead_code)]`: each integration test file compiles this shared
/// module into its own binary crate, and not every one of this crate's test
/// binaries calls this helper (`acceptance.rs` reads `.scratch/` and the
/// reference stack directly, never a fixture).
#[allow(dead_code)]
pub fn fixtures_dir() -> PathBuf {
    workspace_root().join("fixtures")
}

/// A fresh, empty directory, removed again when the test that made it ends
/// -- used as a spawned process's own working directory, so `.scratch/`
/// lands here and never collides with another test's.
///
/// Derefs to `Path`, so `&dir` and `dir.join(..)` read exactly as a bare
/// `PathBuf` would.
///
/// `#[allow(dead_code)]`: not every test binary in this crate calls
/// [`tmp_dir`].
#[allow(dead_code)]
pub struct TmpDir {
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

/// **Named per process, not just per test.** `CARGO_TARGET_TMPDIR` can be
/// shared by concurrent `cargo test` runs, so without the pid two runs of
/// the same test name raced on the same `.scratch/resume/` ledger: `error:
/// .scratch/resume…: No such file or directory` and a `stack` run exiting 1
/// where it should exit 0, reproduced 3 rounds out of 3.
///
/// `#[allow(dead_code)]`: not every test binary in this crate calls this
/// helper.
#[allow(dead_code)]
pub fn tmp_dir(name: &str) -> TmpDir {
    let path =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    TmpDir { path }
}

/// Appends `--cpu` to `args` when this crate's own `gpu` feature is
/// compiled in -- a no-op build has no such flag at all, not even as a
/// no-op (`crates/cli/src/main.rs`'s own `#[cfg(feature = "gpu")]` on the
/// arg). `gpu` is a default build feature, so any test that names a
/// real algorithm (`laplacian`, `depth-map`) and depends on running the CPU
/// tile scheduler -- exact-byte equality against a CPU oracle, deterministic
/// single-worker pause/resume timing -- must call this or silently start
/// exercising the GPU path's ±1 LSB budget instead.
///
/// `#[allow(dead_code)]`: not every test binary in this crate calls this
/// helper.
#[allow(dead_code)]
pub fn push_no_gpu(mut args: Vec<&str>) -> Vec<&str> {
    if cfg!(feature = "gpu") {
        args.push("--cpu");
    }
    args
}

/// The built CLI, with `FOCUSFORGE_DATA_DIR` pointed at a fresh directory
/// per `test_name` and process, so a spawned run never touches the
/// developer's own data directory and two parallel test binaries never
/// share one.
#[allow(dead_code)]
pub fn cli_bin(test_name: &str) -> Command {
    let data_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("{test_name}-data-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_focusforge-cli"));
    cmd.env("FOCUSFORGE_DATA_DIR", &data_dir);
    cmd
}

/// The `FOCUSFORGE_DATA_DIR` [`cli_bin`] set on `cmd`.
///
/// `#[allow(dead_code)]`: not every test binary in this crate calls this
/// helper.
#[allow(dead_code)]
pub fn data_dir_of(cmd: &Command) -> PathBuf {
    cmd.get_envs()
        .find(|(k, _)| *k == "FOCUSFORGE_DATA_DIR")
        .and_then(|(_, v)| v)
        .map(PathBuf::from)
        .expect("cli_bin always sets FOCUSFORGE_DATA_DIR")
}

/// `crates/cli/../../fixtures/mantis-512` -- the 512-pixel, multi-frame
/// fixture tree.
///
/// `#[allow(dead_code)]`: not every test binary in this crate calls this
/// helper.
#[allow(dead_code)]
pub fn mantis_512_dir() -> PathBuf {
    fixtures_dir().join("mantis-512")
}

/// The finished process's status and both byte streams, decoded lossily.
///
/// `#[allow(dead_code)]`: each integration test file compiles this shared
/// module into its own binary crate, and not every one reads every field.
#[allow(dead_code)]
pub struct CliOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl From<std::process::Output> for CliOutput {
    fn from(output: std::process::Output) -> Self {
        CliOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

/// Runs `cmd` with `args` and `dir` as its working directory to completion.
///
/// `#[allow(dead_code)]`: not every test binary in this crate calls this
/// helper.
#[allow(dead_code)]
pub fn run_in(cmd: &mut Command, dir: &Path, args: &[&str]) -> CliOutput {
    cmd.args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run binary")
        .into()
}

/// The rectangle covering a `width` x `height` image.
///
/// `#[allow(dead_code)]`: not every test binary in this crate calls this
/// helper.
#[allow(dead_code)]
pub fn full_rect(width: u32, height: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

/// Whole-image read: samples, width, height, samples-per-pixel.
///
/// `#[allow(dead_code)]`: not every test binary in this crate calls this
/// helper.
#[allow(dead_code)]
pub fn read_whole(path: &Path) -> (Vec<u16>, u32, u32, u32) {
    let stack = TiffStack::open_files(&[path.to_path_buf()]).expect("output must reopen");
    let (w, h) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let samples = stack
        .read_tile(0, full_rect(w, h))
        .expect("read must succeed")
        .samples;
    (samples, w, h, spp)
}

/// A directory built with `tempfile`, which honours `TMPDIR` and so lives on
/// a filesystem where a mode change actually takes effect. Restores a
/// writable mode before its own `TempDir` removes it.
#[allow(dead_code)]
#[cfg(unix)]
pub struct ReadOnlyDir {
    dir: tempfile::TempDir,
}

#[cfg(unix)]
impl std::ops::Deref for ReadOnlyDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.dir.path()
    }
}

#[cfg(unix)]
impl Drop for ReadOnlyDir {
    fn drop(&mut self) {
        let _ = set_mode(self.dir.path(), 0o755);
    }
}

#[cfg(unix)]
fn set_mode(dir: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))
}

/// A file made unreadable (mode `000`) for the guard's lifetime. Dropping it,
/// on success or while a failed assertion unwinds, restores a readable mode so
/// nothing unreadable outlives the test.
#[allow(dead_code)]
#[cfg(unix)]
pub struct UnreadableFile {
    path: PathBuf,
}

#[cfg(unix)]
impl UnreadableFile {
    #[allow(dead_code)]
    pub fn lock(path: &Path) -> std::io::Result<Self> {
        set_mode(path, 0o000)?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

#[cfg(unix)]
impl Drop for UnreadableFile {
    fn drop(&mut self) {
        let _ = set_mode(&self.path, 0o644);
    }
}

/// Tries to create and remove a marker file in `dir`; `true` means the
/// directory is still writable, so a mode change meant to block that either
/// failed or has no effect on this filesystem.
#[cfg(unix)]
fn still_writable(dir: &Path) -> bool {
    let marker = dir.join(".readonly-probe");
    match std::fs::write(&marker, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&marker);
            true
        }
        Err(_) => false,
    }
}

/// A fresh directory: `populate` fills it in while still writable, then
/// `mode` is applied and proven to actually block writes. When that proof
/// fails, undoes the mode change, prints a one-line skip notice to stderr,
/// and returns `None`.
#[allow(dead_code)]
#[cfg(unix)]
pub fn readonly_dir(label: &str, mode: u32, populate: impl FnOnce(&Path)) -> Option<ReadOnlyDir> {
    let dir = tempfile::Builder::new()
        .prefix(label)
        .tempdir()
        .expect("temp directory must be creatable");
    populate(dir.path());
    set_mode(dir.path(), mode).expect("mode must be settable");
    if still_writable(dir.path()) {
        let _ = set_mode(dir.path(), 0o755);
        eprintln!("skipping {label}: this filesystem does not enforce directory permissions");
        return None;
    }
    Some(ReadOnlyDir { dir })
}

/// Sinusoids of 17 to 751 px wavelength: textured at every scale of the
/// alignment pyramid and never periodic over a frame. `channel` offsets every
/// phase, so the three planes differ.
fn synthetic_scene(x: f64, y: f64, channel: usize) -> f64 {
    const RIPPLES: [(f64, f64, f64, f64); 10] = [
        (1.0 / 613.0, 1.0 / 751.0, 0.7, 1.6),
        (-1.0 / 487.0, 1.0 / 529.0, 2.9, 1.4),
        (1.0 / 97.0, 1.0 / 131.0, 0.3, 1.0),
        (1.0 / 61.0, -1.0 / 83.0, 1.7, 0.8),
        (-1.0 / 47.0, 1.0 / 71.0, 4.1, 0.6),
        (1.0 / 113.0, 1.0 / 37.0, 2.4, 0.7),
        (1.0 / 23.0, 1.0 / 29.0, 0.9, 0.5),
        (-1.0 / 19.0, 1.0 / 17.0, 5.0, 0.4),
        (1.0 / 31.0, 1.0 / 43.0, 3.3, 0.5),
        (-1.0 / 41.0, -1.0 / 27.0, 1.1, 0.4),
    ];
    RIPPLES
        .iter()
        .map(|&(fx, fy, phase, amp)| {
            amp * (std::f64::consts::TAU * (fx * x + fy * y) + phase + channel as f64).sin()
        })
        .sum()
}

/// Writes 16-bit RGB frames `frame-00.tif`, ... into the existing directory
/// `dir`: frame `i` is one analytic textured scene translated by `shifts[i]`
/// pixels, an exact translate of the first at zero shift.
#[allow(dead_code)]
pub fn write_synthetic_stack(dir: &Path, width: u32, height: u32, shifts: &[(f64, f64)]) {
    for (index, &(dx, dy)) in shifts.iter().enumerate() {
        let mut samples = Vec::with_capacity(width as usize * height as usize * 3);
        for y in 0..height {
            for x in 0..width {
                for channel in 0..3 {
                    let v = synthetic_scene(f64::from(x) - dx, f64::from(y) - dy, channel);
                    samples.push((32768.0 + 6000.0 * v).clamp(0.0, 65535.0) as u16);
                }
            }
        }
        let mut writer = TiffWriter::create(
            &dir.join(format!("frame-{index:02}.tif")),
            TiffSpec {
                width,
                height,
                samples_per_pixel: 3,
                description: None,
            },
        )
        .expect("synthetic frame writer must create");
        writer
            .write_tile(full_rect(width, height), &samples)
            .expect("synthetic frame writer must accept the full tile");
        writer.finish().expect("synthetic frame writer must finish");
    }
}
