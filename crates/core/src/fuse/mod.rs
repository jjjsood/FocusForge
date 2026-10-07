// SPDX-License-Identifier: GPL-3.0-or-later

//! Pure pyramid math over a single buffer: Gaussian reduce, Laplacian
//! expand, build to `N` levels, collapse back to one buffer. No file I/O,
//! no [`crate::io::TiffStack`], no [`crate::algorithm`] trait -- this
//! module operates on plain `&[u16]` buffers and `Plane`s so a synthetic
//! test buffer and a real frame tile are both callable with no conversion,
//! and so a fusion algorithm can call this math without going through the
//! `StackAlgorithm` trait.
//!
//! See [`pyramid`] for the full spec -- kernel coefficients, border rule,
//! rounding expression and the measured tile-overlap margin -- written so
//! a GPU twin can be authored from it alone.
//!
//! # Data contract
//!
//! Every fusion method works on **encoded code values as delivered** by the
//! reader: RAW frames arrive sRGB-encoded, TIFF frames as 16-bit unsigned
//! samples with an unknown transfer function. Nothing is linearised or
//! rescaled; the run reports which case applied as `working_space`
//! (`encoded-srgb` or `encoded-unknown-transfer`).
//!
//! - **One weight per pixel.** A sharpness score sums over every channel, so
//!   a pixel's channels are selected or blended with one shared weight; a
//!   chroma-only edge picks one frame for all channels.
//! - **Saturated samples are data.** A clipped plateau scores as flat and
//!   resolves like any flat region.
//! - **Ties.** Comparisons are strictly greater, so the lowest eligible
//!   frame index wins an exact tie or a flat region.
//! - **Eligibility.** A frame whose warp leaves part of a tile without a
//!   source supplies a coverage mask ([`crate::algorithm::TileCtx::for_each_frame`]).
//!   A frame takes part at a pixel only if its mask, eroded by the reach of
//!   the method's score there, is true: uncovered samples never score,
//!   weigh or get copied. Unaligned frames and frames whose warp covers the
//!   whole read rect carry no mask and are eligible everywhere. A pixel with
//!   no eligible frame is [`crate::zmap::NO_SOURCE`] with zero samples.
//! - **What the Z-Map means.** Laplacian: the level-0 winner (coarser levels
//!   may select other frames). Weighted Average: the dominant contributor,
//!   not the provenance of a blend. Depth Map: the source of the copied
//!   pixel, so each composite pixel is the indexed frame's own pixel. A
//!   source index is an index into the stack, never a metric depth.
//! - **Known limit.** Depth Map copies a whole source pixel, so under
//!   exposure or tint drift between frames its colour is the one a single
//!   frame had; it does not equalise photometry.

pub mod backend_consumer;
pub mod consumer;
pub mod depth_map;
pub mod description;
mod error;
pub mod laplacian;
pub mod metrics;
pub mod pyramid;
mod sharpness;
pub mod weighted_average;

pub use backend_consumer::BackendFusionConsumer;
pub use consumer::{FusionConsumer, FusionGeometry};
pub use error::PyramidError;
pub use pyramid::{
    MEASURED_MARGIN, Plane, Pyramid, build, build_from_plane, collapse, collapse_into,
    gaussian_levels, max_levels,
};
