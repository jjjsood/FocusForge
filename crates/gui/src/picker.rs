// SPDX-License-Identifier: GPL-3.0-or-later

//! The native folder/file picker: blocking, never `AsyncFileDialog`, which
//! sidesteps slint-ui/slint#12437 (an open, macOS-only bug from an async
//! dialog left open during a drag) by construction. Freezing the shell's
//! own repaints while the dialog is up is correct: the portal dialog is a
//! separate process with its own window.
//!
//! rfd 0.17.2's own risk
//! (<https://github.com/PolyMeilex/rfd/issues/320>): a cancelled dialog and
//! "no dialog backend available" both return `None` here. Either way
//! nothing opens and the empty state stays on screen.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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
}
