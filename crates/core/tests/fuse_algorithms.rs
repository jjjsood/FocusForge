// SPDX-License-Identifier: GPL-3.0-or-later

//! Fixture-backed driver tests for `weighted-average` and `depth-map` over
//! `fixtures/mantis-512`, modelled on `crates/core/tests/laplacian_fusion.rs`
//! but local rather than lifted into `tests/common/mod.rs`, so that settled
//! file is not touched. Two differences from its model, both load-bearing:
//! the scheduler's `levels` comes from `algo.tile_levels(&values)`, exactly
//! as `crates/cli/src/stack.rs` does, so this test exercises the same
//! bridge the CLI uses; and `levels_override` exists solely for the seam
//! demonstration (`the_trait_default_would_have_seamed`).

mod common;

use std::path::PathBuf;
use std::sync::OnceLock;

use focusforge_core::algorithm::TileCtx;
use focusforge_core::fuse::{FusionConsumer, FusionGeometry, Plane, build, metrics};
use focusforge_core::io::{Rect, TiffStack};
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::registry;
use focusforge_core::tile::{NoopPreprocess, TileScheduler, TileSchedulerOptions};
use focusforge_core::zmap::NO_SOURCE;

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
/// comes from `algo.tile_levels(&values)` unless `levels_override` is
/// given -- the seam demonstration's only reason to exist.
fn run_fusion(
    algo_id: &str,
    label: &str,
    tile_size: u32,
    explicit: &ParamValues,
    levels_override: Option<u32>,
) -> FusionResult {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let stack_ctx = StackContext::new(frame_count, width, height);

    let algo =
        registry::find_stack(algo_id).unwrap_or_else(|e| panic!("{algo_id} must resolve: {e}"));
    let resolved =
        params::resolve(algo.params(), explicit, &stack_ctx).expect("resolve must succeed");
    let values = params::resolved_values(&resolved);

    // The same bridge `crates/cli/src/stack.rs` uses: the algorithm names
    // its own margin requirement, never a number this test supplies
    // separately.
    let levels = levels_override.unwrap_or_else(|| algo.tile_levels(&values));

    let scratch_dir = common::workspace_root().join(".scratch").join("fuse");
    std::fs::create_dir_all(&scratch_dir).expect("scratch dir must be creatable");
    let composite_path = scratch_dir.join(format!("{label}.tif"));
    let zmap_path = scratch_dir.join(format!("{label}-zmap.tif"));

    let scheduler_scratch =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("fuse_algorithms_{label}"));
    let options = TileSchedulerOptions {
        tile_size,
        levels,
        max_bytes_in_flight: 1 << 30,
        scratch_base: scheduler_scratch,
    };
    let scheduler =
        TileScheduler::new(&stack, stack_ctx, spp, &options).expect("scheduler must build");
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
/// *default* `tile_levels` would answer (`0`, i.e. no halo at all) --
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

/// The fraction of pixels whose **entire** (present) 8-neighbourhood shares
/// the pixel's own winner. Copied from `laplacian_fusion.rs`, keeping its
/// border convention (an out-of-range neighbour is skipped, never counted
/// as a mismatch).
fn all_eight_neighbours_identical_fraction(zmap: &[u16], width: u32, height: u32) -> f64 {
    let mut identical = 0usize;
    let total = (width * height) as usize;
    for y in 0..height as i64 {
        for x in 0..width as i64 {
            let center = zmap[(y as u32 * width + x as u32) as usize];
            let mut all_match = true;
            'neighbours: for dy in -1i64..=1 {
                for dx in -1i64..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || ny < 0 || nx >= width as i64 || ny >= height as i64 {
                        continue;
                    }
                    let idx = ny as u32 * width + nx as u32;
                    if zmap[idx as usize] != center {
                        all_match = false;
                        break 'neighbours;
                    }
                }
            }
            if all_match {
                identical += 1;
            }
        }
    }
    identical as f64 / total as f64
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
    let stack_ctx = StackContext::new(stack.len(), width, height);
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

    let stack_ctx = StackContext::new(frames.len(), *width, *height);
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
        "weighted-average's own tile_levels answer must still match the single-tile reference"
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

    let default_fraction = all_eight_neighbours_identical_fraction(
        &default_run.zmap,
        default_run.width,
        default_run.height,
    );
    let zero_fraction = all_eight_neighbours_identical_fraction(
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
