// SPDX-License-Identifier: GPL-3.0-or-later

//! [`WarpingReader`]: the [`TileReader`] decorator that warps each read by
//! a per-frame transform without touching the scheduler. Not a
//! [`crate::tile::FramePreprocess`] impl: that hook is unused here.

use serde::{Deserialize, Serialize};

use crate::algorithm::{AlgorithmError, TileReader};
use crate::align::warp::warp_tile;
use crate::align::{SourceFootprint, Transform};
use crate::equalize::{Equalization, UNIT_GAIN};
use crate::job::stage_cache::{transform_from_bits, transform_to_bits};
use crate::tile::Rect;
use crate::timing::{TimingCounter, measurement_scope, record_timing};

/// A stack's alignment: every frame's reference-to-frame [`Transform`] and
/// the crop rect the fuse applied, if any. `transforms` is stored as each
/// entry's `f64::to_bits()` projection, since JSON does not round-trip
/// `f64` bit-for-bit by default.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Registration {
    #[serde(with = "transform_bits")]
    pub transforms: Vec<Transform>,
    pub crop: Option<Rect>,
    /// The brightness gains the fuse applied to each frame, `None` when
    /// equalization was off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equalization: Option<Equalization>,
}

impl Registration {
    /// Every frame's Q16 brightness gain, empty when the fuse applied none.
    pub fn gains(&self) -> &[u32] {
        self.equalization
            .as_ref()
            .map_or(&[], |e| e.gains.as_slice())
    }

    /// Frame `i`'s Q16 brightness gain, unit when the fuse applied none.
    pub fn gain(&self, i: usize) -> u32 {
        self.gains().get(i).copied().unwrap_or(UNIT_GAIN)
    }

    /// Frame `i`'s transform, [`Transform::identity`] past the end.
    pub fn transform(&self, i: usize) -> Transform {
        self.transforms
            .get(i)
            .copied()
            .unwrap_or_else(Transform::identity)
    }

    /// Every frame's transform, index == frame index, padded with
    /// [`Transform::identity`] up to `frame_count`.
    pub fn transforms(&self, frame_count: usize) -> Vec<Transform> {
        (0..frame_count).map(|i| self.transform(i)).collect()
    }

    /// The crop's top-left corner in frame coordinates, `(0, 0)` when there
    /// is no crop.
    pub fn origin(&self) -> (u32, u32) {
        self.crop.map(|r| (r.x, r.y)).unwrap_or((0, 0))
    }

    /// The composite's extent: the crop's size, or `frame` when there is no
    /// crop.
    pub fn extent(&self, frame: (u32, u32)) -> (u32, u32) {
        self.crop.map(|r| (r.width, r.height)).unwrap_or(frame)
    }
}

mod transform_bits {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::{Transform, transform_from_bits, transform_to_bits};

    pub fn serialize<S: Serializer>(transforms: &[Transform], s: S) -> Result<S::Ok, S::Error> {
        let bits: Vec<[[u64; 3]; 3]> = transforms.iter().map(transform_to_bits).collect();
        bits.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Transform>, D::Error> {
        let bits: Vec<[[u64; 3]; 3]> = Vec::deserialize(d)?;
        Ok(bits.into_iter().map(transform_from_bits).collect())
    }
}

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
    /// The largest of the inner reader's own scratch and, per non-identity
    /// transform, a warped read of a rect this size centred in the frame:
    /// the source buffer over the footprint, the warped samples with their
    /// coverage, and the inner scratch at that footprint. Translations and
    /// similarities need the same footprint at every position, and clamping
    /// only shrinks it. The one live coverage mask is the scheduler's.
    fn read_scratch_bytes(&self, width: u32, height: u32) -> u64 {
        let spp = u64::from(self.samples_per_pixel);
        let (width, height) = (width.min(self.frame.0), height.min(self.frame.1));
        let dst = Rect {
            x: (self.frame.0 - width) / 2,
            y: (self.frame.1 - height) / 2,
            width,
            height,
        };
        let warped = u64::from(width) * u64::from(height) * (spp * 2 + 1);
        let mut largest = self.inner.read_scratch_bytes(width, height);
        for transform in &self.transforms {
            if *transform == Transform::identity() {
                continue;
            }
            if let Ok(SourceFootprint::Covers { read, .. }) =
                transform.source_footprint(dst, self.frame)
            {
                let pixels = u64::from(read.width) * u64::from(read.height);
                let inner = self.inner.read_scratch_bytes(read.width, read.height);
                largest = largest.max(pixels * spp * 2 + warped + inner);
            }
        }
        largest
    }

    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        self.read_tile_coverage_into(frame, rect, out).map(drop)
    }

    /// `None` for an identity transform and for a warp that covered every
    /// pixel of `rect`; otherwise the mask [`warp_tile`] produced.
    fn read_tile_coverage_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<Option<Vec<bool>>, AlgorithmError> {
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
            return self.inner.read_tile_into(frame, rect, out).map(|()| None);
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
        Ok(warped.covered.contains(&false).then_some(warped.covered))
    }
}
