// SPDX-License-Identifier: GPL-3.0-or-later

//! FocusForge's algorithm crate: every operation `focusforge-cli` and,
//! later, the GUI drive, exercisable without a GPU device or a window.
//! `crates/core` never depends on `slint`, `wgpu` or `rfd`.

pub mod algorithm;
pub mod align;
pub mod fuse;
pub mod io;
pub mod job;
pub mod params;
pub mod registry;
pub mod tile;
pub mod zmap;

#[cfg(feature = "test-algorithms")]
pub mod test_algorithms;

pub use io::TiffError;
