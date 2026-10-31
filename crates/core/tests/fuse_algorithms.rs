// SPDX-License-Identifier: GPL-3.0-or-later

//! Fixture-backed driver tests for `weighted-average` and `depth-map` over
//! `fixtures/mantis-512`. The scheduler's `levels` comes from
//! `algo.tile_support(&values)`, the same bridge the CLI uses; and
//! `levels_override` exists solely for the halo-free comparison
//! (`the_trait_default_would_have_seamed`).

mod common;

use std::path::PathBuf;
use std::sync::OnceLock;

use focusforge_core::algorithm::{AlgorithmError, TileCtx, TileReader};
use focusforge_core::align::{Transform, WarpingReader};
use focusforge_core::fuse::{FusionConsumer, FusionGeometry, Plane, build, metrics};
use focusforge_core::io::{Rect, TiffStack};
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::registry;
use focusforge_core::tile::plan::{TilePlan, TileSupport};
use focusforge_core::tile::{NoopPreprocess, TileScheduler, TileSchedulerOptions};
use focusforge_core::zmap::NO_SOURCE;
use focusforge_core::zmap::metrics::all_eight_identical_fraction;

/// One fusion run's outputs, read back from disk into plain sample buffers.
struct FusionResult {
    composite: Vec<u16>,
    zmap: Vec<u16>,
    width: u32,
    height: u32,
    spp: u32,
    frame_count: usize,
}

/// Runs `algo_id` at `(tile_size, levels)` over `fixtures/mantis-512` into
/// `.scratch/fuse/<label>.tif` + `-zmap.tif`, and reads both back. `levels`
/// comes from `algo.tile_support(&values)` unless `levels_override` is
/// given -- the seam demonstration's only reason to exist.
fn run_fusion(
    algo_id: &str,
    label: &str,
    tile_size: u32,
    explicit: &ParamValues,
    levels_override: Option<u32>,
) -> FusionResult {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    run_fusion_over(
        stack_context(&stack),
        &stack,
        algo_id,
        label,
        tile_size,
        explicit,
        levels_override,
    )
}

/// [`run_fusion`] over `reader`, which serves the frames `stack_ctx`
/// describes (the same stack, a warping reader wrapped around it, or
/// in-memory frames).
fn run_fusion_over(
    stack_ctx: StackContext,
    reader: &dyn TileReader,
    algo_id: &str,
    label: &str,
    tile_size: u32,
    explicit: &ParamValues,
    levels_override: Option<u32>,
) -> FusionResult {
    let (width, height) = (stack_ctx.width, stack_ctx.height);
    let spp = stack_ctx.samples_per_pixel;
    let frame_count = stack_ctx.frame_count;

    let algo =
        registry::find_stack(algo_id).unwrap_or_else(|e| panic!("{algo_id} must resolve: {e}"));
    let resolved =
        params::resolve(algo.params(), explicit, &stack_ctx).expect("resolve must succeed");
    let values = params::resolved_values(&resolved);

    // The same bridge `crates/cli/src/stack.rs` uses: the algorithm names
    // its own margin requirement, never a number this test supplies
    // separately.
    let support = levels_override
        .map(TileSupport::from)
        .unwrap_or_else(|| algo.tile_support(&values));

    let scratch_dir = common::workspace_root().join(".scratch").join("fuse");
    std::fs::create_dir_all(&scratch_dir).expect("scratch dir must be creatable");
    let composite_path = scratch_dir.join(format!("{label}.tif"));
    let zmap_path = scratch_dir.join(format!("{label}-zmap.tif"));

    let scheduler_scratch =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("fuse_algorithms_{label}"));
    let mut options = TileSchedulerOptions::explicit_budget(scheduler_scratch, 1 << 30);
    options.apply_support(support);
    options.tile_size = tile_size;
    let scheduler =
        TileScheduler::new(reader, stack_ctx, spp, &options).expect("scheduler must build");
    let consumer = FusionConsumer::create(
        algo,
        values,
        &composite_path,
        Some(&zmap_path),
        FusionGeometry {
            width,
            height,
            samples_per_pixel: spp,
            frame_count: frame_count as u32,
            crop: None,
            description: None,
        },
    )
    .expect("consumer must create");
    scheduler
        .run(&NoopPreprocess, &consumer)
        .expect("run must succeed");
    consumer.finish().expect("finish must succeed");

    let full_rect = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let composite = TiffStack::open_files(&[composite_path])
        .expect("composite must reopen")
        .read_tile(0, full_rect)
        .expect("composite read")
        .samples;
    let zmap = TiffStack::open_files(&[zmap_path])
        .expect("zmap must reopen")
        .read_tile(0, full_rect)
        .expect("zmap read")
        .samples;

    FusionResult {
        composite,
        zmap,
        width,
        height,
        spp,
        frame_count,
    }
}

