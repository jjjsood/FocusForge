// SPDX-License-Identifier: GPL-3.0-or-later

//! Frame-to-frame geometric transforms and the tiled warp that applies them.
//!
//! `transform` holds [`Transform`], its algebra and the source-footprint
//! query; [`warp`] holds the interpolation kernel and the tiled resampler
//! built on top of it; `error` holds this module's single error type.
//! `warp` only ever applies a transform; `ecc` is what solves for one, on a
//! single pyramid level. Driving `ecc` across a pyramid to register whole
//! frames is still to come.

mod error;
mod transform;
pub mod warp;

pub use error::AlignError;
pub use transform::{KERNEL_RADIUS, SourceFootprint, Transform};
