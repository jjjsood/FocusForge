// SPDX-License-Identifier: GPL-3.0-or-later

//! FocusForge's algorithm crate: every operation `focusforge-cli` and,
//! later, the GUI drive, exercisable without a GPU device or a window.
//! `crates/core` never depends on `slint`, `wgpu` or `rfd`.

pub mod algorithm;
pub mod align;
pub mod bytes;
/// The 16-to-8-bit display reduction (no tone curve, ordered dither) and its
/// shared constants -- `crates/gpu`'s WGSL kernel is this module's twin.
pub mod display;
pub mod equalize;
pub mod fuse;
pub mod gpu;
pub mod host;
pub mod io;
pub mod job;
pub mod params;
/// The align -> fuse -> seal spine both front ends drive
/// (`crates/cli`, `crates/gui`) -- one function so a stack run means the
/// same thing regardless of which one started it.
pub mod pipeline;
mod pool;
pub mod project;
pub mod registry;
pub mod retouch;
pub mod tile;
pub mod timing;
pub mod zmap;

#[cfg(feature = "test-algorithms")]
pub mod test_algorithms;

pub use io::TiffError;
