// SPDX-License-Identifier: GPL-3.0-or-later

//! The native folder/file picker. [`crate::picker::request`] runs the blocking
//! `rfd::FileDialog` on its own thread and delivers the answer on the event
//! loop, so the window keeps repainting while the dialog is up. macOS keeps
//! the blocking call on the main thread, which AppKit requires and which
//! keeps slint-ui/slint#12437 (macOS-only, an async dialog left open during
//! a drag) away.
//!
//! rfd 0.17.2's own risk
//! (<https://github.com/PolyMeilex/rfd/issues/320>): a cancelled dialog and
//! "no dialog backend available" both return `None` here. Either way
//! nothing opens and the empty state stays on screen.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::entry::RAW_EXTENSIONS;
use crate::job::{ExportFormat, JPEG_EXTENSIONS, PNG_EXTENSIONS, TIFF_EXTENSIONS};

/// Opens the OS folder picker. `None` on cancel (or a missing dialog
/// backend, rfd#320).
pub fn pick_entry() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Open folder or project")
        .pick_folder()
}

/// Opens the OS file picker filtered to `.ffp` -- reopening a saved
/// project.
pub fn pick_project() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Open a FocusForge project")
        .add_filter("FocusForge project", &["ffp"])
        .pick_file()
}

static SAVE_FOLDER: OnceLock<PathBuf> = OnceLock::new();

/// Sends every later project save and image export to `dir`, under its
/// proposed file name, with no dialog. Set once per process; a second call
/// is ignored.
pub fn set_save_folder(dir: PathBuf) {
    let _ = SAVE_FOLDER.set(dir);
}

/// `proposed`'s file name inside the save folder; `None` when no save folder
/// is set.
pub fn in_save_folder(proposed: &Path) -> Option<PathBuf> {
    Some(SAVE_FOLDER.get()?.join(proposed.file_name()?))
}

/// Opens the OS save dialog filtered to `.ffp`, starting at `proposed`'s
/// directory and name. A chosen name without the extension gets it, since
/// GTK's dialog does not add it. `None` on cancel (or a missing dialog
/// backend, rfd#320). A set save folder answers without a dialog.
pub fn pick_save_project(proposed: &Path) -> Option<PathBuf> {
    if let Some(path) = in_save_folder(proposed) {
        return Some(path);
    }
    let mut dialog = rfd::FileDialog::new()
        .set_title("Save project")
        .add_filter("FocusForge project", &["ffp"]);
    if let Some(dir) = proposed.parent() {
        dialog = dialog.set_directory(dir);
    }
    if let Some(name) = proposed.file_name() {
        dialog = dialog.set_file_name(name.to_string_lossy());
    }
    let path = dialog.save_file()?;
    if path.extension().is_some_and(|ext| ext == "ffp") {
        Some(path)
    } else {
        let mut name = path.file_name()?.to_os_string();
        name.push(".ffp");
        Some(path.with_file_name(name))
    }
}

/// The extensions `format`'s files carry, the preferred one first.
fn image_extensions(format: ExportFormat) -> &'static [&'static str] {
    match format {
        ExportFormat::Tiff => TIFF_EXTENSIONS,
        ExportFormat::Png => PNG_EXTENSIONS,
        ExportFormat::Jpeg => JPEG_EXTENSIONS,
    }
}

/// The extension a proposed `format` file name carries.
pub fn default_extension(format: ExportFormat) -> &'static str {
    image_extensions(format)[0]
}

