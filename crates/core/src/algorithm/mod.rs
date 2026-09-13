// SPDX-License-Identifier: GPL-3.0-or-later

//! The two algorithm families: a stacking (fusion) trait and an alignment
//! trait, sharing one shape -- `id()`, `display_name()`, `params()` and one
//! work method -- so a rayon tile scheduler can hold either behind a shared
//! reference and a registry can hold a `&'static dyn` of either.
//!
//! An implementor of either trait may not take a whole frame (the context is
//! the only pixel source, and it is tile-shaped, never a whole-frame
//! buffer), may not require a GPU device (`crates/core` links neither `wgpu`
//! nor a device crate; the GPU twin reaches `core` through a trait `core`
//! owns), and may not read a setting that is not declared in
//! its own `params()` (`ParamDesc` is the single source of truth for every
//! setting -- a struct field that is not a descriptor is invisible to both
//! the CLI and the GUI).

mod error;

pub use crate::params::StackContext;
use crate::params::{ParamDesc, ParamValue, ParamValues};
use crate::tile::{Rect, Tile};

pub use error::AlgorithmError;

/// The output of one [`StackAlgorithm::fuse`] call: the fused pixels for one
/// tile, plus a Z-Map naming which source frame each output pixel came from.
///
/// Invariant: `zmap.len() == tile.rect.width as usize * tile.rect.height as
/// usize` -- one index per **pixel**, never per sample. `Tile::samples` is
/// `width * height * samples_per_pixel` long, but the Z-Map names a source
/// *frame*, which is a per-pixel fact, not a per-sample one. Fields are
/// public and there is no validating constructor here: the Z-Map as a
/// document with its own storage and invariants lives in [`crate::zmap`]
/// (`ZMapInfo`, `crate::zmap::validate_tile`, `crate::zmap::ZMapWriter`) --
/// a checked constructor on `TileOutput` itself still waits for its first
/// real caller.
#[derive(Debug, Clone, PartialEq)]
pub struct TileOutput {
    pub tile: Tile,
    pub zmap: Vec<u16>,
}

/// The per-tile view across every frame of a stack that
/// [`StackAlgorithm::fuse`] reads from.
///
/// Opaque on purpose: today's backing is a materialized slice of
/// already-read [`Tile`]s, and a future scheduler may swap it for memmap'd
/// scratch or a streaming source without touching a single algorithm body,
/// because every call site below is identical either way. The field is
/// named `tiles`, not `frames` -- these are frame-*tiles*, and [`Tile`]
/// itself carries no frame index, so this context is what supplies one by
/// position.
pub struct TileCtx<'a> {
    rect: Rect,
    tiles: &'a [Tile],
}

impl<'a> TileCtx<'a> {
    /// Builds a context over `tiles`, one per frame, all covering `rect`.
    ///
    /// Debug-only: every tile's own `rect` must equal `rect` exactly --
    /// enforced with `debug_assert!` rather than a `Result`, since a
    /// mismatch here is a caller bug in code this crate itself controls, not
    /// a runtime condition an algorithm needs to recover from.
    pub fn new(rect: Rect, tiles: &'a [Tile]) -> Self {
        debug_assert!(
            tiles.iter().all(|tile| tile.rect == rect),
            "every frame-tile handed to TileCtx::new must cover the same rect"
        );
        TileCtx { rect, tiles }
    }

    pub fn rect(&self) -> Rect {
        self.rect
    }

    pub fn frame_count(&self) -> usize {
        self.tiles.len()
    }

    /// The tile for frame `i`. A `Result`, not an `Option`, so an
    /// algorithm's `?` carries the failure straight out unchanged.
    pub fn frame(&self, i: usize) -> Result<&Tile, AlgorithmError> {
        self.tiles.get(i).ok_or(AlgorithmError::FrameOutOfRange {
            frame: i,
            frame_count: self.tiles.len(),
        })
    }
}

/// One tile-shaped pixel source. Mirrors
/// [`crate::io::TiffStack::read_tile_into`] exactly, so an
/// [`AlignAlgorithm`] reaches real pixels with no frame buffer and no
/// device.
///
/// `out.len()` must equal exactly `rect.width * rect.height *
/// samples_per_pixel`; a rect that does not fit fully inside the frame is an
/// error, **never clamped** -- a caller that wants a clamped rect clamps it
/// at its own call site.
pub trait TileReader: Send + Sync {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError>;
}