fn no_explicit() -> ParamValues {
    ParamValues::new()
}

fn stack_context(stack: &TiffStack) -> StackContext {
    let (width, height) = stack.dimensions();
    StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    )
}

/// Full frames held in memory, served as a [`TileReader`].
struct MemoryFrames {
    ctx: StackContext,
    frames: Vec<Vec<u16>>,
}

impl TileReader for MemoryFrames {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        let spp = self.ctx.samples_per_pixel as usize;
        let row_len = rect.width as usize * spp;
        for row in 0..rect.height as usize {
            let src = ((rect.y as usize + row) * self.ctx.width as usize + rect.x as usize) * spp;
            out[row * row_len..(row + 1) * row_len]
                .copy_from_slice(&self.frames[frame][src..src + row_len]);
        }
        Ok(())
    }
}

fn weighted_average_tiled() -> &'static FusionResult {
    static ONCE: OnceLock<FusionResult> = OnceLock::new();
    ONCE.get_or_init(|| {
        run_fusion(
            "weighted-average",
            "weighted-average-256",
            256,
            &no_explicit(),
            None,
        )
    })
}

fn weighted_average_single_tile() -> &'static FusionResult {
    static ONCE: OnceLock<FusionResult> = OnceLock::new();
    ONCE.get_or_init(|| {
        run_fusion(
            "weighted-average",
            "weighted-average-704",
            704,
            &no_explicit(),
            None,
        )
    })
}

/// The same tiled (256) run, but with the scheduler forced to the trait
/// *default* `tile_support` would answer (`0`, i.e. no halo at all) --
/// the halo-free comparison the rest of this suite checks against.
fn weighted_average_forced_to_the_default_levels() -> &'static FusionResult {
    static ONCE: OnceLock<FusionResult> = OnceLock::new();
    ONCE.get_or_init(|| {
        run_fusion(
            "weighted-average",
            "weighted-average-256-levels0",
            256,
            &no_explicit(),
            Some(0),
        )
    })
}

fn depth_map_tiled() -> &'static FusionResult {
    static ONCE: OnceLock<FusionResult> = OnceLock::new();
    ONCE.get_or_init(|| run_fusion("depth-map", "depth-map-256", 256, &no_explicit(), None))
}

fn depth_map_single_tile() -> &'static FusionResult {
    static ONCE: OnceLock<FusionResult> = OnceLock::new();
    ONCE.get_or_init(|| run_fusion("depth-map", "depth-map-704", 704, &no_explicit(), None))
}

/// `depth-map` at `(score-radius, smoothing-radius) = (0, 0)`, single-tile
/// so the coherence comparison is not confounded by any tiling seam.
fn depth_map_zero_radius() -> &'static FusionResult {
    static ONCE: OnceLock<FusionResult> = OnceLock::new();
    ONCE.get_or_init(|| {
        let mut explicit = ParamValues::new();
        explicit.insert("score-radius".to_string(), ParamValue::Int(0));
        explicit.insert("smoothing-radius".to_string(), ParamValue::Int(0));
        run_fusion(
            "depth-map",
            "depth-map-704-zero-radius",
            704,
            &explicit,
            None,
        )
    })
}

/// Every frame of `fixtures/mantis-512`, read once and shared -- the source
/// data `depth_map_composite_is_its_zmap` and
/// `weighted_average_zmap_follows_its_rule` both check the algorithms'
/// outputs against.
fn all_frames() -> &'static (Vec<Vec<u16>>, u32, u32, u32) {
    static ONCE: OnceLock<(Vec<Vec<u16>>, u32, u32, u32)> = OnceLock::new();
    ONCE.get_or_init(|| {
        let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
        let (width, height) = stack.dimensions();
        let spp = stack.header(0).samples_per_pixel;
        let rect = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let frames = (0..stack.len())
            .map(|f| stack.read_tile(f, rect).expect("frame read").samples)
            .collect();
        (frames, width, height, spp)
    })
}