/// Opens the OS save dialog filtered to `format`, starting at `proposed`'s
/// directory and name -- the export modal's own destination picker,
/// mirroring [`pick_save_project`]. A chosen name that does not carry one
/// of the format's extensions gets the preferred one appended. `None` on
/// cancel (or a missing dialog backend, rfd#320). A set save folder answers
/// without a dialog.
pub fn pick_save_image(proposed: &Path, format: ExportFormat) -> Option<PathBuf> {
    if let Some(path) = in_save_folder(proposed) {
        return Some(path);
    }
    let name = format!("{} image", format.id().to_uppercase());
    let mut dialog = rfd::FileDialog::new()
        .set_title("Export image")
        .add_filter(name, image_extensions(format));
    if let Some(dir) = proposed.parent() {
        dialog = dialog.set_directory(dir);
    }
    if let Some(name) = proposed.file_name() {
        dialog = dialog.set_file_name(name.to_string_lossy());
    }
    ensure_extension(dialog.save_file()?, format)
}

/// `path` unchanged when it already carries one of `format`'s extensions
/// (case-insensitive), the preferred one appended to its file name
/// otherwise.
fn ensure_extension(path: PathBuf, format: ExportFormat) -> Option<PathBuf> {
    if ExportFormat::from_extension(&path) == Some(format) {
        Some(path)
    } else {
        let mut name = path.file_name()?.to_os_string();
        name.push(".");
        name.push(default_extension(format));
        Some(path.with_file_name(name))
    }
}

/// Opens the OS file picker, multi-select, filtered to TIFF frames -- the
/// `Images` panel's Add path. `None` on cancel (or a missing dialog backend,
/// rfd#320), the same as its two siblings.
pub fn pick_frames() -> Option<Vec<PathBuf>> {
    rfd::FileDialog::new()
        .set_title("Add frames")
        .add_filter("TIFF frames", &["tif", "tiff"])
        .pick_files()
}

/// `extensions` in lower and upper case: portal filters are case-sensitive
/// globs, and camera files often carry upper-case extensions.
fn both_cases(extensions: &[&str]) -> Vec<String> {
    extensions
        .iter()
        .flat_map(|ext| [ext.to_string(), ext.to_uppercase()])
        .collect()
}

/// Opens the OS file picker, multi-select, filtered to TIFF frames and to
/// camera RAW -- the empty state's `Open…`. `None` on cancel (or a missing
/// dialog backend, rfd#320).
pub fn pick_images() -> Option<Vec<PathBuf>> {
    rfd::FileDialog::new()
        .set_title("Open images")
        .add_filter("TIFF frames", &both_cases(TIFF_EXTENSIONS))
        .add_filter("Camera RAW", &both_cases(RAW_EXTENSIONS))
        .pick_files()
}

/// Which dialog [`request`] opens.
#[derive(Debug, Clone, PartialEq)]
pub enum DialogRequest {
    /// The folder picker.
    Entry,
    /// The `.ffp` file picker.
    Project,
    /// The multi-select TIFF frame picker.
    Frames,
    /// The multi-select TIFF and camera RAW picker that starts a project.
    Images,
    /// The project save dialog, starting at the proposed path.
    SaveProject(PathBuf),
    /// The image export dialog, starting at the proposed path.
    SaveImage(PathBuf, ExportFormat),
}

/// A dialog's answer: `None` on cancel, otherwise the chosen paths -- one
/// for every kind but [`DialogRequest::Frames`] and [`DialogRequest::Images`].
pub type DialogAnswer = Option<Vec<PathBuf>>;

type Continuation = Box<dyn FnOnce(DialogAnswer)>;

thread_local! {
    /// The open dialog's continuation, on the thread that asked.
    static OPEN_DIALOG: RefCell<Option<Continuation>> = const { RefCell::new(None) };
}

fn run(request: DialogRequest) -> DialogAnswer {
    let one = |path: Option<PathBuf>| path.map(|path| vec![path]);
    match request {
        DialogRequest::Entry => one(pick_entry()),
        DialogRequest::Project => one(pick_project()),
        DialogRequest::Frames => pick_frames(),
        DialogRequest::Images => pick_images(),
        DialogRequest::SaveProject(proposed) => one(pick_save_project(&proposed)),
        DialogRequest::SaveImage(proposed, format) => one(pick_save_image(&proposed, format)),
    }
}

