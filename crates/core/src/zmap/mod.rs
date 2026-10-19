// SPDX-License-Identifier: GPL-3.0-or-later

//! The Z-Map: one `u16` per pixel naming the source frame that contributed
//! the detail there. **The Z-Map is the document**: fusion emits the composite image *and* this
//! map, retouching paints indices, not pixels, and no edit path writes one directly.
//! [`crate::project`] already snapshots it into a checkpoint -- one content-addressed blob, named
//! by [`crate::project::Checkpoint::zmap_blob`].
//!
//! [`NO_SOURCE`] is legal everywhere and every consumer must *handle* it --
//! forgetting it is a silent wrong-pixel bug, not a compile error: the
//! palette needs a defined colour for it.
//!
//! Two traps a reader will otherwise hit:
//!
//! - **Never write a Z-Map into a composite's directory.** `TiffStack::open`
//!   takes every `*.tif` in a directory and its `check_uniform` step rejects
//!   a `samples_per_pixel = 1` map mixed in with `samples_per_pixel = 3`
//!   frames with `FrameMismatch` -- a Z-Map's output path must be named
//!   explicitly, never left to fall inside a frames directory that also
//!   gets scanned as a stack.
//! - **A Z-Map looks near-black in an ordinary viewer.** `photometric` is
//!   forced to 1 (BlackIsZero) for `samples_per_pixel < 3`, and indices in
//!   `0..60` sit at the bottom of a `0..65535` range. The inspection path is
//!   `tools/tiff-diff.py --zmap`, not an image viewer.
//!
//! Never materialize the whole map: validation is per-tile on read, plus an
//! explicit opt-in tile-wise scan ([`ZMapReader::validate_all`]).
//! [`ZMapReader::open`] never scans.

mod description;
mod error;
pub mod metrics;
mod palette;
mod reader;
mod writer;

use crate::tile::Rect;

pub use description::{format_description, parse_description};
pub use error::ZMapError;
pub use metrics::{SpreadScan, ZMapSpread, all_eight_identical_fraction};
pub use palette::{overlay_table, source_color};
pub use reader::ZMapReader;
pub use writer::ZMapWriter;

/// The reserved "no single source" index: legal at any frame count, on both
/// the write-side and read-side validators. Weighted Average records its
/// dominant contributor and does not emit this sentinel; every consumer
/// must still handle it.
pub const NO_SOURCE: u16 = u16::MAX;

/// The largest frame count a Z-Map can claim. `u16::MAX` is reserved as
/// [`NO_SOURCE`], so the stack bound is one below the index type's range.
pub const MAX_FRAMES: u32 = 65_534;

/// The facts a Z-Map claims about itself: its extent and how many source
/// frames it names. This *is* "the Z-Map type" in the only form the
/// no-whole-map rule allows -- the map's facts plus tile accessors, never
/// its pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZMapInfo {
    width: u32,
    height: u32,
    frame_count: u32,
}

