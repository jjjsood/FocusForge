// SPDX-License-Identifier: GPL-3.0-or-later

//! The seam `crates/gpu` reaches real pixels through, without `crates/core`
//! taking a `wgpu` dependency. Unconditional -- no feature flag -- because a
//! trait definition costs nothing without an implementor; `crates/gpu` is
//! the only crate that implements [`TileBackend`], behind its own `gpu`
//! feature.
//!
//! Every [`TileBackend`] method takes `&dyn TileReader` plus a rect the
//! *caller* names, never a pre-read [`Tile`] or a `TileCtx`: a
//! `TileConsumer` can hand a backend a
//! [`crate::align::Transform::source_footprint`]-shaped rect instead of the
//! scheduler's destination read rect, and pull its own pixels. `crates/gpu`
//! owns the shape's only implementor.

use rayon::prelude::*;

use crate::algorithm::{AlgorithmError, TileOutput, TileReader};
use crate::align::warp::WarpedTile;
use crate::align::{AlignError, Transform};
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

    /// `true` once this backend's device has been lost. Every later call on
    /// the backend then fails, so a caller that observes `true` must finish
    /// the work on a path of its own instead of retrying. Defaults to
    /// `false`, for a backend with no notion of device loss.
    fn device_lost(&self) -> bool {
        false
    }

    /// The largest side, in pixels, of a square read rect this backend accepts
    /// at `samples_per_pixel`; the tile planner never plans a larger tile.
    /// Defaults to no limit.
    fn max_tile_size(&self, _samples_per_pixel: u32) -> u32 {
        u32::MAX
    }

    /// Why one `width` x `height` tile at `samples_per_pixel`, pyramid depth
    /// `levels` and `transforms` (the shape [`Self::warp_fuse_tile`] takes)
    /// cannot run on this backend's device, as a message naming the host
    /// path the run continues on; `None` when it fits. A caller that gets
    /// `Some` fuses on the host instead of calling this backend. Defaults to
    /// `None`, for a backend with no budget of its own.
    fn tile_budget_shortfall(
        &self,
        _width: u32,
        _height: u32,
        _samples_per_pixel: u32,
        _levels: u32,
        _transforms: &[Transform],
    ) -> Option<String> {
        None
    }

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

    /// Fuses `rect` across `frame_count` frames of `reader` with the
    /// `laplacian` algorithm's pyramid depth `levels`, on the GPU, returning
    /// both the composite tile and the Z-Map naming the winning source frame
    /// per pixel -- `crates/gpu`'s implementation is the WGSL twin of
    /// [`crate::algorithm::StackAlgorithm::fuse`] as implemented by
    /// [`crate::fuse::laplacian::Laplacian`].
    fn fuse_tile(
        &self,
        reader: &dyn TileReader,
        frame_count: usize,
        rect: Rect,
        samples_per_pixel: u32,
        levels: u32,
    ) -> Result<TileOutput, AlgorithmError>;

    /// Warps destination tile `dst` of `frame_index` (a `frame`-sized frame)
    /// through `transform`, on the GPU -- the WGSL twin of
    /// [`crate::align::warp::warp_tile`], reproducing its bicubic
    /// Catmull-Rom kernel, its out-of-footprint rule and its
    /// [`AlignError::SourceTileTooSmall`] precheck exactly.
    ///
    /// The GPU path must return the same `AlignError::SourceTileTooSmall`
    /// variant and payload as the CPU twin, so this returns
    /// `Result<_, AlignError>`, not `AlgorithmError` like
    /// [`Self::self_check`] and [`Self::fuse_tile`]. There is no
    /// `From<AlignError> for AlgorithmError` (only the reverse), so a
    /// widened signature would stringify the variant and lose that guarantee.
    fn warp_tile(
        &self,
        reader: &dyn TileReader,
        frame_index: usize,
        frame: (u32, u32),
        samples_per_pixel: u32,
        dst: Rect,
        transform: &Transform,
    ) -> Result<WarpedTile, AlignError>;

    /// Drives the whole tile through both stages in one call: warps every
    /// frame's contribution to `rect` (an identity entry in `transforms` is
    /// the identical short-circuit `WarpingReader` uses -- "read `rect`
    /// directly", never a dispatch) and fuses the result with the
    /// `laplacian` algorithm's pyramid depth `levels`, keeping every
    /// intermediate resident on the GPU implementation's own device.
    /// `transforms[i]` is frame `i`'s reference-to-frame transform, the same
    /// index-is-frame convention [`crate::align::WarpingReader`]
    /// already carries -- `transforms.len()` is the frame count, so there is
    /// no separate `frame_count` argument.
    ///
    /// Returns `AlgorithmError`, not `AlignError` like [`Self::warp_tile`]:
    /// this value crosses into [`crate::tile::TileConsumer::consume`], which
    /// is `AlgorithmError`-typed, and a warp failure wraps as
    /// `AlgorithmError::Failed { algorithm: "warp", detail }` exactly the way
    /// [`crate::align::WarpingReader`] already wraps it -- so
    /// the two paths' error text is identical.
    fn warp_fuse_tile(
        &self,
        reader: &dyn TileReader,
        frame: (u32, u32),
        transforms: &[Transform],
        rect: Rect,
        samples_per_pixel: u32,
        levels: u32,
    ) -> Result<TileOutput, AlgorithmError>;
}