/// Opens `request`'s dialog and later calls `answer` on the event loop with
/// the result. `false` -- and nothing opens -- while another dialog is open.
pub fn request(request: DialogRequest, answer: impl FnOnce(DialogAnswer) + 'static) -> bool {
    request_with(request, answer, run, |deliver| {
        // The loop only refuses once it has stopped; no one is left to answer.
        let _ = slint::invoke_from_event_loop(deliver);
    })
}

/// [`request`] with the dialog runner and the event-loop hand-off supplied.
/// `run` and `post` execute on a spawned thread (inline on macOS); `post`
/// must make the closure it is given run on the calling thread.
fn request_with(
    request: DialogRequest,
    answer: impl FnOnce(DialogAnswer) + 'static,
    run: impl FnOnce(DialogRequest) -> DialogAnswer + Send + 'static,
    post: impl FnOnce(Box<dyn FnOnce() + Send>) + Send + 'static,
) -> bool {
    if OPEN_DIALOG.with(|slot| slot.borrow().is_some()) {
        return false;
    }
    OPEN_DIALOG.with(|slot| *slot.borrow_mut() = Some(Box::new(answer)));
    // The slot empties before the continuation runs, so it may ask again.
    fn deliver(answer: DialogAnswer) {
        let continuation = OPEN_DIALOG.with(|slot| slot.borrow_mut().take());
        if let Some(continuation) = continuation {
            continuation(answer);
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = post;
        deliver(run(request));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let spawned = std::thread::Builder::new()
            .name("file-dialog".into())
            .spawn(move || {
                let answer = run(request);
                post(Box::new(move || deliver(answer)));
            });
        if spawned.is_err() {
            deliver(None);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_extension_follows_each_format() {
        let cases = [
            ("/o/a", ExportFormat::Png, "/o/a.png"),
            ("/o/a", ExportFormat::Jpeg, "/o/a.jpg"),
            ("/o/a.TIFF", ExportFormat::Tiff, "/o/a.TIFF"),
            ("/o/a.PNG", ExportFormat::Png, "/o/a.PNG"),
            ("/o/a.JPEG", ExportFormat::Jpeg, "/o/a.JPEG"),
            ("/o/a.jpg", ExportFormat::Jpeg, "/o/a.jpg"),
            ("/o/x.png", ExportFormat::Tiff, "/o/x.png.tif"),
            ("/o/x.jpg", ExportFormat::Png, "/o/x.jpg.png"),
            ("/o/x.tif", ExportFormat::Jpeg, "/o/x.tif.jpg"),
        ];
        for (given, format, expected) in cases {
            assert_eq!(
                ensure_extension(PathBuf::from(given), format),
                Some(PathBuf::from(expected)),
                "{given} under {format:?}"
            );
        }
    }

    #[test]
    fn default_extension_is_the_first_of_each_list() {
        assert_eq!(default_extension(ExportFormat::Tiff), "tif");
        assert_eq!(default_extension(ExportFormat::Png), "png");
        assert_eq!(default_extension(ExportFormat::Jpeg), "jpg");
    }

    #[test]
    fn ensure_extension_appends_only_when_missing() {
        assert_eq!(
            ensure_extension(PathBuf::from("/out/mantis"), ExportFormat::Tiff),
            Some(PathBuf::from("/out/mantis.tif"))
        );
        assert_eq!(
            ensure_extension(PathBuf::from("/out/mantis.tif"), ExportFormat::Tiff),
            Some(PathBuf::from("/out/mantis.tif"))
        );
        assert_eq!(
            ensure_extension(PathBuf::from("/out/mantis.TIFF"), ExportFormat::Tiff),
            Some(PathBuf::from("/out/mantis.TIFF"))
        );
        assert_eq!(
            ensure_extension(PathBuf::from("/out/mantis.png"), ExportFormat::Tiff),
            Some(PathBuf::from("/out/mantis.png.tif"))
        );
    }

    type Posted = Box<dyn FnOnce() + Send>;

    /// The answer the fake runner gives for `request`: two paths for
    /// `Frames` and `Images`, none for the saves, one path otherwise.
    fn fake_answer(request: &DialogRequest) -> Option<Vec<PathBuf>> {
        match request {
            DialogRequest::Entry => Some(vec![PathBuf::from("/pick/folder")]),
            DialogRequest::Project => Some(vec![PathBuf::from("/pick/a.ffp")]),
            DialogRequest::Frames | DialogRequest::Images => {
                Some(vec![PathBuf::from("/f/1.tif"), PathBuf::from("/f/2.tif")])
            }
            DialogRequest::SaveProject(_) | DialogRequest::SaveImage(..) => None,
        }
    }

    fn all_requests() -> Vec<DialogRequest> {
        vec![
            DialogRequest::Entry,
            DialogRequest::Project,
            DialogRequest::Frames,
            DialogRequest::Images,
            DialogRequest::SaveProject(PathBuf::from("/p/a.ffp")),
            DialogRequest::SaveImage(PathBuf::from("/p/a.png"), ExportFormat::Png),
        ]
    }

    #[test]
    fn image_filters_list_each_extension_in_both_cases() {
        assert_eq!(both_cases(&["tif", "tiff"]), ["tif", "TIF", "tiff", "TIFF"]);
    }

    #[test]
    fn dialog_request_returns_before_the_answer() {
        for request in all_requests() {
            let (release, gate) = std::sync::mpsc::channel::<()>();
            let (post_tx, post_rx) = std::sync::mpsc::channel::<Posted>();
            let got = std::rc::Rc::new(std::cell::RefCell::new(None));
            let sink = got.clone();
            let expected = fake_answer(&request);
            let accepted = request_with(
                request.clone(),
                move |answer| *sink.borrow_mut() = Some(answer),
                move |asked| {
                    gate.recv().expect("the test releases the dialog");
                    fake_answer(&asked)
                },
                move |closure| post_tx.send(closure).expect("the test holds the receiver"),
            );
            assert!(accepted, "{request:?} was accepted");
            assert!(
                got.borrow().is_none(),
                "{request:?}: nothing answered while the dialog is open"
            );
            release.send(()).unwrap();
            let posted = post_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("the answer is posted");
            assert!(
                got.borrow().is_none(),
                "{request:?}: posting is not delivering"
            );
            posted();
            assert_eq!(*got.borrow(), Some(expected), "{request:?}");
        }
    }

    #[test]
    fn second_dialog_request_is_ignored_while_one_is_open() {
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let (post_tx, post_rx) = std::sync::mpsc::channel::<Posted>();
        let first = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = first.clone();
        assert!(request_with(
            DialogRequest::Entry,
            move |answer| sink.borrow_mut().push(answer),
            move |asked| {
                gate.recv().unwrap();
                fake_answer(&asked)
            },
            move |closure| post_tx.send(closure).unwrap(),
        ));
        let accepted = request_with(
            DialogRequest::Project,
            |_| panic!("the ignored request's continuation never runs"),
            |_| panic!("the ignored request's dialog never opens"),
            |_| panic!("the ignored request posts nothing"),
        );
        assert!(!accepted);
        release.send(()).unwrap();
        post_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()();
        assert_eq!(*first.borrow(), vec![fake_answer(&DialogRequest::Entry)]);

        let third = std::rc::Rc::new(std::cell::RefCell::new(None));
        let sink = third.clone();
        let (post_tx, post_rx) = std::sync::mpsc::channel::<Posted>();
        assert!(request_with(
            DialogRequest::Frames,
            move |answer| *sink.borrow_mut() = Some(answer),
            |asked| fake_answer(&asked),
            move |closure| post_tx.send(closure).unwrap(),
        ));
        post_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()();
        assert_eq!(*third.borrow(), Some(fake_answer(&DialogRequest::Frames)));
    }
}