impl ZMapInfo {
    /// Refuses a zero extent ([`ZMapError::EmptyExtent`]) and a frame count
    /// outside `1..=MAX_FRAMES` ([`ZMapError::FrameCountOutOfRange`]).
    pub fn new(width: u32, height: u32, frame_count: u32) -> Result<Self, ZMapError> {
        if width == 0 || height == 0 {
            return Err(ZMapError::EmptyExtent { width, height });
        }
        if frame_count == 0 || frame_count > MAX_FRAMES {
            return Err(ZMapError::FrameCountOutOfRange {
                frame_count,
                max: MAX_FRAMES,
            });
        }
        Ok(ZMapInfo {
            width,
            height,
            frame_count,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn frame_count(&self) -> u32 {
        self.frame_count
    }

    /// Whether `rect` lies fully inside this map's extent.
    pub fn contains(&self, rect: Rect) -> bool {
        rect.width > 0
            && rect.height > 0
            && rect
                .x
                .checked_add(rect.width)
                .is_some_and(|xw| xw <= self.width)
            && rect
                .y
                .checked_add(rect.height)
                .is_some_and(|yh| yh <= self.height)
    }

    /// The number of `u16` indices a tile over `rect` holds -- one per
    /// *pixel*, never `* samples_per_pixel`, matching the invariant
    /// `TileOutput::zmap` already pins.
    pub fn tile_len(&self, rect: Rect) -> usize {
        rect.width as usize * rect.height as usize
    }
}

/// The single validator both the write path and the read path call, so the
/// reserved-value clause cannot drift between them. One pass, first
/// offender wins: any index that is not [`NO_SOURCE`] and is `>=
/// frame_count` is [`ZMapError::IndexOutOfRange`].
pub fn validate_tile(indices: &[u16], frame_count: u32) -> Result<(), ZMapError> {
    for &index in indices {
        if index != NO_SOURCE && u32::from(index) >= frame_count {
            return Err(ZMapError::IndexOutOfRange { index, frame_count });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_a_zero_extent() {
        match ZMapInfo::new(0, 10, 5) {
            Err(ZMapError::EmptyExtent {
                width: 0,
                height: 10,
            }) => {}
            other => panic!("expected EmptyExtent, got {other:?}"),
        }
        match ZMapInfo::new(10, 0, 5) {
            Err(ZMapError::EmptyExtent {
                width: 10,
                height: 0,
            }) => {}
            other => panic!("expected EmptyExtent, got {other:?}"),
        }
    }

    #[test]
    fn new_rejects_a_zero_frame_count() {
        match ZMapInfo::new(4, 4, 0) {
            Err(ZMapError::FrameCountOutOfRange {
                frame_count: 0,
                max: MAX_FRAMES,
            }) => {}
            other => panic!("expected FrameCountOutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn new_rejects_one_past_max_frames() {
        match ZMapInfo::new(4, 4, MAX_FRAMES + 1) {
            Err(ZMapError::FrameCountOutOfRange { frame_count, max }) => {
                assert_eq!(frame_count, 65_535);
                assert_eq!(max, MAX_FRAMES);
            }
            other => panic!("expected FrameCountOutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn contains_rejects_a_rect_outside_the_extent() {
        let info = ZMapInfo::new(10, 10, 5).unwrap();
        assert!(!info.contains(Rect {
            x: 5,
            y: 5,
            width: 6,
            height: 1
        }));
        assert!(!info.contains(Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 1
        }));
        assert!(info.contains(Rect {
            x: 5,
            y: 5,
            width: 5,
            height: 5
        }));
    }

    #[test]
    fn tile_len_is_w_times_h_never_times_spp() {
        let info = ZMapInfo::new(10, 10, 5).unwrap();
        let rect = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 3,
        };
        assert_eq!(info.tile_len(rect), 12);
    }

    #[test]
    fn rejects_out_of_range_index() {
        match validate_tile(&[0, 1, 4], 4) {
            Err(ZMapError::IndexOutOfRange {
                index: 4,
                frame_count: 4,
            }) => {}
            other => panic!("expected IndexOutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn reserved_index_is_not_out_of_range() {
        validate_tile(&[NO_SOURCE], 1).expect("NO_SOURCE must be legal at frame_count = 1");
        validate_tile(&[NO_SOURCE], MAX_FRAMES)
            .expect("NO_SOURCE must be legal at frame_count = MAX_FRAMES");
    }

    #[test]
    fn parse_description_over_the_four_failure_shapes() {
        use std::path::Path;
        let path = Path::new("test.tif");

        match parse_description(path, None) {
            Err(ZMapError::MissingDescription { .. }) => {}
            other => panic!("expected MissingDescription, got {other:?}"),
        }

        match parse_description(path, Some("FocusForge 0.1.0")) {
            Err(ZMapError::NotAZMap { .. }) => {}
            other => panic!("expected NotAZMap, got {other:?}"),
        }

        match parse_description(path, Some("FocusForge zmap v1")) {
            Err(ZMapError::MissingFrameCount { .. }) => {}
            other => panic!("expected MissingFrameCount, got {other:?}"),
        }

        match parse_description(path, Some("FocusForge zmap v1 frames=not-a-number")) {
            Err(ZMapError::BadFrameCount { .. }) => {}
            other => panic!("expected BadFrameCount, got {other:?}"),
        }
    }

    #[test]
    fn parse_description_ignores_unknown_key_tokens() {
        use std::path::Path;
        let frame_count = parse_description(
            Path::new("test.tif"),
            Some("FocusForge zmap v1 frames=4 producer=laplacian"),
        )
        .expect("unknown key= tokens must be ignored");
        assert_eq!(frame_count, 4);
    }
}
