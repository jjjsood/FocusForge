// SPDX-License-Identifier: GPL-3.0-or-later

//! Path helpers shared by this crate's integration tests, resolving upward
//! from `CARGO_MANIFEST_DIR`, the idiom `crates/core/tests/common/mod.rs`
//! already established -- never a literal `../..` string.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use focusforge_gui::job::Op;
use focusforge_gui::{AppController, OpenAs, OpenOutcome, ShellMode};

#[path = "../../../core/tests/common/reference_stack.rs"]
mod reference_stack;
#[allow(unused_imports)]
pub use reference_stack::reference_stack_dir;

/// `crates/gui/../..` -- the workspace root.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// `crates/gui/ui` -- the `.slint` source tree.
///
/// `#[allow(dead_code)]`: each integration test file compiles this shared
/// module into its own binary crate, and not every one of this crate's test
/// binaries calls every helper here (`icons.rs` does not need `ui_dir`).
#[allow(dead_code)]
pub fn ui_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("ui")
}

/// `assets/fonts` at the workspace root.
#[allow(dead_code)]
pub fn fonts_dir() -> PathBuf {
    workspace_root().join("assets").join("fonts")
}

/// `assets/icons` at the workspace root.
#[allow(dead_code)]
pub fn icons_dir() -> PathBuf {
    workspace_root().join("assets").join("icons")
}

/// `crates/gui/src` -- this crate's own hand-written Rust, for source scans
/// (`controller_api_is_slint_free`, `checkpoint_panel_touches_no_storage`).
#[allow(dead_code)]
pub fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every `.slint` file under `dir`, recursively -- no external crate, this
/// crate's own `.slint` tree is small.
#[allow(dead_code)]
pub fn find_slint_files(dir: &Path) -> Vec<PathBuf> {
    find_files_with_extension(dir, "slint")
}

/// Every `.rs` file under `dir`, recursively.
#[allow(dead_code)]
pub fn find_rust_files(dir: &Path) -> Vec<PathBuf> {
    find_files_with_extension(dir, "rs")
}

/// `contents` with every line whose trimmed start is `//` dropped -- code
/// only, so a source-scanning test does not trip on what a comment merely
/// talks about.
#[allow(dead_code)]
pub fn strip_line_comments(contents: &str) -> String {
    contents
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Production Rust before this crate's conventional test-only module. Source
/// architecture scans enforce shipped code, not fixture identifiers.
#[allow(dead_code)]
pub fn production_rust_source(contents: &str) -> &str {
    contents
        .split_once("\n#[cfg(test)]\nmod tests {")
        .map_or(contents, |(production, _)| production)
}

fn find_files_with_extension(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(find_files_with_extension(&path, ext));
        } else if path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path);
        }
    }
    out
}

/// `fixtures/<set>/frames/<index>.tif` -- `set` names a fixture directory
/// under `fixtures/` (`"mantis-64"`, `"mantis-512"`), `index` the frame's own
/// two-digit position in it.
#[allow(dead_code)]
pub fn fixture_frame(set: &str, index: usize) -> PathBuf {
    workspace_root()
        .join("fixtures")
        .join(set)
        .join("frames")
        .join(format!("{index:02}.tif"))
}

/// Copies `mantis-512` fixture frame `source_index` into `dest_dir` as `name`.
#[allow(dead_code)]
pub fn copy_frame(dest_dir: &Path, name: &str, source_index: usize) {
    std::fs::create_dir_all(dest_dir).unwrap();
    std::fs::copy(
        fixture_frame("mantis-512", source_index),
        dest_dir.join(name),
    )
    .expect("fixture frame must copy");
}

/// A fresh, empty renderer cell for `bridge::install`, with no project's
/// planes loaded yet.
#[allow(dead_code)]
pub fn empty_canvas() -> Rc<RefCell<focusforge_gui::bridge::Canvas>> {
    Rc::new(RefCell::new(focusforge_gui::bridge::Canvas::default()))
}

