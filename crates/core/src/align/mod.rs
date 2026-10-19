// SPDX-License-Identifier: GPL-3.0-or-later

//! Frame-to-frame geometric transforms and the tiled warp that applies them.
//!
//! `transform` holds [`Transform`], its algebra and the source-footprint
//! query; [`warp`] holds the interpolation kernel and the tiled resampler
//! built on top of it; `error` holds this module's single error type.
//! `warp` only ever applies a transform; `ecc` is what solves for one, on a
//! single pyramid level. [`crate::align::PyramidalEcc`] drives that solver
//! coarse-to-fine over a pyramid to register whole frames, and
//! [`align_stack`] is the free-function driver that runs it across a whole
//! stack under a reference policy. `crop` computes the auto-crop rect from
//! a finished [`StackAlignment`]: pure geometry over the transforms, no
//! pixel ever read.

mod crop;
mod driver;
mod ecc;
mod ecc_algorithm;
mod error;
mod feature_seed;
mod pyramid;
mod transform;
pub mod warp;
mod warp_reader;

pub use crop::{auto_crop, crop_rect};
pub use driver::{
    AlignDriverOptions, AlignRun, DEFAULT_ALIGN_MAX_BYTES_IN_FLIGHT, FailurePolicy, FrameAlignment,
    ReferenceFrame, ReferencePolicy, StackAlignment, align_stack, align_stack_with,
    bytes_per_registration, bytes_shared_reference,
};
pub use ecc::{
    CpuMomentSession, DivergenceReason, EccIteration, EccMode, EccParams, EccResult, EccStatus,
    solve_level, solve_level_traced,
};
pub use ecc_algorithm::{EccLevelRecord, PyramidalEcc};
pub use error::AlignError;
pub use feature_seed::{
    FeatureSeedError, Features, SeedFit, detector_threshold, fit_between, seed_between,
};
pub use pyramid::{AlignPyramid, LumaAssembler, max_levels};
pub use transform::{KERNEL_RADIUS, SourceFootprint, Transform};
pub use warp_reader::{Registration, WarpingReader};
