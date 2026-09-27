// SPDX-License-Identifier: GPL-3.0-or-later

//! [`WarpingReader`]: the [`TileReader`] decorator that warps each read by
//! a per-frame transform without touching the scheduler. Not a
//! [`crate::tile::FramePreprocess`] impl: that hook is unused here.

use crate::algorithm::{AlgorithmError, TileReader};
use crate::align::Transform;
use crate::align::warp::warp_tile;
use crate::tile::Rect;
use crate::timing::{TimingCounter, measurement_scope, record_timing};

/// Warps every read through `inner` by a per-frame [`Transform`]
/// (reference coordinates in, `inner`'s own frame coordinates out). Index
/// `i` of `transforms` is frame `i`'s reference-to-frame transform, the
/// same convention and the same per-frame ordering
/// [`crate::align::StackAlignment::frames`] already carries.
///
/// Adds no clamping of its own: [`Transform::source_footprint`] already
/// grows by [`crate::align::KERNEL_RADIUS`] and clamps to the frame
/// (`crate::align::transform`'s module docs), returning
/// [`crate::align::SourceFootprint::Outside`] when the clamp empties the
/// interval, which [`warp_tile`] answers with an all-zero, all-uncovered
/// tile without reading. This decorator only ever hands `inner` an
/// already-clamped rect.
pub struct WarpingReader<'a> {
    inner: &'a dyn TileReader,
    frame: (u32, u32),
    samples_per_pixel: u32,
    /// Index == frame index.
    transforms: Vec<Transform>,
}

impl<'a> WarpingReader<'a> {
    pub fn new(
        inner: &'a dyn TileReader,
        frame: (u32, u32),
        samples_per_pixel: u32,
        transforms: Vec<Transform>,
    ) -> Self {
        WarpingReader {
            inner,
            frame,
            samples_per_pixel,
            transforms,
        }
    }
}

impl TileReader for WarpingReader<'_> {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        let _scope = measurement_scope("warp_read");
        let t = self
            .transforms
            .get(frame)
            .ok_or(AlgorithmError::FrameOutOfRange {
                frame,
                frame_count: self.transforms.len(),
            })?;

        // Identity short-circuit: bit-identical to a direct read, because
        // the Catmull-Rom kernel is exactly `[0, 1, 0, 0]` at `t == 0`
        // (`crate::align::warp`'s module docs) -- proved by
        // `warping_reader_identity_matches_a_direct_read`
        // (`crates/core/tests/warp.rs`), not merely asserted here. This is
        // what keeps a `--no-align` run, and every reference-frame read
        // during an aligned one, bit-identical to a direct `TiffStack`
        // read.
        if *t == Transform::identity() {
            return self.inner.read_tile_into(frame, rect, out);
        }

        let warped = warp_tile(
            self.inner,
            frame,
            self.frame,
            self.samples_per_pixel,
            rect,
            t,
        )
        .map_err(|source| AlgorithmError::Failed {
            algorithm: "warp".to_string(),
            detail: source.to_string(),
        })?;

        if out.len() != warped.tile.samples.len() {
            return Err(AlgorithmError::Failed {
                algorithm: "warp".to_string(),
                detail: format!(
                    "buffer size mismatch: expected {} u16 samples, got {}",
                    warped.tile.samples.len(),
                    out.len()
                ),
            });
        }
        out.copy_from_slice(&warped.tile.samples);
        let bytes = std::mem::size_of_val(out) as u64;
        record_timing(TimingCounter::WarpBytes, bytes);
        record_timing(TimingCounter::CopyBytes, bytes);
        record_timing(TimingCounter::CopyOperations, 1);
        Ok(())
    }
}
