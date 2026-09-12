// SPDX-License-Identifier: GPL-3.0-or-later

//! FocusForge's algorithm crate: every operation `focusforge-cli` and,
//! later, the GUI drive, exercisable without a GPU device or a window.
//! `crates/core` never depends on `slint`, `wgpu` or `rfd`.

pub mod io;
pub mod tile;

pub use io::TiffError;