/// What an [`AlignAlgorithm::align_frame`] call sees: the facts about the
/// stack, which frame is the alignment reference, and a handle onto real
/// pixels through a [`TileReader`].
///
/// `Send + Sync` on [`TileReader`] is what keeps `AlignContext<'_>` `Sync`,
/// which is what lets `align_frame` run across frames under rayon.
/// The reader field stays private -- no accessor -- so exposing one later is
/// an additive change rather than a removal.
pub struct AlignContext<'a> {
    stack: StackContext,
    reference: usize,
    reader: &'a dyn TileReader,
}

impl<'a> AlignContext<'a> {
    pub fn new(stack: StackContext, reference: usize, reader: &'a dyn TileReader) -> Self {
        AlignContext {
            stack,
            reference,
            reader,
        }
    }

    pub fn stack(&self) -> &StackContext {
        &self.stack
    }

    pub fn reference(&self) -> usize {
        self.reference
    }

    /// Reads `rect` from `frame` into `out`, through the reader this
    /// context was built with. Same contract as [`TileReader::read_tile_into`]:
    /// `out.len()` must equal `rect.width * rect.height * samples_per_pixel`
    /// exactly, and an out-of-bounds rect is an error, never clamped.
    pub fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        self.reader.read_tile_into(frame, rect, out)
    }
}

/// A stacking (fusion) algorithm: fuses one tile across every frame of a
/// stack into a composite tile plus a Z-Map naming the contributing source
/// frame per pixel.
///
/// The Z-Map indices `fuse` returns in its [`TileOutput`] are frame indices
/// into the [`TileCtx`] the call was given, `0..ctx.frame_count()` -- which
/// is what makes the `u16` meaningful to anything downstream that reads it.
///
/// `fuse` receives a fully resolved [`ParamValues`] -- the caller runs
/// [`crate::params::resolve`] plus [`crate::params::resolved_values`] first --
/// and a key its own schema declares but the map omits is a named
/// [`AlgorithmError`], never an index panic.
pub trait StackAlgorithm: Send + Sync + 'static {
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;
    fn params(&self) -> &[ParamDesc];
    fn fuse(&self, ctx: &TileCtx<'_>, cfg: &ParamValues) -> Result<TileOutput, AlgorithmError>;

    /// How many pyramid levels the tile scheduler must size its read-rect
    /// margin for, given this algorithm's fully resolved `cfg`
    /// (`crate::params::resolve` plus `crate::params::resolved_values`,
    /// same as `fuse` receives). The CLI asks every algorithm this question
    /// and feeds the answer straight into
    /// `crate::tile::TileSchedulerOptions::levels`.
    ///
    /// The default reads the well-known key `"levels"` out of `cfg` and
    /// returns it verbatim, or `0` if `cfg` has no such key. This is exactly
    /// right for the two cases that matter:
    ///
    /// 1. **An algorithm with no pyramid at all** never declares a `"levels"`
    ///    parameter, so `cfg` (which `crate::params::resolve` fills with
    ///    exactly one entry per *declared* parameter, nothing else) never
    ///    carries that key -- the default answers `0`, i.e. no margin, with
    ///    no work from the algorithm's author. Weighted Average and the
    ///    Depth Map inherit this without ever thinking about margins.
    /// 2. **An algorithm that does build a pyramid and spells its depth
    ///    parameter `"levels"`** (`laplacian` does) gets the right answer
    ///    from this same default, reading the identical resolved map `fuse`
    ///    itself reads its own `"levels"` entry from -- the same number from
    ///    the same place, which is the whole point: `fuse`'s pyramid depth
    ///    and the scheduler's margin can no longer silently disagree.
    ///
    /// An algorithm whose depth parameter is *not* called `"levels"`, or
    /// whose margin requirement is not a simple pyramid depth, **overrides**
    /// this method -- a visible, greppable claim about its own margin
    /// requirement, in its own file, rather than an unnamed convention that
    /// fails quietly the way the pre-this-decision code did (`laplacian`'s
    /// pyramid depth and the scheduler's margin were computed from two
    /// separately supplied numbers that nothing kept in agreement).
    fn tile_levels(&self, cfg: &ParamValues) -> u32 {
        cfg.get("levels")
            .and_then(ParamValue::as_i64)
            .map(|levels| levels.max(0) as u32)
            .unwrap_or(0)
    }
}