fn open_frame(path: PathBuf) -> (Vec<u16>, u32, u32, u32) {
    let stack = TiffStack::open_files(&[path]).expect("frame must open");
    let (w, h) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let samples = stack
        .read_tile(
            0,
            Rect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            },
        )
        .expect("frame read")
        .samples;
    (samples, w, h, spp)
}

/// Reimplements `crate::fuse::laplacian::score_plane`'s **documented** rule
/// -- a `(2*radius+1)`-square, clamp-to-edge sum of `|L|` over every
/// channel, in the pinned separable two-pass order -- directly against
/// [`Plane`]'s public accessors. This test crate cannot call the
/// crate-internal (`pub(crate)`) implementation, and should not: this is an
/// independent oracle for the published rule, not a second call to the code
/// under test.
fn reference_score_plane(level: &Plane, radius: u32) -> Vec<f32> {
    let width = level.width();
    let height = level.height();
    let spp = level.samples_per_pixel();
    let data = level.data();
    let r = radius as i64;
    let pixel_count = width as usize * height as usize;
    let clamp = |v: i64, dim: u32| v.clamp(0, dim as i64 - 1) as u32;

    let mut row_sums = vec![0.0f32; pixel_count];
    for y in 0..height {
        for x in 0..width {
            let mut acc = 0.0f32;
            for dx in -r..=r {
                let sx = clamp(x as i64 + dx, width);
                for c in 0..spp {
                    acc += data[((y * width + sx) * spp + c) as usize].abs();
                }
            }
            row_sums[(y * width + x) as usize] = acc;
        }
    }
    let mut scores = vec![0.0f32; pixel_count];
    for y in 0..height {
        for x in 0..width {
            let mut acc = 0.0f32;
            for dy in -r..=r {
                let sy = clamp(y as i64 + dy, height);
                acc += row_sums[(sy * width + x) as usize];
            }
            scores[(y * width + x) as usize] = acc;
        }
    }
    scores
}

#[test]
fn both_algorithms_are_registered() {
    for id in ["weighted-average", "depth-map"] {
        let algo = registry::find_stack(id).unwrap_or_else(|e| panic!("{id} must resolve: {e}"));
        assert_eq!(algo.id(), id);
        assert!(!algo.display_name().is_empty());
        assert!(
            !algo.params().is_empty(),
            "{id} must declare a non-empty schema"
        );
    }
}

#[test]
fn every_parameter_is_declared() {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let stack_ctx = StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    );
    let rect = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let tiles: Vec<_> = (0..stack.len())
        .map(|f| stack.read_tile(f, rect).expect("frame read"))
        .collect();
    let ctx = TileCtx::new(rect, &tiles);

    for id in ["weighted-average", "depth-map"] {
        let algo = registry::find_stack(id).unwrap_or_else(|e| panic!("{id} must resolve: {e}"));
        let resolved = params::resolve(algo.params(), &ParamValues::new(), &stack_ctx)
            .expect("resolve must succeed with no explicit overrides");
        let values = params::resolved_values(&resolved);
        assert_eq!(
            values.len(),
            algo.params().len(),
            "{id}: the resolved map must carry exactly one value per declared parameter"
        );
        algo.fuse(&ctx, &values)
            .unwrap_or_else(|e| panic!("{id} must fuse reading exactly the resolved schema: {e}"));
    }
}

#[test]
fn both_outputs_are_produced() {
    for result in [weighted_average_tiled(), depth_map_tiled()] {
        let pixel_count = (result.width * result.height) as usize;
        assert_eq!(result.composite.len(), pixel_count * result.spp as usize);
        assert_eq!(result.zmap.len(), pixel_count);
        for &index in &result.zmap {
            assert_ne!(index, NO_SOURCE, "neither algorithm may emit NO_SOURCE");
            assert!((index as usize) < result.frame_count);
        }
    }
}

