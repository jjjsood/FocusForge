// SPDX-License-Identifier: GPL-3.0-or-later

//! Error type for `crate::align`.
//!
//! Modelled on `crate::tile::error` and `crate::fuse::error`: every variant
//! names its offending value. `#[non_exhaustive]` so new variants can be
//! added without a breaking change.

use thiserror::Error;

use crate::algorithm::AlgorithmError;
use crate::tile::Rect;

/// Everything that can go wrong building, inverting or applying a
/// [`super::Transform`], or running [`super::warp`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AlignError {
    /// [`super::Transform::inverse`] was called on a matrix whose
    /// determinant is (numerically) zero.
    #[error("transform is not invertible: determinant {det} is numerically zero")]
    NonInvertible { det: f64 },

    /// A destination rect's corners do not all lie in the positive-`w`
    /// half-space, so its source footprint has no finite bounding box.
    #[error("destination rect {rect:?} has a corner beyond the horizon (w <= 0)")]
    HorizonCrossesRect { rect: Rect },

    /// The pure warp was handed a source [`crate::tile::Tile`] that does not
    /// contain the taps a covered destination pixel needs. A caller bug,
    /// never clamped -- the same discipline as
    /// [`crate::tile::TileError::CropOutOfBounds`].
    #[error(
        "source tile does not contain the taps a covered pixel needs: need {needed:?}, have {have:?}"
    )]
    SourceTileTooSmall { needed: Rect, have: Rect },

    /// An `_into` call's output buffer is the wrong length for the
    /// requested rect.
    #[error("buffer size mismatch: expected {expected} u16 samples, got {got}")]
    BufferSize { expected: usize, got: usize },

    /// A [`crate::algorithm::TileReader`] read failed.
    #[error("reading frame {frame} tile {rect:?}: {source}")]
    Read {
        frame: usize,
        rect: Rect,
        // Boxed: `AlgorithmError` is larger on Windows, where an `OsString` is 32 bytes.
        #[source]
        source: Box<AlgorithmError>,
    },

    /// A [`super::pyramid::LumaAssembler`] tile push whose rect is not
    /// fully inside the assembler's declared frame extent. Never clamped:
    /// the caller's tiling is wrong, not this assembler's problem to paper
    /// over.
    #[error("tile {tile:?} is not inside the frame extent {frame:?}")]
    TileOutsideFrame { tile: Rect, frame: (u32, u32) },

    /// [`super::ecc::solve_level`]'s template and moving planes disagree on
    /// `samples_per_pixel` -- both must be exactly `1`, a single-channel
    /// luminance plane. They may still differ in width/height: a coarser
    /// pyramid level is exactly that.
    #[error(
        "template and moving planes must both be single-channel: template spp={template_spp}, moving spp={moving_spp}"
    )]
    PlaneMismatch { template_spp: u32, moving_spp: u32 },

    /// [`super::ecc::solve_level`]'s `region` is not fully inside the
    /// template plane's gradient-valid interior (`1..width-1`,
    /// `1..height-1`) -- the central-difference gradient has no value on
    /// the outer ring.
    #[error("region {region:?} is not inside the template's valid interior {plane:?}")]
    RegionOutOfBounds { region: Rect, plane: (u32, u32) },

    /// [`super::ecc::solve_level`]'s `region` has zero area.
    #[error("region {region:?} has zero area")]
    RegionTooSmall { region: Rect },

    /// [`super::ecc::EccParams`] contains a non-finite or out-of-range
    /// field: `max_iterations == 0`, a non-positive or non-finite
    /// `epsilon_px`, or a `min_valid_fraction` outside `(0.0, 1.0]`.
    #[error("invalid EccParams: {detail}")]
    InvalidParams { detail: String },

    /// The auto-crop footprint mapping was handed a [`super::Transform`]
    /// whose bottom row is not `[0, 0, w]` -- a genuinely projective matrix,
    /// which can send a rectangle's corner to infinity and has no bounded
    /// footprint for the inscribed-rectangle search to run over.
    #[error(
        "transform's bottom row {bottom_row:?} is not affine (perspective row must be [0, 0, w])"
    )]
    NotAffine { bottom_row: [f64; 3] },

    /// `crates/gpu`'s device backend failed underneath a warp call --
    /// `impl From<focusforge_gpu::device::GpuError> for AlignError`
    /// (`crates/gpu/src/device.rs`) is the only place this variant is
    /// built, mirroring `AlgorithmError::Failed { algorithm: "gpu", .. }`
    /// one crate over.
    #[error("gpu backend: {detail}")]
    Backend { detail: String },
}