/// A fresh, empty directory under `CARGO_TARGET_TMPDIR`, named `name` --
/// removed first if an earlier run left one behind.
#[allow(dead_code)]
pub fn tree(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp tree must be creatable");
    dir
}

/// A controller with one unsaved project open over `count` copied
/// `mantis-512` frames (`00.tif`, `01.tif`, ... in fixture order), its own
/// scratch root under `CARGO_TARGET_TMPDIR`.
#[allow(dead_code)]
pub fn controller_with_frames(name: &str, count: usize) -> AppController {
    let root = tree(&format!("{name}_frames"));
    for i in 0..count {
        copy_frame(&root, &format!("{i:02}.tif"), i);
    }
    let mut controller = AppController::new();
    assert!(matches!(
        open_and_settle(&mut controller, &[root]),
        OpenOutcome::Opened(_)
    ));
    controller.set_scratch_root_for_test(tree(&format!("{name}_scratch")));
    controller
}

/// Polls `settle_job` until it returns or no work is pending any more,
/// returning the materialized plane paths from the call that actually
/// settled it (if any). Tracks the worker/queue directly, not the active
/// tab's own view of them, so this still waits out a run on a tab other than
/// the one selected. The window's own timer does exactly this poll, just on
/// a Slint `Timer` instead of a plain loop.
#[allow(dead_code)]
pub fn wait_for_settle(
    controller: &mut focusforge_gui::AppController,
    timeout: Duration,
) -> Option<(PathBuf, PathBuf)> {
    let start = Instant::now();
    loop {
        if let Some(planes) = controller.settle_job() {
            settle_session(controller);
            return Some(planes);
        }
        if !controller.work_pending() {
            settle_session(controller);
            return None;
        }
        assert!(
            start.elapsed() < timeout,
            "job did not settle within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Drives an installed window's own repeating job-poll timer
/// ([`focusforge_gui::job::POLL`]) until the run it is polling settles, by
/// alternating a mocked timer tick with a short real sleep. The fuse itself
/// runs on a real worker thread, so `mock_elapsed_time` alone never unblocks
/// it, and the timer tick locks `controller` itself, so the lock here is
/// taken and released between ticks rather than held across the call.
#[allow(dead_code)]
pub fn settle_via_job_timer(controller: &Arc<Mutex<AppController>>, timeout: Duration) {
    let start = Instant::now();
    loop {
        i_slint_backend_testing::mock_elapsed_time(focusforge_gui::job::POLL);
        if !controller.lock().unwrap().work_pending() {
            return;
        }
        assert!(
            start.elapsed() < timeout,
            "job did not settle within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The built GUI, with `FOCUSFORGE_DATA_DIR` pointed at a fresh directory
/// under `CARGO_TARGET_TMPDIR`, named for `test_name` and this process --
/// mirrors `crates/cli/tests/common/mod.rs::cli_bin`, so a windowed test run
/// never touches the developer's own data directory.
#[allow(dead_code)]
pub fn gui_bin(test_name: &str) -> Command {
    let data_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("{test_name}-data-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_focusforge-gui"));
    cmd.env("FOCUSFORGE_DATA_DIR", &data_dir);
    cmd
}

/// `true` when this host can plausibly open a window -- a test that opens
/// the real binary's window skips with an explicit line rather than
/// reddening `cargo test --workspace` on a headless gate host.
#[allow(dead_code)]
pub fn has_display() -> bool {
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Starts a load, blocks until its worker finishes, and settles it --
/// `AppController::open_paths` runs off-thread, so a test that needs the
/// outcome synchronously waits for it here.
#[allow(dead_code)]
pub fn open_and_settle(controller: &mut AppController, paths: &[PathBuf]) -> OpenOutcome {
    if let Some(handle) = controller.open_paths(paths) {
        handle.join().expect("the load worker did not panic");
    }
    controller
        .settle_open()
        .expect("a load was started, so one outcome is parked")
}

/// Starts the active tab's save, blocks until its worker finishes and
/// settles it -- `AppController::start_save_active_project` writes
/// off-thread, so a test that needs the outcome synchronously waits for it
/// here. A save that keeps a modified live as a checkpoint may start the
/// row's background render; it is waited out too, so no render outlives the
/// test. `true` when the save started and wrote.
#[allow(dead_code)]
pub fn save_and_settle(
    controller: &mut AppController,
    pick: impl FnOnce(&Path) -> Option<PathBuf>,
) -> bool {
    let wrote = controller.start_save_active_project(pick) && {
        controller.join_save();
        controller.settle_save() == Some(true)
    };
    while !controller.settle_checkpoint_render() {
        std::thread::sleep(Duration::from_millis(5));
    }
    wrote
}

/// Saves the active tab, which holds one fused run, fuses it again by
/// another method and saves once more: the file, the older fuse's id and
/// the head's. Opening verifies the head's Z-Map only; the older one is read
/// when its row is shown.
#[allow(dead_code)]
pub fn save_two_fuses(controller: &mut AppController) -> (PathBuf, String, String) {
    use focusforge_core::project::Project;

    let mut target = None;
    assert!(save_and_settle(controller, |proposed| {
        target = Some(proposed.to_path_buf());
        target.clone()
    }));
    let path = target.expect("a never-saved tab asks where to save");
    controller.set_fuse_algorithm("depth-map");
    assert!(controller.start_fuse());
    wait_for_settle(controller, Duration::from_secs(120));
    assert!(controller.save_active_project(|_| panic!("a saved tab never asks where")));

    let project = Project::open(&path).expect("the saved project opens");
    let head = project.checkpoint().clone();
    let older = head.parent.clone().expect("the second fuse has a parent");
    let older_blob = &project
        .history()
        .get(&older)
        .expect("the parent is saved")
        .zmap_blob;
    assert_ne!(
        *older_blob, head.zmap_blob,
        "the two methods map differently"
    );
    (path, older, head.id)
}

/// Rewrites the saved project at `path` with the entry of blob `hash`
/// replaced by bytes no codec decodes: the project still opens when `hash`
/// is not the head's Z-Map, and only a read of that blob fails.
#[allow(dead_code)]
pub fn corrupt_blob(path: &Path, hash: &str) {
    use focusforge_core::project::container::{ContainerWriter, Index};

    let target = format!("blobs/{hash}.zst");
    let index = Index::read(path).expect("the project's container reads");
    let rewritten = path.with_extension("rewritten");
    let mut writer = ContainerWriter::create(&rewritten).expect("the copy is created");
    for name in index.names() {
        let bytes = if name == target {
            b"not a zstd frame".to_vec()
        } else {
            index.read_entry(path, name).expect("the entry reads")
        };
        writer.put_bytes(name, &bytes).expect("the entry is copied");
    }
    writer.finish().expect("the copy is sealed");
    std::fs::rename(&rewritten, path).expect("the copy replaces the project");
}

/// Answers the pending grouping question, blocks until its worker finishes,
/// and settles it -- `AppController::answer_question` runs off-thread too,
/// the same as [`open_and_settle`].
#[allow(dead_code)]
pub fn answer_and_settle(controller: &mut AppController, answer: OpenAs) -> OpenOutcome {
    if let Some(handle) = controller.answer_question(answer) {
        handle.join().expect("the load worker did not panic");
    }
    controller
        .settle_open()
        .expect("a load was started, so one outcome is parked")
}

/// Starts an additive import, blocks until its worker finishes, and settles
/// it -- `AppController::add_frames` runs off-thread too, the same as
/// [`open_and_settle`]. Propagates a refusal without joining or settling
/// anything, since [`AppController::add_frames`] never spawns a worker for
/// one.
#[allow(dead_code)]
pub fn add_and_settle(
    controller: &mut AppController,
    paths: &[PathBuf],
) -> Result<(), focusforge_gui::entry::ImportRefused> {
    if let Some(handle) = controller.add_frames(paths)? {
        handle.join().expect("the import worker did not panic");
    }
    controller.settle_import();
    Ok(())
}

/// Starts a `Locate frames…` verification, blocks until its worker
/// finishes, and settles it -- `AppController::locate_frames` runs
/// off-thread too, the same as [`open_and_settle`]. A no-op with nothing to
/// verify against.
#[allow(dead_code)]
pub fn locate_and_settle(
    controller: &mut AppController,
    target: focusforge_gui::entry::LocateTarget,
    folder: PathBuf,
) {
    if let Some(handle) = controller.locate_frames(target, folder) {
        handle.join().expect("the locate worker did not panic");
    }
    controller.settle_locate();
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

/// A `<root>/focusforge` data directory holding roughly `total_log_bytes`
/// across a live log and three rotated siblings, one JSON Lines record per
/// line, plus one crash report -- large enough that a synchronous manifest
/// scan over it costs measurable time.
#[allow(dead_code)]
pub fn seed_large_diagnostics_dir(root: &Path, total_log_bytes: usize) -> PathBuf {
    let data = root.join("focusforge");
    std::fs::create_dir_all(data.join("logs")).unwrap();
    std::fs::create_dir_all(data.join("crashes")).unwrap();
    let line = "{\"level\":\"INFO\",\"target\":\"focusforge_gui\",\"message\":\"tick\"}\n";
    let lines_per_file = (total_log_bytes / 4) / line.len();
    let content = line.repeat(lines_per_file);
    std::fs::write(data.join("logs/focusforge.jsonl"), &content).unwrap();
    for n in 1..=3 {
        std::fs::write(data.join(format!("logs/focusforge.{n}.jsonl")), &content).unwrap();
    }
    std::fs::write(
        data.join("crashes/20260918T010203Z-focusforge-gui-1.dmp"),
        b"MDMP",
    )
    .unwrap();
    data
}

/// Appends `op` to the active tab's edit session.
#[allow(dead_code)]
pub fn paint(controller: &mut AppController, op: Op) {
    controller
        .edit_session_mut()
        .expect("the tab has an edit session")
        .append(op)
        .expect("a stroke inside the map applies");
}

/// Polls `settle_checkpoint_render` until the checkpoint render has settled.
#[allow(dead_code)]
pub fn settle_render(controller: &mut AppController) {
    let start = Instant::now();
    while !controller.settle_checkpoint_render() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the checkpoint render did not settle"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Polls `settle_edit_session` until no editor session is being opened.
#[allow(dead_code)]
pub fn settle_session(controller: &mut AppController) {
    let start = Instant::now();
    while !controller.settle_edit_session() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the editor session did not settle"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Polls `settle_region_renders` until no stroke, undo or redo has a region render
/// in flight.
#[allow(dead_code)]
pub fn settle_region_renders(controller: &mut AppController) {
    let start = Instant::now();
    while !controller.settle_region_renders() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the region renders did not settle"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Polls `settle_thumbnails` until every picture decode has been applied.
#[allow(dead_code)]
pub fn settle_thumbnails(controller: &mut AppController) {
    let start = Instant::now();
    while !controller.settle_thumbnails() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the thumbnail decodes did not settle"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// `set_mode(Editor)`, then waits out the render an op-bearing row needs and
/// the session's open.
#[allow(dead_code)]
pub fn enter_editor(controller: &mut AppController) {
    assert!(
        controller.set_mode(ShellMode::Editor),
        "the editor did not open"
    );
    let start = Instant::now();
    while controller.edit_session().is_none() {
        assert_eq!(
            controller.mode(),
            ShellMode::Editor,
            "the session failed to open"
        );
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the editor session did not open"
        );
        controller.settle_checkpoint_render();
        controller.settle_edit_session();
        std::thread::sleep(Duration::from_millis(5));
    }
}
