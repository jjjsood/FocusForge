// SPDX-License-Identifier: GPL-3.0-or-later

//! Pyramid math errors, and the helpers that wrap errors as an algorithm's own
//! failure. The pyramid errors carry no path fields. `AlgorithmError` is
//! `#[non_exhaustive]` with a `Failed { algorithm, detail }` shape, so a
//! caller wraps a `PyramidError` into that without a breaking change.

use crate::algorithm::AlgorithmError;

/// Everything that can go wrong building a [`super::pyramid::Plane`] or a
/// [`super::pyramid::Pyramid`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PyramidError {
    /// A [`super::pyramid::Plane`] was requested over a zero-area extent.
    #[error("extent {width}x{height} is empty")]
    EmptyExtent { width: u32, height: u32 },

    /// A [`super::pyramid::Plane`] was requested with
    /// `samples_per_pixel == 0`. Refused, never defaulted to 1.
    #[error("samples_per_pixel must be non-zero")]
    SamplesPerPixelZero,

    /// [`super::pyramid::Plane::from_u16`]'s input slice, or
    /// [`super::pyramid::collapse_into`]'s output slice, is not exactly
    /// `width * height * samples_per_pixel` long. Never clamped -- the same
    /// exact-length contract `crate::tile::Tile::crop_into` enforces.
    #[error("buffer size mismatch: expected {expected} u16 samples, got {got}")]
    BufferSize { expected: usize, got: usize },

    /// `levels` exceeds [`super::pyramid::max_levels`] for the given
    /// `width x height`. Deliberately **not** the same type as
    /// `crate::tile::TileError::LevelsTooDeep`.
    #[error("levels {requested} exceeds the maximum of {max} for a {width}x{height} buffer")]
    LevelsTooDeep {
        requested: u32,
        max: u32,
        width: u32,
        height: u32,
    },
}

/// Wraps any displayable `source` as `algorithm`'s own
/// [`AlgorithmError::Failed`]. `PyramidError` is not one of
/// [`AlgorithmError`]'s `#[from]` variants by design: a bare conversion would
/// drop the algorithm id from the message.
pub(crate) fn algorithm_error<E: std::fmt::Display>(
    algorithm: &str,
) -> impl Fn(E) -> AlgorithmError {
    move |source| AlgorithmError::Failed {
        algorithm: algorithm.to_string(),
        detail: source.to_string(),
    }
}

/// The error for a resolved-values map that lacks `key`.
pub(crate) fn missing_key(algorithm: &str, key: &str) -> AlgorithmError {
    AlgorithmError::Failed {
        algorithm: algorithm.to_string(),
        detail: format!(
            "resolved values are missing `{key}`; call crate::params::resolve before fuse"
        ),
    }
}