/// Length of the moment vector an [`EccMomentSession`] returns.
pub const ECC_MOMENTS: usize = 18;

/// One ECC level's prepared reference and moving plane, as a backend sees
/// them when it opens a session.
pub struct EccSessionRequest<'a> {
    /// Unique per prepared template for the life of the process.
    pub template_id: u64,
    /// The template region, in full-frame coordinates.
    pub region: Rect,
    /// Jacobian columns: 2 for translation, 4 for similarity.
    pub columns: usize,
    /// Template values over `region`, row-major.
    pub template: &'a [f32],
    /// `columns` column-centred Jacobian values per `region` pixel, row-major.
    pub jacobian: &'a [f64],
    /// Subtracted from every template and moving value before summing.
    pub shift: f64,
    /// A window of the moving frame, row-major.
    pub moving: &'a [f32],
    pub moving_dims: (u32, u32),
    /// Full-frame coordinates of the window's top-left pixel.
    pub moving_origin: (u32, u32),
}

/// A backend's per-level source of ECC moments.
pub trait EccMomentSession: Send {
    /// Samples the moving plane at `warp(x)` for every `x` in the region
    /// (bicubic Catmull-Rom; a sample whose 4x4 taps leave the window is
    /// invalid) and sums over the valid samples `s`, template values `t` and
    /// Jacobian columns `g_k`, all minus the request's `shift`:
    /// `[n, Σs, Σs², Σt·s, Σg_k·s (4), Σt, Σt², Σg_k·t (4), Σg_k (4)]`.
    /// Entries for columns at and past the request's `columns` are zero. The
    /// template-side moments (indices 8..18) are summed only under partial
    /// overlap and are zero under full overlap, where the solver reads its
    /// own statistics.
    fn moments(&mut self, warp: &Transform) -> Result<[f64; ECC_MOMENTS], AlgorithmError>;
}

/// An optional accelerator for the ECC alignment solver. Object-safe and
/// `Send + Sync` for the same reason as [`TileBackend`].
pub trait AlignBackend: Send + Sync {
    /// Opens a session for one level, or `Ok(None)` to decline it, in which
    /// case the level runs on the CPU. An error fails the whole alignment.
    fn open_ecc(
        &self,
        request: &EccSessionRequest<'_>,
    ) -> Result<Option<Box<dyn EccMomentSession>>, AlgorithmError>;
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
    use crate::algorithm::StackAlgorithm;
    use crate::algorithm::TileCtx;
    use crate::fuse::laplacian::Laplacian;
    use crate::params::{ParamValue, ParamValues};

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

        fn fuse_tile(
            &self,
            reader: &dyn TileReader,
            frame_count: usize,
            rect: Rect,
            samples_per_pixel: u32,
            levels: u32,
        ) -> Result<TileOutput, AlgorithmError> {
            let mut tiles = Vec::with_capacity(frame_count);
            for frame in 0..frame_count {
                let mut samples =
                    vec![0u16; (rect.width * rect.height * samples_per_pixel) as usize];
                reader.read_tile_into(frame, rect, &mut samples)?;
                tiles.push(Tile {
                    rect,
                    samples_per_pixel,
                    samples,
                });
            }
            let ctx = TileCtx::new(rect, &tiles);
            let mut cfg = ParamValues::new();
            cfg.insert("levels".to_string(), ParamValue::Int(levels as i64));
            Laplacian.fuse(&ctx, &cfg)
        }

