// SPDX-License-Identifier: GPL-3.0-or-later

//! The seam `crates/gpu` reaches real pixels through, without `crates/core`
//! taking a `wgpu` dependency. Unconditional -- no feature flag -- because a
//! trait definition costs nothing without an implementor; `crates/gpu` is
//! the only crate that implements [`TileBackend`], behind its own `gpu`
//! feature.
//!
//! Every [`TileBackend`] method takes `&dyn TileReader` plus a rect the
//! *caller* names, never a pre-read [`Tile`] or a `TileCtx`: a later
//! `TileConsumer` can hand a backend a
//! [`crate::align::Transform::source_footprint`]-shaped rect instead of the
//! scheduler's destination read rect, and pull its own pixels. `crates/gpu`
//! owns the shape's only implementor.

use rayon::prelude::*;

use crate::algorithm::{AlgorithmError, TileReader};
use crate::tile::{Rect, Tile};

/// The adapter facts a [`TileBackend`] implementation was built on: plain
/// `String`s, so no `wgpu` type crosses into `crates/core`. `crates/gpu`
/// fills these from `wgpu::AdapterInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendInfo {
    pub name: String,
    pub backend: String,
    pub driver: String,
    pub driver_info: String,
}

impl std::fmt::Display for BackendInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (backend={}, driver={}, driver_info={})",
            self.name, self.backend, self.driver, self.driver_info
        )
    }
}

/// One tile-shaped GPU operation. `Send + Sync` and object-safe for the same
/// reason every other seam in this crate is
/// ([`crate::algorithm::TileReader`], [`crate::tile::TileConsumer`],
/// [`crate::tile::FramePreprocess`]): every one of those is used as `&dyn`.
pub trait TileBackend: Send + Sync {
    /// The adapter this backend runs on, for logging and diagnosability.
    fn info(&self) -> BackendInfo;

    /// Read `rect` of `frame` through `reader`, apply a trivial per-sample
    /// gain on the GPU, and return the result. Exists only to exercise the
    /// upload/dispatch/readback path -- this trait's other methods,
    /// `fuse_tile` / `warp_tile`, extend the same shape.
    fn self_check(
        &self,
        reader: &dyn TileReader,
        frame: usize,
        rect: Rect,
        samples_per_pixel: u32,
        gain: f32,
    ) -> Result<Tile, AlgorithmError>;
}

/// The rayon CPU twin of the self-check kernel every [`TileBackend`]
/// implementation's `self_check` runs on the GPU: every compute kernel has
/// a CPU twin.
///
/// Per sample: `floor(clamp(f32::from(s) * gain, 0.0, 65535.0) + 0.5)`.
/// Deliberately `floor(x + 0.5)`, not `f32::round`: Rust's `round` is
/// ties-away-from-zero and WGSL's `round` is ties-to-even, so the two
/// disagree by 1 LSB on every exact tie. `floor(x + 0.5)` is the same
/// expression in both languages, which is what makes this harness kernel's
/// expected GPU/CPU delta exactly 0 rather than merely within tolerance.
pub fn self_check_cpu(tile: &Tile, gain: f32) -> Tile {
    let samples: Vec<u16> = tile
        .samples
        .par_iter()
        .map(|&s| {
            let scaled = (f32::from(s) * gain).clamp(0.0, 65535.0);
            (scaled + 0.5).floor() as u16
        })
        .collect();
    Tile {
        rect: tile.rect,
        samples_per_pixel: tile.samples_per_pixel,
        samples,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyBackend;

    impl TileBackend for DummyBackend {
        fn info(&self) -> BackendInfo {
            BackendInfo {
                name: "dummy".into(),
                backend: "dummy".into(),
                driver: "dummy".into(),
                driver_info: "dummy".into(),
            }
        }

        fn self_check(
            &self,
            reader: &dyn TileReader,
            frame: usize,
            rect: Rect,
            samples_per_pixel: u32,
            gain: f32,
        ) -> Result<Tile, AlgorithmError> {
            let mut samples = vec![0u16; (rect.width * rect.height * samples_per_pixel) as usize];
            reader.read_tile_into(frame, rect, &mut samples)?;
            let tile = Tile {
                rect,
                samples_per_pixel,
                samples,
            };
            Ok(self_check_cpu(&tile, gain))
        }
    }

    #[test]
    fn trait_is_object_safe() {
        let backends: Vec<Box<dyn TileBackend>> = vec![Box::new(DummyBackend)];
        for backend in &backends {
            let info = backend.info();
            assert_eq!(info.name, "dummy");
        }
    }

    #[test]
    fn tile_backend_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<Box<dyn TileBackend>>();
    }

    #[test]
    fn self_check_cpu_applies_gain_and_clamps() {
        let tile = Tile {
            rect: Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
            samples_per_pixel: 1,
            samples: vec![100, 65535],
        };

        let out = self_check_cpu(&tile, 2.0);

        assert_eq!(out.samples, vec![200, 65535]);
        assert_eq!(out.rect, tile.rect);
        assert_eq!(out.samples_per_pixel, tile.samples_per_pixel);
    }

    #[test]
    fn self_check_cpu_uses_floor_of_x_plus_half_not_round() {
        // 1 * 0.5 = 0.5 -- Rust's ties-away-from-zero `round` and WGSL's
        // ties-to-even `round` would disagree on plenty of other inputs, but
        // `floor(x + 0.5)` is unambiguous: floor(0.5 + 0.5) = floor(1.0) = 1.
        let tile = Tile {
            rect: Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            samples_per_pixel: 1,
            samples: vec![1],
        };

        let out = self_check_cpu(&tile, 0.5);

        assert_eq!(out.samples, vec![1]);
    }
}