#[test]
fn weighted_average_zmap_follows_its_rule() {
    let (frames, width, height, spp) = all_frames();
    let result = weighted_average_tiled();

    let stack_ctx = StackContext::new(frames.len(), *width, *height, *spp);
    let algo = registry::find_stack("weighted-average").expect("weighted-average must resolve");
    let resolved = params::resolve(algo.params(), &ParamValues::new(), &stack_ctx)
        .expect("resolve must succeed");
    let values = params::resolved_values(&resolved);
    let radius = values
        .get("radius")
        .and_then(ParamValue::as_i64)
        .expect("radius must resolve") as u32;

    // `argmax_f score_f(p)` and `argmax_f score_f(p) ^ weight_exponent` name
    // the same frame for any `weight_exponent > 0`, since `powf` with a
    // positive exponent is strictly order-preserving on non-negative
    // inputs -- so the raw score is enough to recompute the rule
    // independently of whatever exponent the default resolves to.
    let pixel_count = (*width as usize) * (*height as usize);
    let mut best = vec![f32::NEG_INFINITY; pixel_count];
    let mut expected_winner = vec![0u16; pixel_count];
    for (f, samples) in frames.iter().enumerate() {
        let pyramid = build(samples, *width, *height, *spp, 1).expect("build must succeed");
        let scores = reference_score_plane(&pyramid.levels()[0], radius);
        for p in 0..pixel_count {
            if scores[p] > best[p] {
                best[p] = scores[p];
                expected_winner[p] = f as u16;
            }
        }
    }

    assert_eq!(
        result.zmap, expected_winner,
        "the recorded index must be the lowest-indexed frame attaining the maximum weight, \
         recomputed from the published sharpness rule"
    );
}

#[test]
fn depth_map_composite_is_its_zmap() {
    let (frames, width, height, spp) = all_frames();
    let result = depth_map_tiled();
    let spp_usize = *spp as usize;

    for p in 0..(*width as usize * *height as usize) {
        let f = result.zmap[p] as usize;
        let base = p * spp_usize;
        for c in 0..spp_usize {
            assert_eq!(
                result.composite[base + c],
                frames[f][base + c],
                "pixel {p} channel {c}: composite must equal frames[zmap[p]][p]"
            );
        }
    }
}

#[test]
fn tiled_matches_single_tile_for_each() {
    let wa_tiled = weighted_average_tiled();
    let wa_single = weighted_average_single_tile();
    assert_eq!(
        wa_tiled.zmap, wa_single.zmap,
        "weighted-average: tiled (256) and single-tile (704) Z-Maps must be exactly equal"
    );
    assert_eq!(
        wa_tiled.composite, wa_single.composite,
        "weighted-average: tiled (256) and single-tile (704) composites must be exactly equal \
         -- the margin covers the whole reach and no `collapse` follows it"
    );

    let dm_tiled = depth_map_tiled();
    let dm_single = depth_map_single_tile();
    assert_eq!(
        dm_tiled.zmap, dm_single.zmap,
        "depth-map: tiled (256) and single-tile (704) Z-Maps must be exactly equal"
    );
    assert_eq!(
        dm_tiled.composite, dm_single.composite,
        "depth-map: tiled (256) and single-tile (704) composites must be exactly equal -- the \
         composite is a copy of the winning frame's own samples, so an exact Z-Map gives an \
         exact composite"
    );
}

#[test]
fn the_trait_default_would_have_seamed() {
    let forced = weighted_average_forced_to_the_default_levels();
    let single = weighted_average_single_tile();
    let own_answer = weighted_average_tiled();

    assert_eq!(
        own_answer.composite, single.composite,
        "weighted-average's own tile_support answer must still match the single-tile reference"
    );

    let differs = forced.composite != single.composite || forced.zmap != single.zmap;
    assert!(
        differs,
        "forcing the scheduler to the trait default's answer (levels = 0, no halo) must \
         produce a composite or Z-Map that differs from the single-tile reference -- if it does \
         not, this is not evidence that the override is needed"
    );
}

#[test]
fn depth_map_zmap_is_coherent() {
    let default_run = depth_map_single_tile();
    let zero_radius_run = depth_map_zero_radius();

    let default_fraction =
        all_eight_identical_fraction(&default_run.zmap, default_run.width, default_run.height);
    let zero_fraction = all_eight_identical_fraction(
        &zero_radius_run.zmap,
        zero_radius_run.width,
        zero_radius_run.height,
    );
    println!(
        "depth_map_zmap_is_coherent: all-8-identical default(4,8)={default_fraction:.4} \
         (0,0)={zero_fraction:.4}"
    );
    assert!(
        default_fraction > zero_fraction,
        "the default (score-radius 4, smoothing-radius 8) must be strictly more spatially \
         coherent than (0, 0) over the same input: default={default_fraction:.4}, \
         zero={zero_fraction:.4}"
    );
}

