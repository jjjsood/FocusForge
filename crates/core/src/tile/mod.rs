// SPDX-License-Identifier: GPL-3.0-or-later

//! Tile types and the out-of-core tile scheduler: `plan` is the pure tiling
//! arithmetic over a frame extent, `scratch` is a run-scoped scratch
//! directory, `preprocess` is the per-frame-tile hook, `consumer` is the
//! scheduler-owned sink trait, and `scheduler` is the rayon-parallel driver
//! that ties them together.
//!
//! No API here takes or returns a whole frame.

pub mod budget;
pub mod consumer;
pub mod control;
mod error;
pub mod plan;
pub mod preprocess;
mod sample;
pub mod scheduler;
pub mod scratch;

pub use budget::{
    Clock, HOST_BUDGET_SHARE_PERCENT, HostBudget, HostBudgetGate, HostPermit, HostSources,
    ManualClock, MonotonicClock, PRESSURE_CONFIRMATIONS, PRESSURE_POLL_INTERVAL_MS,
    PRESSURE_SHARE_PERCENT,
};
pub use consumer::{TileConsumer, TileJob};
pub use control::{NoControl, TileControl};
pub use error::TileError;
pub use plan::{PlannedTile, TilePlan};
pub use preprocess::{FramePreprocess, NoopPreprocess};
pub use sample::{Sample, SampleFormat};
pub use scheduler::{ControlledRun, TileRunReport, TileScheduler, TileSchedulerOptions};
pub use scratch::ScratchDir;

/// An axis-aligned rectangle in frame pixel coordinates, `u32` throughout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// The overlap of `self` and `other`, both in the same coordinate space.
    /// `None` when the overlap is empty, including rects that only touch.
    pub fn intersect(self, other: Rect) -> Option<Rect> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = (self.x + self.width).min(other.x + other.width);
        let y1 = (self.y + self.height).min(other.y + other.height);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(Rect {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        })
    }
}

/// An owned crop of one frame's pixels.
///
/// `samples` is chunky, row-major, native-endian `u16` (`f32` for a
/// `Tile<f32>`), with length exactly
/// `rect.width * rect.height * samples_per_pixel`. A borrowed view over the
/// backing `mmap` is not possible in general: `MM`-ordered files need a
/// byte swap, a sub-row rectangle is non-contiguous in the mapping, and mmap
/// bytes are not `u16`-aligned. "Zero-decode" means no decompression, never
/// zero copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tile<S = u16> {
    pub rect: Rect,
    pub samples_per_pixel: u32,
    pub samples: Vec<S>,
}

impl<S> Tile<S> {
    #[inline]
    pub fn width(&self) -> u32 {
        self.rect.width
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.rect.height
    }
}

impl<S: Copy> Tile<S> {
    /// Copy the sub-rectangle `rect` (in the same absolute frame coordinate
    /// space as `self.rect`) out of this tile's samples into `out`.
    ///
    /// Same exact-length, never-clamped contract as
    /// [`crate::io::TiffStack::read_tile_into`]: `rect` must be fully inside
    /// `self.rect`, and `out.len()` must equal exactly
    /// `rect.width * rect.height * samples_per_pixel`. Every consumer needs
    /// this to turn a read tile (with its margin) into its interior.
    pub fn crop_into(&self, rect: Rect, out: &mut [S]) -> Result<(), TileError> {
        let spp = self.samples_per_pixel as usize;
        let in_bounds = rect.width > 0
            && rect.height > 0
            && rect.x >= self.rect.x
            && rect.y >= self.rect.y
            && rect
                .x
                .checked_add(rect.width)
                .is_some_and(|xw| xw <= self.rect.x + self.rect.width)
            && rect
                .y
                .checked_add(rect.height)
                .is_some_and(|yh| yh <= self.rect.y + self.rect.height);
        if !in_bounds {
            return Err(TileError::CropOutOfBounds {
                rect,
                tile: self.rect,
            });
        }

        let expected_len = rect.width as usize * rect.height as usize * spp;
        if out.len() != expected_len {
            return Err(TileError::CropBufferSize {
                expected: expected_len,
                got: out.len(),
            });
        }

        let tile_stride = self.rect.width as usize * spp;
        let row_len = rect.width as usize * spp;
        let x_off = (rect.x - self.rect.x) as usize * spp;
        let mut out_off = 0usize;
        for r in 0..rect.height {
            let src_row = (rect.y - self.rect.y + r) as usize;
            let src_start = src_row * tile_stride + x_off;
            out[out_off..out_off + row_len]
                .copy_from_slice(&self.samples[src_start..src_start + row_len]);
            out_off += row_len;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp_tile(rect: Rect, spp: u32) -> Tile {
        let samples = (0..(rect.width * rect.height * spp))
            .map(|i| i as u16)
            .collect();
        Tile {
            rect,
            samples_per_pixel: spp,
            samples,
        }
    }

    #[test]
    fn crop_into_extracts_the_sub_rect() {
        let tile = ramp_tile(
            Rect {
                x: 10,
                y: 20,
                width: 4,
                height: 4,
            },
            1,
        );
        let crop_rect = Rect {
            x: 11,
            y: 21,
            width: 2,
            height: 2,
        };
        let mut out = vec![0u16; 4];
        tile.crop_into(crop_rect, &mut out).unwrap();
        // Row-major within the 4x4 tile: rows 1..3, cols 1..3.
        assert_eq!(out, vec![5, 6, 9, 10]);
    }

    #[test]
    fn crop_into_rejects_a_rect_outside_the_tile() {
        let tile = ramp_tile(
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            1,
        );
        let crop_rect = Rect {
            x: 3,
            y: 0,
            width: 2,
            height: 4,
        };
        let mut out = vec![0u16; 8];
        match tile.crop_into(crop_rect, &mut out) {
            Err(TileError::CropOutOfBounds { .. }) => {}
            other => panic!("expected CropOutOfBounds, got {other:?}"),
        }
    }

    #[test]
    fn rect_intersect_clips_overlap_and_rejects_touching_or_disjoint() {
        let rect = |x, y, width, height| Rect {
            x,
            y,
            width,
            height,
        };
        let a = rect(10, 20, 8, 6);
        // Partial overlap clips to the shared window, in either order.
        assert_eq!(a.intersect(rect(14, 22, 10, 10)), Some(rect(14, 22, 4, 4)));
        assert_eq!(rect(14, 22, 10, 10).intersect(a), Some(rect(14, 22, 4, 4)));
        // Containment yields the inner rect.
        assert_eq!(a.intersect(rect(11, 21, 2, 2)), Some(rect(11, 21, 2, 2)));
        assert_eq!(rect(11, 21, 2, 2).intersect(a), Some(rect(11, 21, 2, 2)));
        // Edge-touching rects share no pixels.
        assert_eq!(a.intersect(rect(18, 20, 4, 6)), None);
        assert_eq!(a.intersect(rect(10, 26, 8, 3)), None);
        // Disjoint rects.
        assert_eq!(a.intersect(rect(40, 50, 4, 4)), None);
    }

    #[test]
    fn crop_into_rejects_a_wrong_length_buffer() {
        let tile = ramp_tile(
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            1,
        );
        let crop_rect = Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let mut out = vec![0u16; 3];
        match tile.crop_into(crop_rect, &mut out) {
            Err(TileError::CropBufferSize {
                expected: 4,
                got: 3,
            }) => {}
            other => panic!("expected CropBufferSize, got {other:?}"),
        }
    }
}