        fn warp_tile(
            &self,
            reader: &dyn TileReader,
            frame_index: usize,
            frame: (u32, u32),
            samples_per_pixel: u32,
            dst: Rect,
            transform: &Transform,
        ) -> Result<WarpedTile, AlignError> {
            crate::align::warp::warp_tile(
                reader,
                frame_index,
                frame,
                samples_per_pixel,
                dst,
                transform,
            )
        }

        fn warp_fuse_tile(
            &self,
            reader: &dyn TileReader,
            frame: (u32, u32),
            transforms: &[Transform],
            rect: Rect,
            samples_per_pixel: u32,
            levels: u32,
        ) -> Result<TileOutput, AlgorithmError> {
            let mut tiles = Vec::with_capacity(transforms.len());
            for (i, t) in transforms.iter().enumerate() {
                let tile = if *t == Transform::identity() {
                    let mut samples =
                        vec![0u16; (rect.width * rect.height * samples_per_pixel) as usize];
                    reader.read_tile_into(i, rect, &mut samples)?;
                    Tile {
                        rect,
                        samples_per_pixel,
                        samples,
                    }
                } else {
                    crate::align::warp::warp_tile(reader, i, frame, samples_per_pixel, rect, t)
                        .map_err(|source| AlgorithmError::Failed {
                            algorithm: "warp".to_string(),
                            detail: source.to_string(),
                        })?
                        .tile
                };
                tiles.push(tile);
            }
            let ctx = TileCtx::new(rect, &tiles);
            let mut cfg = ParamValues::new();
            cfg.insert("levels".to_string(), ParamValue::Int(levels as i64));
            Laplacian.fuse(&ctx, &cfg)
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

    struct DecliningAlign;

    impl AlignBackend for DecliningAlign {
        fn open_ecc(
            &self,
            _request: &EccSessionRequest<'_>,
        ) -> Result<Option<Box<dyn EccMomentSession>>, AlgorithmError> {
            Ok(None)
        }
    }

    #[test]
    fn align_traits_are_object_safe_send_and_sync() {
        fn assert_send<T: Send + 'static>() {}
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<Box<dyn AlignBackend>>();
        assert_send::<Box<dyn EccMomentSession>>();
        let backend: Box<dyn AlignBackend> = Box::new(DecliningAlign);
        let request = EccSessionRequest {
            template_id: 0,
            region: Rect {
                x: 1,
                y: 1,
                width: 1,
                height: 1,
            },
            columns: 2,
            template: &[],
            jacobian: &[],
            shift: 0.0,
            moving: &[],
            moving_dims: (0, 0),
            moving_origin: (0, 0),
        };
        assert!(backend.open_ecc(&request).unwrap().is_none());
    }

    /// A test-only, in-memory [`TileReader`] over one flat sample buffer per
    /// frame -- just enough to drive [`DummyBackend::fuse_tile`] without a
    /// real fixture.
    struct FlatReader {
        frames: Vec<Vec<u16>>,
    }

    impl TileReader for FlatReader {
        fn read_tile_into(
            &self,
            frame: usize,
            _rect: Rect,
            out: &mut [u16],
        ) -> Result<(), AlgorithmError> {
            out.copy_from_slice(&self.frames[frame]);
            Ok(())
        }
    }

    #[test]
    fn dummy_backend_fuse_tile_runs_laplacian_and_names_a_winner() {
        let (width, height, spp) = (8u32, 8u32, 3u32);
        let rect = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let flat_a = vec![10_000u16; (width * height * spp) as usize];
        let mut checker = vec![0u16; (width * height * spp) as usize];
        for y in 0..height {
            for x in 0..width {
                let v = if (x + y) % 2 == 0 { 40_000 } else { 20_000 };
                for c in 0..spp {
                    checker[((y * width + x) * spp + c) as usize] = v;
                }
            }
        }
        let reader = FlatReader {
            frames: vec![flat_a, checker],
        };

        let backend = DummyBackend;
        let out = backend
            .fuse_tile(&reader, 2, rect, spp, 1)
            .expect("fuse_tile must succeed");

        assert_eq!(out.tile.rect, rect);
        assert_eq!(out.zmap.len(), (width * height) as usize);
        assert!(
            out.zmap.iter().all(|&index| index == 1),
            "the only frame with detail must win every pixel"
        );
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