#[test]
fn sharpness_is_recorded_for_each() {
    let wa = weighted_average_tiled();
    let dm = depth_map_tiled();
    let (frame03, w, h, spp) = open_frame(common::mantis_512_dir().join("frames").join("03.tif"));
    let golden_dir = common::fixtures_dir().join("mantis-512").join("golden");
    let (laplacian_golden, ..) = open_frame(golden_dir.join("laplacian.tif"));

    let lapvar_wa = metrics::laplacian_variance(&wa.composite, wa.width, wa.height, wa.spp);
    let lapvar_dm = metrics::laplacian_variance(&dm.composite, dm.width, dm.height, dm.spp);
    let lapvar_laplacian = metrics::laplacian_variance(&laplacian_golden, w, h, spp);
    let lapvar_03 = metrics::laplacian_variance(&frame03, w, h, spp);

    println!(
        "sharpness_is_recorded_for_each: weighted-average={lapvar_wa:.1} depth-map={lapvar_dm:.1} \
         laplacian={lapvar_laplacian:.1} 03.tif={lapvar_03:.1}"
    );

    // Assert only the alignment-independent ordering. Both absolutes, and
    // laplacian's, are recorded above and nowhere asserted against a floor.
    assert!(
        lapvar_dm > lapvar_wa,
        "depth-map's composite must be strictly sharper than weighted-average's: \
         depth-map={lapvar_dm:.1}, weighted-average={lapvar_wa:.1}"
    );
}

/// Frame 3 of `fixtures/mantis-512` translated by `(-12.5, 0)` through a
/// [`WarpingReader`], no crop: the frame's left columns have no source.
const SHIFTED_FRAME: usize = 3;

fn shifted_transforms(frame_count: usize) -> Vec<Transform> {
    let mut transforms = vec![Transform::identity(); frame_count];
    transforms[SHIFTED_FRAME] = Transform::translation(-12.5, 0.0);
    transforms
}

fn run_shifted(algo_id: &str, label: &str, tile_size: u32) -> FusionResult {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let reader = WarpingReader::new(
        &stack,
        (width, height),
        spp,
        shifted_transforms(stack.len()),
    );
    run_fusion_over(
        stack_context(&stack),
        &reader,
        algo_id,
        label,
        tile_size,
        &no_explicit(),
        None,
    )
}

/// Every frame as the warping reader serves it, with its coverage mask
/// (`None`: covered everywhere).
fn warped_frames() -> (Vec<Vec<u16>>, Vec<Option<Vec<bool>>>) {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let reader = WarpingReader::new(
        &stack,
        (width, height),
        spp,
        shifted_transforms(stack.len()),
    );
    let rect = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let mut frames = Vec::new();
    let mut coverage = Vec::new();
    for f in 0..stack.len() {
        let mut samples = vec![0u16; (width * height * spp) as usize];
        coverage.push(
            reader
                .read_tile_coverage_into(f, rect, &mut samples)
                .expect("warped read"),
        );
        frames.push(samples);
    }
    (frames, coverage)
}

/// Mean over pixels with an uncovered source of how far the composite's
/// luma (channel mean) falls below the darkest covered source's.
fn border_pull(result: &FusionResult, frames: &[Vec<u16>], coverage: &[Option<Vec<bool>>]) -> f64 {
    let spp = result.spp as usize;
    let luma = |samples: &[u16], p: usize| {
        samples[p * spp..(p + 1) * spp]
            .iter()
            .map(|&s| f64::from(s))
            .sum::<f64>()
            / spp as f64
    };
    let (mut total, mut count) = (0.0, 0u64);
    for p in 0..(result.width * result.height) as usize {
        let covered = |f: usize| coverage[f].as_ref().is_none_or(|c| c[p]);
        if (0..frames.len()).all(covered) {
            continue;
        }
        let darkest = (0..frames.len())
            .filter(|&f| covered(f))
            .map(|f| luma(&frames[f], p))
            .fold(f64::INFINITY, f64::min);
        total += (darkest - luma(&result.composite, p)).max(0.0);
        count += 1;
    }
    assert!(count > 0, "the shifted frame must leave pixels uncovered");
    total / count as f64
}

#[test]
fn uncovered_border_never_wins_or_darkens() {
    let (frames, coverage) = warped_frames();
    for algo_id in ["laplacian", "weighted-average", "depth-map"] {
        let result = run_shifted(algo_id, &format!("{algo_id}-shifted"), 512);
        for (p, &f) in result.zmap.iter().enumerate() {
            assert!(
                f != NO_SOURCE && coverage[f as usize].as_ref().is_none_or(|c| c[p]),
                "{algo_id}: pixel {p} names frame {f}, which has no source there"
            );
        }
        let pull = border_pull(&result, &frames, &coverage);
        println!("uncovered_border_never_wins_or_darkens: {algo_id} border_pull={pull:.1}");
        assert!(pull <= 200.0, "{algo_id}: border_pull {pull:.1} counts");
        if algo_id == "depth-map" {
            let spp = result.spp as usize;
            for (p, &f) in result.zmap.iter().enumerate() {
                assert_eq!(
                    result.composite[p * spp..(p + 1) * spp],
                    frames[f as usize][p * spp..(p + 1) * spp],
                    "depth-map: pixel {p} must copy its winner"
                );
            }
        }
    }
}