/// An alignment algorithm: solves one frame's geometric transform against
/// the stack's reference frame, one call per frame.
pub trait AlignAlgorithm: Send + Sync + 'static {
    fn id(&self) -> &str;
    fn display_name(&self) -> &str;
    fn params(&self) -> &[ParamDesc];
    fn align_frame(
        &self,
        ctx: &AlignContext<'_>,
        frame: usize,
        cfg: &ParamValues,
    ) -> Result<crate::align::Transform, AlgorithmError>;
}

impl TileReader for crate::io::TiffStack {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        // Fully-qualified: the inherent `TiffStack::read_tile_into` and this
        // trait method share a name, and the inherent one wins at every
        // direct call site -- a plain `self.read_tile_into(...)` here would
        // recurse into this very impl instead of reaching the inherent
        // method.
        Ok(crate::io::TiffStack::read_tile_into(
            self, frame, rect, out,
        )?)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, LazyLock};

    use super::*;
    use crate::align::Transform;
    use crate::io::TiffError;
    use crate::params::{EnumVariant, ParamKind, ParamValue, Provenance};

    fn sample_tiles(frame_count: usize, width: u32, height: u32) -> (Rect, Vec<Tile>) {
        let rect = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let spp = 3;
        let tiles = (0..frame_count)
            .map(|f| Tile {
                rect,
                samples_per_pixel: spp,
                samples: vec![f as u16; (width * height * spp) as usize],
            })
            .collect();
        (rect, tiles)
    }

    static DUMMY_PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
        vec![
            ParamDesc {
                id: "count-based-threshold".to_string(),
                label: "Count-based threshold".to_string(),
                help: "derived from the stack's frame count".to_string(),
                kind: ParamKind::Float {
                    min: 0.0,
                    max: 1000.0,
                    step: 1.0,
                    logarithmic: false,
                },
                default: ParamValue::Float(0.0),
                auto: Some(Arc::new(|ctx: &StackContext| {
                    ParamValue::Float(ctx.frame_count as f64)
                })),
            },
            ParamDesc {
                id: "mode".to_string(),
                label: "Mode".to_string(),
                help: "dummy mode selector".to_string(),
                kind: ParamKind::Enum {
                    variants: vec![
                        EnumVariant {
                            id: "fast".to_string(),
                            label: "Fast".to_string(),
                        },
                        EnumVariant {
                            id: "slow".to_string(),
                            label: "Slow".to_string(),
                        },
                    ],
                },
                default: ParamValue::Enum("fast".to_string()),
                auto: None,
            },
        ]
    });

    /// A test-only fusion algorithm. `id` and `display_name` are owned
    /// `String` fields, not literals, so the trait's accessors genuinely
    /// borrow from `&self` rather than returning `&'static str` in
    /// disguise.
    struct DummyStack {
        id: String,
        display_name: String,
    }

    impl DummyStack {
        fn new() -> Self {
            DummyStack {
                id: "dummy-stack".to_string(),
                display_name: "Dummy Stack".to_string(),
            }
        }
    }

    impl StackAlgorithm for DummyStack {
        fn id(&self) -> &str {
            &self.id
        }

        fn display_name(&self) -> &str {
            &self.display_name
        }

        fn params(&self) -> &[ParamDesc] {
            &DUMMY_PARAMS
        }

        fn fuse(&self, ctx: &TileCtx<'_>, cfg: &ParamValues) -> Result<TileOutput, AlgorithmError> {
            crate::params::validate(self.params(), cfg)?;
            let tile = ctx.frame(0)?.clone();
            let zmap = vec![0u16; (tile.rect.width * tile.rect.height) as usize];
            Ok(TileOutput { tile, zmap })
        }
    }

    /// A test-only alignment algorithm: identity for the reference frame, a
    /// translation for every other frame.
    struct DummyAlign;

    impl AlignAlgorithm for DummyAlign {
        fn id(&self) -> &str {
            "dummy-align"
        }

        fn display_name(&self) -> &str {
            "Dummy Align"
        }

        fn params(&self) -> &[ParamDesc] {
            &[]
        }

        fn align_frame(
            &self,
            ctx: &AlignContext<'_>,
            frame: usize,
            _cfg: &ParamValues,
        ) -> Result<Transform, AlgorithmError> {
            if frame == ctx.reference() {
                Ok(Transform::identity())
            } else {
                let mut m = Transform::identity().m;
                m[0][2] = frame as f64 - ctx.reference() as f64;
                Ok(Transform { m })
            }
        }
    }

    /// A test-only alignment algorithm that always fails, proving a work
    /// method reports why rather than returning a poisoned output.
    struct FailingAlign;

    impl AlignAlgorithm for FailingAlign {
        fn id(&self) -> &str {
            "failing-align"
        }

        fn display_name(&self) -> &str {
            "Failing Align"
        }

        fn params(&self) -> &[ParamDesc] {
            &[]
        }

        fn align_frame(
            &self,
            _ctx: &AlignContext<'_>,
            _frame: usize,
            _cfg: &ParamValues,
        ) -> Result<Transform, AlgorithmError> {
            Err(AlgorithmError::Failed {
                algorithm: self.id().to_string(),
                detail: "intentional test failure".to_string(),
            })
        }
    }

    /// A test-only, in-memory [`TileReader`]: one flat sample buffer per
    /// frame, chunky row-major, honouring the same exact-length and
    /// in-bounds contracts as [`crate::io::TiffStack::read_tile_into`].
    struct VecReader {
        frames: Vec<Vec<u16>>,
        width: u32,
        height: u32,
        spp: u32,
    }

    impl TileReader for VecReader {
        fn read_tile_into(
            &self,
            frame: usize,
            rect: Rect,
            out: &mut [u16],
        ) -> Result<(), AlgorithmError> {
            let in_bounds = rect.width > 0
                && rect.height > 0
                && rect
                    .x
                    .checked_add(rect.width)
                    .is_some_and(|xw| xw <= self.width)
                && rect
                    .y
                    .checked_add(rect.height)
                    .is_some_and(|yh| yh <= self.height);
            if !in_bounds {
                return Err(AlgorithmError::Tiff(TiffError::OutOfBounds {
                    path: PathBuf::from("<memory>"),
                    requested: rect,
                    frame: (self.width, self.height),
                }));
            }

            let expected_len = rect.width as usize * rect.height as usize * self.spp as usize;
            if out.len() != expected_len {
                return Err(AlgorithmError::Tiff(TiffError::BufferSize {
                    expected: expected_len,
                    got: out.len(),
                }));
            }

            let frame_samples = &self.frames[frame];
            let stride = self.width as usize * self.spp as usize;
            let row_len = rect.width as usize * self.spp as usize;
            let mut out_off = 0;
            for r in rect.y..(rect.y + rect.height) {
                let row_start = r as usize * stride + rect.x as usize * self.spp as usize;
                out[out_off..out_off + row_len]
                    .copy_from_slice(&frame_samples[row_start..row_start + row_len]);
                out_off += row_len;
            }
            Ok(())
        }
    }

    #[test]
    fn dummy_stack_algorithm_implements_the_trait() {
        let algo = DummyStack::new();
        assert_eq!(algo.id(), "dummy-stack");
        assert_eq!(algo.display_name(), "Dummy Stack");
        assert_eq!(algo.params().len(), 2);

        let (rect, tiles) = sample_tiles(2, 4, 4);
        let ctx = TileCtx::new(rect, &tiles);
        let out = algo.fuse(&ctx, &ParamValues::new()).unwrap();
        assert_eq!(out.tile.rect, rect);
        assert_eq!(out.zmap.len(), (rect.width * rect.height) as usize);
    }

    #[test]
    fn dummy_align_algorithm_implements_the_trait() {
        let reader = VecReader {
            frames: vec![vec![0u16; 4 * 4 * 3]; 3],
            width: 4,
            height: 4,
            spp: 3,
        };
        let stack_ctx = StackContext::new(3, 4, 4);
        let ctx = AlignContext::new(stack_ctx, 1, &reader);
        let algo = DummyAlign;

        for frame in 0..stack_ctx.frame_count {
            let t = algo.align_frame(&ctx, frame, &ParamValues::new()).unwrap();
            if frame == ctx.reference() {
                assert_eq!(t, Transform::identity());
            } else {
                assert_ne!(t, Transform::identity());
            }
        }
    }

    #[test]
    fn transform_is_one_representation() {
        let translation = Transform {
            m: [[1.0, 0.0, 5.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        };
        // A real rotation, not a pure scale: the criterion is that one representation
        // covers both restricted modes, and a zero-angle rotation would not show that.
        let (s, c) = (0.3_f64.sin(), 0.3_f64.cos());
        let rotate_scale = Transform {
            m: [
                [2.0 * c, -2.0 * s, 0.0],
                [2.0 * s, 2.0 * c, 0.0],
                [0.0, 0.0, 1.0],
            ],
        };

        let a = translation.compose(&rotate_scale);
        let b = rotate_scale.compose(&translation);
        assert_ne!(a, b, "composition must not be commutative");

        assert_eq!(translation.compose(&Transform::identity()), translation);
    }

    #[test]
    fn traits_are_object_safe() {
        let stacks: Vec<Box<dyn StackAlgorithm>> = vec![Box::new(DummyStack::new())];
        for algo in &stacks {
            let _ = algo.id();
            let _ = algo.display_name();
            let _ = algo.params();
        }

        let aligns: Vec<Box<dyn AlignAlgorithm>> =
            vec![Box::new(DummyAlign), Box::new(FailingAlign)];
        for algo in &aligns {
            let _ = algo.id();
            let _ = algo.display_name();
            let _ = algo.params();
        }
    }

    #[test]
    fn algorithm_is_send_and_sync() {
        fn _assert<T: Send + Sync + 'static>() {}
        _assert::<Box<dyn StackAlgorithm>>();
        _assert::<Box<dyn AlignAlgorithm>>();

        let algo: Arc<dyn StackAlgorithm> = Arc::new(DummyStack::new());
        let (rect, tiles) = sample_tiles(2, 4, 4);
        let ctx = TileCtx::new(rect, &tiles);

        std::thread::scope(|s| {
            for _ in 0..4 {
                let algo = Arc::clone(&algo);
                let ctx = &ctx;
                s.spawn(move || {
                    let out = algo.fuse(ctx, &ParamValues::new()).unwrap();
                    assert_eq!(out.zmap.len(), (rect.width * rect.height) as usize);
                });
            }
        });
    }

    #[test]
    fn params_validate_against_the_schema() {
        let algo = DummyStack::new();
        let mut cfg = ParamValues::new();
        cfg.insert(
            "count-based-threshold".to_string(),
            ParamValue::Float(9999.0),
        );
        let direct_err = crate::params::validate(algo.params(), &cfg).unwrap_err();
        assert!(direct_err.to_string().contains("count-based-threshold"));

        let (rect, tiles) = sample_tiles(1, 2, 2);
        let ctx = TileCtx::new(rect, &tiles);
        match algo.fuse(&ctx, &cfg) {
            Err(AlgorithmError::Params(inner)) => {
                assert_eq!(inner.to_string(), direct_err.to_string());
            }
            other => panic!("expected AlgorithmError::Params, got {other:?}"),
        }
    }

    #[test]
    fn fusion_output_carries_a_z_map() {
        let algo = DummyStack::new();
        let (rect, tiles) = sample_tiles(3, 4, 4);
        let ctx = TileCtx::new(rect, &tiles);
        let out = algo.fuse(&ctx, &ParamValues::new()).unwrap();

        assert_eq!(out.zmap.len(), (rect.width * rect.height) as usize);
        assert!(
            out.zmap
                .iter()
                .all(|&index| (index as usize) < ctx.frame_count())
        );
    }

    #[test]
    fn frame_out_of_range_names_the_index_and_the_count() {
        let (rect, tiles) = sample_tiles(3, 2, 2);
        let ctx = TileCtx::new(rect, &tiles);

        assert!(ctx.frame(2).is_ok());
        let err = ctx.frame(3).expect_err("frame 3 of 3 must be out of range");
        assert!(matches!(
            err,
            AlgorithmError::FrameOutOfRange {
                frame: 3,
                frame_count: 3
            }
        ));
        let rendered = err.to_string();
        assert!(
            rendered.contains('3'),
            "message must name the index: {rendered}"
        );
    }

    #[test]
    fn work_method_reports_failure() {
        let reader = VecReader {
            frames: vec![vec![0u16; 4 * 4 * 3]],
            width: 4,
            height: 4,
            spp: 3,
        };
        let stack_ctx = StackContext::new(1, 4, 4);
        let ctx = AlignContext::new(stack_ctx, 0, &reader);

        let err = FailingAlign
            .align_frame(&ctx, 0, &ParamValues::new())
            .unwrap_err();
        match &err {
            AlgorithmError::Failed { algorithm, detail } => {
                let msg = err.to_string();
                assert!(msg.contains(algorithm));
                assert!(msg.contains(detail));
            }
            other => panic!("expected Failed, got {other:?}"),
        }

        let rect = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        let mut too_small = vec![0u16; 1];
        let read_err = reader.read_tile_into(0, rect, &mut too_small).unwrap_err();
        let expected_source = TiffError::BufferSize {
            expected: 4 * 4 * 3,
            got: 1,
        };
        match &read_err {
            AlgorithmError::Tiff(inner) => {
                assert_eq!(inner.to_string(), expected_source.to_string());
            }
            other => panic!("expected Tiff, got {other:?}"),
        }
    }

    #[test]
    fn tile_levels_default_is_zero_when_the_schema_declares_no_levels_key() {
        // `DummyStack`'s schema is `count-based-threshold` + `mode`, never
        // `levels`, so the resolved map never carries that key -- the
        // default answers "no pyramid, no margin".
        let algo = DummyStack::new();
        assert_eq!(algo.tile_levels(&ParamValues::new()), 0);
    }

    #[test]
    fn tile_levels_default_reads_the_levels_key_when_present() {
        let algo = DummyStack::new();
        let mut cfg = ParamValues::new();
        cfg.insert("levels".to_string(), ParamValue::Int(7));
        assert_eq!(algo.tile_levels(&cfg), 7);
    }

    #[test]
    fn auto_hook_reads_the_stack_context() {
        let ctx: StackContext = crate::params::StackContext::new(5, 10, 10);
        let algo = DummyStack::new();
        let resolved = crate::params::resolve(algo.params(), &ParamValues::new(), &ctx).unwrap();
        assert_eq!(
            resolved["count-based-threshold"].provenance,
            Provenance::Auto
        );
        assert_eq!(
            resolved["count-based-threshold"].value,
            ParamValue::Float(5.0)
        );
    }

    #[test]
    fn align_context_reads_tiles() {
        let samples: Vec<u16> = (0..(4 * 4 * 3)).map(|i| i as u16).collect();
        let reader = VecReader {
            frames: vec![samples],
            width: 4,
            height: 4,
            spp: 3,
        };
        let stack_ctx = StackContext::new(1, 4, 4);
        let ctx = AlignContext::new(stack_ctx, 0, &reader);

        let rect = Rect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        let mut out = vec![0u16; 2 * 2 * 3];
        ctx.read_tile_into(0, rect, &mut out).unwrap();

        let mut direct = vec![0u16; 2 * 2 * 3];
        reader.read_tile_into(0, rect, &mut direct).unwrap();
        assert_eq!(out, direct);

        let mut wrong = vec![0u16; 3];
        match ctx.read_tile_into(0, rect, &mut wrong) {
            Err(AlgorithmError::Tiff(TiffError::BufferSize { .. })) => {}
            other => panic!("expected BufferSize error, got {other:?}"),
        }
        assert_eq!(
            wrong,
            vec![0u16; 3],
            "a rejected buffer must not be partially written"
        );
    }
}
