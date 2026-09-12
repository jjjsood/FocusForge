// SPDX-License-Identifier: GPL-3.0-or-later

//! Tile types and the out-of-core tile scheduler: `plan` is the pure
//! tiling arithmetic over a frame extent, `scratch` is a run-scoped scratch
//! directory, `preprocess` is the (currently empty) per-frame-tile hook,
//! `consumer` is the scheduler-owned sink trait, and `scheduler` is the
//! rayon-parallel driver that ties them together.
//!
//! No API here takes or returns a whole frame.

/// An axis-aligned rectangle in frame pixel coordinates, `u32` throughout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// An owned crop of one frame's pixels.
///
/// `samples` is chunky, row-major, native-endian `u16`, with length exactly
/// `rect.width * rect.height * samples_per_pixel`. A borrowed view over the
/// backing `mmap` is not possible in general: `MM`-ordered files need a
/// byte swap, a sub-row rectangle is non-contiguous in the mapping, and mmap
/// bytes are not `u16`-aligned. "Zero-decode" means no decompression, never
/// zero copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tile {
    pub rect: Rect,
    pub samples_per_pixel: u32,
    pub samples: Vec<u16>,
}

impl Tile {
    #[inline]
    pub fn width(&self) -> u32 {
        self.rect.width
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.rect.height
    }
}
