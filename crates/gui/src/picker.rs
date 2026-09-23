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

/// Opens the OS save dialog filtered to `.ffp`, starting at `proposed`'s
/// directory and name. A chosen name without the extension gets it, since
/// GTK's dialog does not add it. `None` on cancel (or a missing dialog
/// backend, rfd#320).
pub fn pick_save_project(proposed: &Path) -> Option<PathBuf> {
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

/// Opens the OS file picker, multi-select, filtered to TIFF frames -- the
/// `Images` panel's Add path. `None` on cancel (or a missing dialog backend,
/// rfd#320), the same as its two siblings.
pub fn pick_frames() -> Option<Vec<PathBuf>> {
    rfd::FileDialog::new()
        .set_title("Add frames")
        .add_filter("TIFF frames", &["tif", "tiff"])
        .pick_files()
}