#[test]
fn masked_tiled_matches_single_tile() {
    for algo_id in ["laplacian", "weighted-average", "depth-map"] {
        let large = run_shifted(algo_id, &format!("{algo_id}-shifted-1024"), 1024);
        for tile_size in [448, 480] {
            let small = run_shifted(
                algo_id,
                &format!("{algo_id}-shifted-{tile_size}"),
                tile_size,
            );
            assert_eq!(
                small.zmap, large.zmap,
                "{algo_id} at {tile_size}: Z-Maps differ"
            );
            assert_eq!(
                small.composite, large.composite,
                "{algo_id} at {tile_size}: composites differ"
            );
        }
    }
}

const WINDOW: Rect = Rect {
    x: 1,
    y: 3,
    width: 509,
    height: 437,
};

/// The mantis-512 frames cut to [`WINDOW`].
fn mantis_window() -> MemoryFrames {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let spp = stack.header(0).samples_per_pixel;
    MemoryFrames {
        ctx: StackContext::new(stack.len(), WINDOW.width, WINDOW.height, spp),
        frames: (0..stack.len())
            .map(|f| stack.read_tile(f, WINDOW).expect("window read").samples)
            .collect(),
    }
}

/// Sets a 16x16 block of one channel of frame 2 to full scale, ending at the
/// bottom-right pixel of tile 0's read rect for `support` at `tile_size`: an
/// interior read-rect edge, where the neighbouring tiles' interiors begin.
fn put_impulse_at_the_first_read_corner(
    frames: &mut MemoryFrames,
    support: TileSupport,
    tile_size: u32,
) {
    let spp = frames.ctx.samples_per_pixel;
    let plan = TilePlan::new(WINDOW.width, WINDOW.height, tile_size, support)
        .expect("the window must plan");
    let read = plan.tiles()[0].read();
    let (right, bottom) = (read.x + read.width, read.y + read.height);
    assert!(
        right < WINDOW.width && bottom < WINDOW.height,
        "the edge must be interior"
    );
    for y in bottom - 16..bottom {
        for x in right - 16..right {
            frames.frames[2][((y * WINDOW.width + x) * spp + 1) as usize] = u16::MAX;
        }
    }
}

#[test]
fn odd_window_with_a_boundary_impulse_is_tile_invariant() {
    for (source, make_frames) in [
        ("mantis", mantis_window as fn() -> MemoryFrames),
        ("synthetic", || {
            synthetic_frames(WINDOW.width, WINDOW.height, 4)
        }),
    ] {
        for algo_id in ["laplacian", "weighted-average", "depth-map"] {
            let mut frames = make_frames();
            let algo = registry::find_stack(algo_id).expect("method is registered");
            let resolved = params::resolve(algo.params(), &no_explicit(), &frames.ctx)
                .expect("resolve must succeed");
            let support = algo.tile_support(&params::resolved_values(&resolved));
            if source == "mantis" {
                put_impulse_at_the_first_read_corner(&mut frames, support, 448);
            }
            let run = |tile_size: u32| {
                run_fusion_over(
                    frames.ctx,
                    &frames,
                    algo_id,
                    &format!("{algo_id}-odd-window-{source}-{tile_size}"),
                    tile_size,
                    &no_explicit(),
                    None,
                )
            };
            let tiled = run(448);
            let single = run(1024);
            assert_eq!(tiled.zmap, single.zmap, "{source} {algo_id}: Z-Maps differ");
            assert_eq!(
                tiled.composite, single.composite,
                "{source} {algo_id}: composites differ"
            );
        }
    }
}

