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

pub mod consumer;
mod error;
pub mod laplacian;
pub mod metrics;
pub mod pyramid;

pub use consumer::{FusionConsumer, FusionGeometry};
pub use error::PyramidError;
pub use pyramid::{
    MEASURED_MARGIN, Plane, Pyramid, build, build_from_plane, collapse, collapse_into, max_levels,
};