/// Deterministic textured frames: every frame carries noise of a similar
/// amplitude, a little stronger in staggered blocks, so scores are near-tied
/// almost everywhere and selections change hands across every tile edge.
fn synthetic_frames(width: u32, height: u32, frame_count: usize) -> MemoryFrames {
    let spp = 3u32;
    let hash = |x: u32, y: u32, f: u32, c: u32| {
        let mut h = x
            .wrapping_mul(0x9E37_79B1)
            .wrapping_add(y.wrapping_mul(0x85EB_CA77))
            .wrapping_add(f.wrapping_mul(0xC2B2_AE3D))
            .wrapping_add(c.wrapping_mul(0x27D4_EB2F));
        h ^= h >> 15;
        h = h.wrapping_mul(0x2C1B_3C6D);
        h ^= h >> 12;
        h = h.wrapping_mul(0x297A_2D39);
        h ^= h >> 15;
        (h & 0xFFFF) as i64 - 32768
    };
    let frames = (0..frame_count as u32)
        .map(|f| {
            let mut samples = Vec::with_capacity((width * height * spp) as usize);
            for y in 0..height {
                for x in 0..width {
                    let sharp = (x / 97 + y / 89 + f) % frame_count as u32 == 0;
                    let amplitude = if sharp { 4000 } else { 3200 };
                    for c in 0..spp {
                        let base = 20000 + (x + 2 * y) as i64 + 3000 * c as i64;
                        let value = base + hash(x, y, f, c) * amplitude / 32768;
                        samples.push(value.clamp(0, 65535) as u16);
                    }
                }
            }
            samples
        })
        .collect();
    MemoryFrames {
        ctx: StackContext::new(frame_count, width, height, spp),
        frames,
    }
}

/// The shipped default geometry for Laplacian at 5 levels (tile 704, margin
/// 160) over a frame of 1500x1600: the width leaves a ragged last tile of its
/// own, the height absorbs its remainder into the previous tile.
#[test]
fn laplacian_default_tile_geometry_is_tile_invariant() {
    let frames = synthetic_frames(1500, 1600, 4);
    let algo = registry::find_stack("laplacian").expect("laplacian is registered");
    let resolved =
        params::resolve(algo.params(), &no_explicit(), &frames.ctx).expect("resolve must succeed");
    let support = algo.tile_support(&params::resolved_values(&resolved));
    assert_eq!(support.margin(), 160);
    let default_tile = support.default_tile_size();
    assert_eq!(default_tile, 704);
    let plan = TilePlan::new(1500, 1600, default_tile, support).expect("default tile plans");
    assert!(plan.len() > 9, "the frame must plan several tiles");

    let run = |label: &str, tile_size: u32| {
        run_fusion_over(
            frames.ctx,
            &frames,
            "laplacian",
            label,
            tile_size,
            &no_explicit(),
            None,
        )
    };
    let tiled = run("laplacian-synthetic-default", default_tile);
    let single = run("laplacian-synthetic-single", 2048);
    assert_eq!(tiled.zmap, single.zmap, "Z-Maps differ");
    assert_eq!(tiled.composite, single.composite, "composites differ");
}

/// An on-demand context over a warping reader fuses a non-square rect at an
/// odd origin, which reaches the uncovered band, to the same bytes as a
/// resident context over the same tiles and masks.
#[test]
fn streamed_context_matches_the_resident_context() {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let reader = WarpingReader::new(
        &stack,
        (width, height),
        spp,
        shifted_transforms(frame_count),
    );
    let rect = Rect {
        x: 1,
        y: 3,
        width: 301,
        height: 187,
    };
    let len = (rect.width * rect.height * spp) as usize;

    let mut tiles = Vec::new();
    let mut coverage = Vec::new();
    for f in 0..frame_count {
        let mut samples = vec![0u16; len];
        coverage.push(
            reader
                .read_tile_coverage_into(f, rect, &mut samples)
                .expect("warped read"),
        );
        tiles.push(focusforge_core::tile::Tile {
            rect,
            samples_per_pixel: spp,
            samples,
        });
    }
    assert!(
        coverage[SHIFTED_FRAME]
            .as_ref()
            .is_some_and(|mask| mask.contains(&false)),
        "the rect must reach the uncovered band"
    );

    let stack_ctx = stack_context(&stack);
    for algo_id in ["laplacian", "weighted-average", "depth-map"] {
        let algo = registry::find_stack(algo_id).expect("method must resolve");
        let mut explicit = ParamValues::new();
        if algo_id == "laplacian" {
            explicit.insert("levels".to_string(), ParamValue::Int(3));
        }
        let resolved = params::resolve(algo.params(), &explicit, &stack_ctx).expect("resolve");
        let values = params::resolved_values(&resolved);

        let resident = TileCtx::new(rect, &tiles).with_coverage(&coverage);
        let streamed = TileCtx::on_demand(rect, frame_count, spp, &reader, &NoopPreprocess);
        let expected = algo.fuse(&resident, &values).expect("resident fuse");
        let actual = algo.fuse(&streamed, &values).expect("streamed fuse");

        assert_eq!(streamed.frames_read(), frame_count, "{algo_id}");
        assert!(
            actual.zmap == expected.zmap,
            "{algo_id}: Z-Maps differ between streamed and resident contexts"
        );
        assert!(
            actual.tile == expected.tile,
            "{algo_id}: composites differ between streamed and resident contexts"
        );
    }
}

/// Frames served as `f32` only; the `u16` read fails, so a fusion that
/// still reads `u16` cannot pass.
struct WideFrames<'a> {
    inner: &'a dyn TileReader,
}

impl TileReader for WideFrames<'_> {
    fn sample_format(&self) -> focusforge_core::tile::SampleFormat {
        focusforge_core::tile::SampleFormat::F32
    }

    fn read_tile_into(&self, _: usize, _: Rect, _: &mut [u16]) -> Result<(), AlgorithmError> {
        Err(AlgorithmError::Failed {
            algorithm: "wide-frames".to_string(),
            detail: "serves f32 only".to_string(),
        })
    }

    fn read_tile_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        let mut narrow = vec![0u16; out.len()];
        self.inner.read_tile_into(frame, rect, &mut narrow)?;
        for (wide, narrow) in out.iter_mut().zip(narrow) {
            *wide = f32::from(narrow);
        }
        Ok(())
    }

    fn read_tile_coverage_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [f32],
    ) -> Result<Option<Vec<bool>>, AlgorithmError> {
        self.read_tile_f32_into(frame, rect, out).map(|()| None)
    }
}

/// Flat `f32` frames, one value per frame.
struct FlatWide(Vec<f32>);

impl TileReader for FlatWide {
    fn sample_format(&self) -> focusforge_core::tile::SampleFormat {
        focusforge_core::tile::SampleFormat::F32
    }

    fn read_tile_into(&self, _: usize, _: Rect, _: &mut [u16]) -> Result<(), AlgorithmError> {
        Err(AlgorithmError::Failed {
            algorithm: "flat-wide".to_string(),
            detail: "serves f32 only".to_string(),
        })
    }

    fn read_tile_f32_into(
        &self,
        frame: usize,
        _: Rect,
        out: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        out.fill(self.0[frame]);
        Ok(())
    }

    fn read_tile_coverage_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [f32],
    ) -> Result<Option<Vec<bool>>, AlgorithmError> {
        self.read_tile_f32_into(frame, rect, out).map(|()| None)
    }
}

#[test]
fn f32_fusion_matches_the_u16_run_on_widened_frames() {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let wide = WideFrames { inner: &stack };
    for algo_id in ["laplacian", "depth-map", "weighted-average"] {
        let narrow = run_fusion_over(
            stack_context(&stack),
            &stack,
            algo_id,
            &format!("{algo_id}-u16-480"),
            480,
            &no_explicit(),
            None,
        );
        let widened = run_fusion_over(
            stack_context(&stack),
            &wide,
            algo_id,
            &format!("{algo_id}-f32-480"),
            480,
            &no_explicit(),
            None,
        );
        assert_eq!(widened.zmap, narrow.zmap, "{algo_id}: Z-Maps differ");
        assert_eq!(
            widened.composite, narrow.composite,
            "{algo_id}: composites differ"
        );
    }
}

#[test]
fn f32_fusion_clips_once_at_the_output() {
    let mut explicit = ParamValues::new();
    explicit.insert("levels".to_string(), ParamValue::Int(2));
    let flat = FlatWide(vec![70_000.0, 50_000.0]);
    let context = StackContext::new(2, 128, 128, 3);
    // Averaging methods mean the unclipped values (60000, not the 57768 of
    // a mean of clipped reads); selection copies frame 0 and clips it.
    for (algo_id, expected) in [
        ("weighted-average", 60_000),
        ("laplacian", 60_000),
        ("depth-map", 65_535),
    ] {
        let result = run_fusion_over(
            context,
            &flat,
            algo_id,
            &format!("{algo_id}-f32-flat"),
            112,
            &explicit,
            None,
        );
        assert!(
            result.composite.iter().all(|v| v.abs_diff(expected) <= 1),
            "{algo_id}: expected {expected}, got {:?}",
            &result.composite[..3]
        );
    }
}
