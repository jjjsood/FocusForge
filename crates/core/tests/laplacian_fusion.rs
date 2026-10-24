// SPDX-License-Identifier: GPL-3.0-or-later

//! Fixture-backed driver tests for `laplacian` over `fixtures/mantis-512`,
//! run at tile 448 / levels 5 (the golden geometry), plus the exact tile
//! invariance comparisons against a 1024/5 single tile and a 480/5 ragged
//! variant.
//!
//! `metrics_reproduce_the_measured_baselines` is the calibration check:
//! every floor below is meaningless unless it passes.

mod common;

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use focusforge_core::algorithm::{AlgorithmError, TileCtx, TileReader};
use focusforge_core::align::{
    FailurePolicy, StackAlignment, Transform, WarpingReader, align_stack, auto_crop,
};
use focusforge_core::fuse::{FusionConsumer, FusionGeometry, metrics};
use focusforge_core::io::{Rect, TiffStack};
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::registry;
use focusforge_core::tile::{NoopPreprocess, TileScheduler, TileSchedulerOptions};
use focusforge_core::zmap::NO_SOURCE;
use focusforge_core::zmap::metrics::all_eight_identical_fraction;

/// One fusion run's outputs, read back from disk into plain sample
/// buffers, plus the geometry needed to interpret them. `width`/`height`
/// are the **output** extent: the crop rect's dimensions when `crop` is
/// `Some`, the full frame otherwise.
struct FusionResult {
    composite: Vec<u16>,
    zmap: Vec<u16>,
    width: u32,
    height: u32,
    spp: u32,
    frame_count: usize,
    /// The crop rect this run was declared at, in absolute frame
    /// coordinates -- `None` for an unaligned (full-frame) run.
    crop: Option<Rect>,
}

/// Runs `laplacian` at `(tile_size, levels)` over `fixtures/mantis-512`,
/// reading through `reader` (`&TiffStack` for an unaligned run, a
/// [`WarpingReader`] for an aligned one) and declaring both outputs at
/// `crop.unwrap_or(full_frame)`, into `.scratch/fuse/<label>.tif` +
/// `-zmap.tif`, then reads both back.
#[allow(clippy::too_many_arguments)]
fn run_fusion(
    label: &str,
    tile_size: u32,
    levels: u32,
    max_bytes_in_flight: u64,
    reader: &dyn TileReader,
    stack_ctx: StackContext,
    crop: Option<Rect>,
) -> FusionResult {
    let (width, height, spp, frame_count) = (
        stack_ctx.width,
        stack_ctx.height,
        stack_ctx.samples_per_pixel,
        stack_ctx.frame_count,
    );

    let algo = registry::find_stack("laplacian").expect("laplacian must be registered");
    let mut explicit = ParamValues::new();
    explicit.insert("levels".to_string(), ParamValue::Int(levels as i64));
    let resolved =
        params::resolve(algo.params(), &explicit, &stack_ctx).expect("resolve must succeed");
    let values = params::resolved_values(&resolved);

    // Repo-relative `.scratch/fuse/`, resolved from `CARGO_MANIFEST_DIR`: a
    // test binary's cwd is `crates/core`, not the workspace root.
    let scratch_dir = common::workspace_root().join(".scratch").join("fuse");
    std::fs::create_dir_all(&scratch_dir).expect("scratch dir must be creatable");
    let composite_path = scratch_dir.join(format!("{label}.tif"));
    let zmap_path = scratch_dir.join(format!("{label}-zmap.tif"));

    let scheduler_scratch =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("laplacian_fusion_{label}"));
    let mut options = TileSchedulerOptions::explicit_budget(scheduler_scratch, max_bytes_in_flight);
    options.apply_support(algo.tile_support(&values));
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
            crop,
            description: None,
        },
    )
    .expect("consumer must create");
    scheduler
        .run(&NoopPreprocess, &consumer)
        .expect("run must succeed");
    consumer.finish().expect("finish must succeed");

    let out_rect = crop.unwrap_or(Rect {
        x: 0,
        y: 0,
        width,
        height,
    });
    let full_rect = Rect {
        x: 0,
        y: 0,
        width: out_rect.width,
        height: out_rect.height,
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
        width: out_rect.width,
        height: out_rect.height,
        spp,
        frame_count,
        crop,
    }
}

fn open_mantis_512() -> (TiffStack, StackContext) {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let stack_ctx = StackContext::new(stack.len(), width, height, spp);
    (stack, stack_ctx)
}

/// 448/5: the golden geometry (four congruent 256px interiors over the
/// 512px fixture), unaligned, full-frame. Computed once per test binary
/// process and shared, since several tests below assert different things
/// about the same run.
fn golden_result() -> &'static FusionResult {
    static ONCE: OnceLock<FusionResult> = OnceLock::new();
    ONCE.get_or_init(|| {
        let (stack, stack_ctx) = open_mantis_512();
        run_fusion("mantis-512", 448, 5, 1 << 30, &stack, stack_ctx, None)
    })
}

/// 1024/5: one tile covering the whole 512x512 fixture (interior 704) -- the
/// full-image reference every tiled run is compared against.
fn single_tile_result() -> &'static FusionResult {
    static ONCE: OnceLock<FusionResult> = OnceLock::new();
    ONCE.get_or_init(|| {
        let (stack, stack_ctx) = open_mantis_512();
        run_fusion("single-tile", 1024, 5, 1 << 30, &stack, stack_ctx, None)
    })
}

/// 480/5: the ragged variant (interiors of 160, 160 and 192).
fn ragged_result() -> &'static FusionResult {
    static ONCE: OnceLock<FusionResult> = OnceLock::new();
    ONCE.get_or_init(|| {
        let (stack, stack_ctx) = open_mantis_512();
        run_fusion("ragged-480", 480, 5, 1 << 30, &stack, stack_ctx, None)
    })
}

/// `pyramidal-ecc` under its own defaults (`mode=rotation-scale`,
/// `reference-frame=middle`) with `reference-policy=anchor`, and
/// `FailurePolicy::IdentityFallback` (frame 0 diverges against the middle
/// anchor), run once and shared between [`aligned_result`] and the
/// self-consistency test below.
fn alignment() -> &'static StackAlignment {
    static ONCE: OnceLock<StackAlignment> = OnceLock::new();
    ONCE.get_or_init(|| {
        let (stack, stack_ctx) = open_mantis_512();
        let align_algo =
            registry::find_align("pyramidal-ecc").expect("pyramidal-ecc must be registered");
        let resolved = params::resolve(align_algo.params(), &ParamValues::new(), &stack_ctx)
            .expect("resolve must succeed with align defaults");
        let mut align_values = params::resolved_values(&resolved);
        align_values.insert(
            "reference-policy".to_string(),
            ParamValue::Enum("anchor".to_string()),
        );
        align_stack(
            align_algo,
            stack_ctx,
            &stack,
            &align_values,
            FailurePolicy::IdentityFallback,
        )
        .expect("aligning the fixture must not error (frame 0's divergence is reported, not fatal)")
    })
}

/// 448/5, **aligned**: fused through a [`WarpingReader`] built from
/// [`alignment`]'s transforms and declared at the stack's own auto-crop
/// rect.
fn aligned_result() -> &'static FusionResult {
    static ONCE: OnceLock<FusionResult> = OnceLock::new();
    ONCE.get_or_init(|| {
        let (stack, stack_ctx) = open_mantis_512();
        let (width, height) = (stack_ctx.width, stack_ctx.height);
        let alignment = alignment();

        let crop = auto_crop(alignment, (width, height))
            .expect("the fixture's transforms are affine")
            .expect("the fixture's footprints intersect");

        let transforms: Vec<_> = alignment.frames.iter().map(|f| f.transform).collect();
        let reader = WarpingReader::new(
            &stack,
            (width, height),
            stack_ctx.samples_per_pixel,
            transforms,
        );

        run_fusion(
            "aligned-mantis-512",
            448,
            5,
            1 << 30,
            &reader,
            stack_ctx,
            Some(crop),
        )
    })
}

/// Reads one frame's window through any [`TileReader`] -- `&stack` for an
/// unwarped read, [`WarpingReader`] for a warped one -- into a plain
/// buffer the way [`open_frame`] does for a whole file.
fn read_window(reader: &dyn TileReader, frame: usize, rect: Rect, spp: u32) -> Vec<u16> {
    let mut out = vec![0u16; rect.width as usize * rect.height as usize * spp as usize];
    reader
        .read_tile_into(frame, rect, &mut out)
        .expect("read must succeed");
    out
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

#[test]
fn algorithm_is_registered_and_lists_its_schema() {
    let algo = registry::find_stack("laplacian").expect("laplacian must resolve by id");
    assert_eq!(algo.id(), "laplacian");
    assert!(!algo.display_name().is_empty());
    assert!(!algo.params().is_empty());
    assert!(
        algo.params().iter().any(|p| p.id == "levels"),
        "the schema must declare `levels`"
    );
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
    let algo = registry::find_stack("laplacian").expect("laplacian must resolve by id");

    let resolved = params::resolve(algo.params(), &ParamValues::new(), &stack_ctx)
        .expect("resolve must succeed with no explicit overrides");
    let values = params::resolved_values(&resolved);
    assert_eq!(
        values.len(),
        algo.params().len(),
        "the resolved map must carry exactly one value per declared parameter"
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
    algo.fuse(&ctx, &values)
        .expect("fuse must succeed reading exactly the resolved schema values");
}

#[test]
fn metrics_reproduce_the_measured_baselines() {
    let (frame03, w, h, spp) = open_frame(common::mantis_512_dir().join("frames").join("03.tif"));
    let (frame00, ..) = open_frame(common::mantis_512_dir().join("frames").join("00.tif"));
    let (reference, ..) = open_frame(common::mantis_512_dir().join("reference.tif"));

    let lapvar_03 = metrics::laplacian_variance(&frame03, w, h, spp);
    let lapvar_ref = metrics::laplacian_variance(&reference, w, h, spp);
    let ssim_00 = metrics::block_mean_ssim(&frame00, &reference, w, h, spp);
    let psnr_00 = metrics::psnr(&frame00, &reference, w, h, spp);
    let ratio = lapvar_ref / lapvar_03;

    println!(
        "calibration: lapvar(03.tif)={lapvar_03:.1} lapvar(reference)={lapvar_ref:.1} \
         ratio={ratio:.4} ssim(00,reference)={ssim_00:.4} psnr(00,reference)={psnr_00:.4}"
    );

    assert!(
        (lapvar_03 - 285_247.0).abs() <= 1.0,
        "lapvar(03.tif) must reproduce 285,247 (+/- 1): got {lapvar_03}"
    );
    assert!(
        (ratio - 3.24).abs() < 0.005,
        "reference/03.tif lapvar ratio must reproduce 3.24 to two decimals: got {ratio:.4}"
    );
    assert!(
        (ssim_00 - 0.8420).abs() <= 0.0005,
        "block_mean_ssim(00.tif, reference) must reproduce 0.8420 (+/- 0.0005): got {ssim_00:.4}"
    );
    assert!(
        (psnr_00 - 24.70).abs() <= 0.01,
        "psnr(00.tif, reference) must reproduce 24.70 dB (+/- 0.01): got {psnr_00:.4}"
    );
}

#[test]
fn rejects_out_of_range_parameter() {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();
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
    let algo = registry::find_stack("laplacian").expect("laplacian must resolve by id");

    let mut cfg = ParamValues::new();
    cfg.insert("levels".to_string(), ParamValue::Int(999));
    match algo.fuse(&ctx, &cfg) {
        Err(AlgorithmError::Params(_)) => {}
        other => panic!("expected AlgorithmError::Params for an out-of-range value, got {other:?}"),
    }
}

/// Extracts the `rect` sub-window (in absolute coordinates of a
/// `full_width`-wide buffer) out of `samples`, chunky, `spp` samples per
/// pixel -- the windowing helper both quality tests below share, since
/// `crate::fuse::metrics` takes only a whole buffer plus its own
/// dimensions, never a rect.
fn extract_window(samples: &[u16], full_width: u32, rect: Rect, spp: u32) -> Vec<u16> {
    let row_len = rect.width as usize * spp as usize;
    let mut out = vec![0u16; rect.height as usize * row_len];
    for row in 0..rect.height as usize {
        let src_row = rect.y as usize + row;
        let src_start = (src_row * full_width as usize + rect.x as usize) * spp as usize;
        let dst_start = row * row_len;
        out[dst_start..dst_start + row_len]
            .copy_from_slice(&samples[src_start..src_start + row_len]);
    }
    out
}

/// **Recorded only, nothing asserted.** `reference.tif` is another
/// stacker's composite from a different alignment and fusion, so no
/// SSIM/PSNR comparison isolates alignment: giving each composite its own
/// best integer shift (up to +/-8 px, within the crop margin) still leaves the
/// unaligned composite ahead on both metrics, with both optima pinned at
/// the search boundary. Both composites' raw and best-shift numbers print.
#[test]
fn composite_similarity_to_reference_is_recorded() {
    let aligned = aligned_result();
    let unaligned = golden_result();
    let crop = aligned
        .crop
        .expect("aligned_result is declared at a crop rect");

    let (reference_full, rw, rh, rspp) = open_frame(common::mantis_512_dir().join("reference.tif"));
    assert_eq!(
        (rw, rh, rspp),
        (unaligned.width, unaligned.height, unaligned.spp)
    );

    let reference_window = extract_window(&reference_full, rw, crop, rspp);
    let unaligned_window =
        extract_window(&unaligned.composite, unaligned.width, crop, unaligned.spp);

    let ssim_aligned = metrics::block_mean_ssim(
        &aligned.composite,
        &reference_window,
        crop.width,
        crop.height,
        aligned.spp,
    );
    let psnr_aligned = metrics::psnr(
        &aligned.composite,
        &reference_window,
        crop.width,
        crop.height,
        aligned.spp,
    );
    let ssim_unaligned = metrics::block_mean_ssim(
        &unaligned_window,
        &reference_window,
        crop.width,
        crop.height,
        unaligned.spp,
    );
    let psnr_unaligned = metrics::psnr(
        &unaligned_window,
        &reference_window,
        crop.width,
        crop.height,
        unaligned.spp,
    );

    // Symmetric: the same +/-8 px search run once per composite, each
    // against its own best-matching reference window.
    // Capped at the real crop margin so shifted windows stay inside the frame.
    let search = [
        crop.x,
        crop.y,
        rw - crop.x - crop.width,
        rh - crop.y - crop.height,
    ]
    .into_iter()
    .min()
    .expect("four margins")
    .min(8) as i32;
    let best_shift_search = |composite: &[u16], spp: u32, unshifted_ssim: f64| {
        let mut best_shift = (0i32, 0i32);
        let mut best_ssim = unshifted_ssim;
        for dy in -search..=search {
            for dx in -search..=search {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let shifted = Rect {
                    x: (crop.x as i32 + dx) as u32,
                    y: (crop.y as i32 + dy) as u32,
                    width: crop.width,
                    height: crop.height,
                };
                let window = extract_window(&reference_full, rw, shifted, rspp);
                let ssim =
                    metrics::block_mean_ssim(composite, &window, crop.width, crop.height, spp);
                if ssim > best_ssim {
                    best_ssim = ssim;
                    best_shift = (dx, dy);
                }
            }
        }
        let best_rect = Rect {
            x: (crop.x as i32 + best_shift.0) as u32,
            y: (crop.y as i32 + best_shift.1) as u32,
            width: crop.width,
            height: crop.height,
        };
        let best_window = extract_window(&reference_full, rw, best_rect, rspp);
        let psnr_best = metrics::psnr(composite, &best_window, crop.width, crop.height, spp);
        (best_shift, best_ssim, psnr_best)
    };

    let (aligned_best_shift, aligned_best_ssim, aligned_psnr_best) =
        best_shift_search(&aligned.composite, aligned.spp, ssim_aligned);
    let (unaligned_best_shift, unaligned_best_ssim, unaligned_psnr_best) =
        best_shift_search(&unaligned_window, unaligned.spp, ssim_unaligned);

    println!(
        "composite_similarity_to_reference_is_recorded: windowed(no shift) aligned ssim={ssim_aligned:.4} \
         psnr={psnr_aligned:.4} unaligned ssim={ssim_unaligned:.4} psnr={psnr_unaligned:.4} -- \
         aligned best-shift=({},{}) ssim={aligned_best_ssim:.4} psnr={aligned_psnr_best:.4} -- \
         unaligned best-shift=({},{}) ssim={unaligned_best_ssim:.4} psnr={unaligned_psnr_best:.4}",
        aligned_best_shift.0, aligned_best_shift.1, unaligned_best_shift.0, unaligned_best_shift.1
    );
}

/// The aligned composite's sharpness ratio over the sharpest single input
/// frame, over the crop window, **asserted** strictly greater than `1.0`,
/// without an absolute floor: fixture-specific magnitudes are recorded, not
/// asserted.
#[test]
fn composite_sharpness_ratio_is_recorded() {
    let aligned = aligned_result();
    let unaligned = golden_result();
    let crop = aligned
        .crop
        .expect("aligned_result is declared at a crop rect");
    let (frame03, w, _h, spp) = open_frame(common::mantis_512_dir().join("frames").join("03.tif"));
    let frame03_window = extract_window(&frame03, w, crop, spp);

    let lapvar =
        metrics::laplacian_variance(&aligned.composite, crop.width, crop.height, aligned.spp);
    let frame03_lapvar = metrics::laplacian_variance(&frame03_window, crop.width, crop.height, spp);
    let ratio = lapvar / frame03_lapvar;
    println!(
        "composite_sharpness_ratio_is_recorded: lapvar(aligned)={lapvar:.1} lapvar(frame03 window)={frame03_lapvar:.1} ratio={ratio:.4}"
    );
    assert!(
        ratio > 1.0,
        "aligned composite sharpness ratio {ratio} must exceed 1.0 over the crop window (sharper than the sharpest input frame)"
    );

    // The aligned path resamples through bicubic Catmull-Rom while the
    // unaligned path never resamples, which could make the aligned
    // composite the blurrier one. Recorded here, over the same crop window,
    // and it comes out the other way: the aligned composite is sharper, not
    // blurrier, so that is not what makes the unaligned composite closer to
    // `reference.tif`.
    let unaligned_window =
        extract_window(&unaligned.composite, unaligned.width, crop, unaligned.spp);
    let unaligned_lapvar =
        metrics::laplacian_variance(&unaligned_window, crop.width, crop.height, unaligned.spp);
    let unaligned_ratio = unaligned_lapvar / frame03_lapvar;
    println!(
        "composite_sharpness_ratio_is_recorded: lapvar(unaligned)={unaligned_lapvar:.1} \
         ratio(unaligned)={unaligned_ratio:.4} -- aligned is sharper by {:.2}% \
         (refutes the bicubic-resample-blurs hypothesis)",
        (ratio / unaligned_ratio - 1.0) * 100.0
    );
}

/// Self-consistency against the anchor frame, no `reference.tif`. For every
/// non-anchor frame, compares that frame's warped window (through the real
/// [`WarpingReader`] path) against its raw, unwarped window, both measured
/// with [`metrics::block_mean_ssim`] against the anchor's own unwarped
/// window over the crop rect.
///
/// The assertion is directional and structural, never a fitted threshold:
///
/// 1. every frame that received a **non-identity** transform: aligned SSIM
///    strictly exceeds unaligned;
/// 2. every frame that received an **identity** transform (this fixture's
///    `FailurePolicy::IdentityFallback` frames): aligned SSIM equals
///    unaligned SSIM **exactly** -- not approximately, because
///    `WarpingReader`'s identity short-circuit (`crate::align::warp_reader`)
///    delegates straight to the inner reader, so the two windows are
///    bit-identical and the derived SSIM values are bit-identical too. That
///    is what this assertion actually proves: [`WarpingReader`] is applying
///    transforms correctly, not measuring solver noise;
/// 3. no frame degrades.
///
/// "Non-identity" means *received a warp*, not literally
/// `AlignStatus::Converged`: three of this fixture's seven non-anchor
/// frames `HitCap` (warped, but did not converge within the iteration cap)
/// and still receive a real, non-identity transform. Reading it as
/// "`Converged` only" would cover one frame instead of four.
///
/// No magnitude is asserted here: a fitted number would be tuned to this
/// one fixture. Every per-frame delta and the two mean deltas are printed
/// instead.
#[test]
fn per_frame_alignment_agrees_with_the_anchor_more_than_unaligned_frames_do() {
    let alignment = alignment();
    let (stack, stack_ctx) = open_mantis_512();
    let (width, height) = (stack_ctx.width, stack_ctx.height);
    let spp = stack_ctx.samples_per_pixel;
    let crop = auto_crop(alignment, (width, height))
        .expect("the fixture's transforms are affine")
        .expect("the fixture's footprints intersect");

    let transforms: Vec<_> = alignment.frames.iter().map(|f| f.transform).collect();
    let reader = WarpingReader::new(&stack, (width, height), spp, transforms);

    let anchor = alignment.reference;
    let anchor_window = read_window(&stack, anchor, crop, spp);

    let mut all_deltas = Vec::new();
    let mut non_identity_deltas = Vec::new();

    for frame in &alignment.frames {
        if frame.frame == anchor {
            continue;
        }
        let unaligned_window = read_window(&stack, frame.frame, crop, spp);
        let aligned_window = read_window(&reader, frame.frame, crop, spp);

        let ssim_unaligned = metrics::block_mean_ssim(
            &unaligned_window,
            &anchor_window,
            crop.width,
            crop.height,
            spp,
        );
        let ssim_aligned = metrics::block_mean_ssim(
            &aligned_window,
            &anchor_window,
            crop.width,
            crop.height,
            spp,
        );
        let delta = ssim_aligned - ssim_unaligned;
        let is_identity = frame.transform == Transform::identity();

        println!(
            "per_frame_alignment_agrees_with_the_anchor_more_than_unaligned_frames_do: frame {} \
             status={:?} identity={is_identity} unaligned_ssim={ssim_unaligned:.6} \
             aligned_ssim={ssim_aligned:.6} delta={delta:+.6}",
            frame.frame, frame.status
        );

        assert!(
            ssim_aligned >= ssim_unaligned,
            "frame {} must not degrade: aligned ssim {ssim_aligned} < unaligned {ssim_unaligned}",
            frame.frame
        );

        if is_identity {
            assert_eq!(
                aligned_window, unaligned_window,
                "frame {} has an identity transform, so WarpingReader's identity short-circuit \
                 must return bit-identical bytes",
                frame.frame
            );
            assert_eq!(
                ssim_aligned, ssim_unaligned,
                "frame {} has an identity transform: aligned and unaligned SSIM must match exactly",
                frame.frame
            );
        } else {
            assert!(
                ssim_aligned > ssim_unaligned,
                "frame {} received a non-identity transform: aligned ssim {ssim_aligned} must \
                 strictly exceed unaligned {ssim_unaligned}",
                frame.frame
            );
            non_identity_deltas.push(delta);
        }
        all_deltas.push(delta);
    }

    let mean_all = all_deltas.iter().sum::<f64>() / all_deltas.len() as f64;
    let mean_non_identity =
        non_identity_deltas.iter().sum::<f64>() / non_identity_deltas.len() as f64;
    println!(
        "per_frame_alignment_agrees_with_the_anchor_more_than_unaligned_frames_do: mean delta over \
         {} non-anchor frames = {mean_all:+.6}; mean delta over {} non-identity frames = \
         {mean_non_identity:+.6}",
        all_deltas.len(),
        non_identity_deltas.len()
    );
}

#[test]
fn zmap_uses_several_sources() {
    let result = golden_result();
    let distinct: std::collections::HashSet<u16> = result.zmap.iter().copied().collect();
    assert!(
        distinct.len() > 1,
        "the Z-Map must draw on more than one source frame, got {distinct:?}"
    );

    // "Winner varies by region": split the map into quadrants and require
    // that not every quadrant has the same single winner.
    let (w, h) = (result.width, result.height);
    let mut quadrant_winners = std::collections::HashSet::new();
    for (qy, qx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
        let x0 = qx * (w / 2);
        let y0 = qy * (h / 2);
        let mut counts: HashMap<u16, usize> = HashMap::new();
        for y in y0..(y0 + h / 2) {
            for x in x0..(x0 + w / 2) {
                *counts.entry(result.zmap[(y * w + x) as usize]).or_insert(0) += 1;
            }
        }
        let dominant = *counts.iter().max_by_key(|&(_, &c)| c).unwrap().0;
        quadrant_winners.insert(dominant);
    }
    assert!(
        quadrant_winners.len() > 1,
        "the dominant winner must not be identical in every quadrant"
    );
}

#[test]
fn zmap_indices_are_in_range() {
    let result = golden_result();
    for &index in &result.zmap {
        assert_ne!(index, NO_SOURCE, "laplacian must never emit NO_SOURCE");
        assert!(
            (index as usize) < result.frame_count,
            "index {index} is out of range for frame_count {}",
            result.frame_count
        );
    }
}

/// The fraction of pixels whose winner agrees with the majority winner of
/// their 8-neighbourhood.
fn spatial_coherence_fraction(zmap: &[u16], width: u32, height: u32) -> f64 {
    let mut agree = 0usize;
    let total = (width * height) as usize;
    for y in 0..height as i64 {
        for x in 0..width as i64 {
            let mut counts: HashMap<u16, usize> = HashMap::new();
            for dy in -1i64..=1 {
                for dx in -1i64..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || ny < 0 || nx >= width as i64 || ny >= height as i64 {
                        continue;
                    }
                    let idx = ny as u32 * width + nx as u32;
                    *counts.entry(zmap[idx as usize]).or_insert(0) += 1;
                }
            }
            // Keyed by `(count, Reverse(index))` so ties resolve to the lowest index
            // deterministically -- `HashMap` iteration order is not reproducible.
            if let Some((&majority, _)) = counts
                .iter()
                .max_by_key(|&(&index, &c)| (c, Reverse(index)))
            {
                let idx = (y as u32 * width + x as u32) as usize;
                if zmap[idx] == majority {
                    agree += 1;
                }
            }
        }
    }
    agree as f64 / total as f64
}

#[test]
fn zmap_is_spatially_coherent() {
    let result = golden_result();
    let fraction = spatial_coherence_fraction(&result.zmap, result.width, result.height);
    let all8 = all_eight_identical_fraction(&result.zmap, result.width, result.height);
    println!(
        "zmap_is_spatially_coherent: majority-agreement={fraction:.4} all-8-identical={all8:.4}"
    );
    assert!(
        fraction >= 0.60,
        "Z-Map 8-neighbourhood majority agreement must be >= 60%, got {:.2}%",
        fraction * 100.0
    );
}

/// The all-8-neighbours-identical fraction is the measure that actually
/// discriminates a structured Z-Map from noise (`spatial_coherence_fraction`'s
/// majority vote does not). Asserted `>= 55%` against a measured 63.76%
/// under this file's border convention and the 12.5% baseline a random
/// 8-way map produces.
#[test]
fn zmap_neighbourhood_is_uniform_well_above_the_random_baseline() {
    let result = golden_result();
    let fraction = all_eight_identical_fraction(&result.zmap, result.width, result.height);
    println!(
        "zmap_neighbourhood_is_uniform_well_above_the_random_baseline: all-8-identical={:.4} \
         (old golden 0.1273, random baseline 0.125, target >= 0.55)",
        fraction
    );
    assert!(
        fraction >= 0.55,
        "Z-Map all-8-neighbours-identical fraction must be >= 55%, got {:.2}%",
        fraction * 100.0
    );
}

/// The composite and the Z-Map of the tiled (448/5) run equal the full-image
/// (1024/5) run byte for byte: the 160 px margin covers the composite's whole
/// dependency cone (`crate::fuse::laplacian`'s "Tiling").
#[test]
fn tiled_composite_and_zmap_are_exact() {
    let tiled = golden_result();
    let single = single_tile_result();
    assert_eq!(
        tiled.zmap, single.zmap,
        "the Z-Map at tile 448/5 must equal the single-tile (1024/5) Z-Map element for element"
    );
    assert_eq!(
        tiled.composite, single.composite,
        "the composite at tile 448/5 must equal the single-tile (1024/5) composite sample for sample"
    );
}

/// 480/5 plans interiors of 160, 160 and 192 (the 32 px remainder is
/// absorbed): the ragged last tile is exact too.
#[test]
fn ragged_tiling_composite_and_zmap_are_exact() {
    let ragged = ragged_result();
    let single = single_tile_result();
    assert_eq!(
        ragged.zmap, single.zmap,
        "the ragged (480/5) Z-Map must equal the single-tile (1024/5) Z-Map element for element"
    );
    assert_eq!(
        ragged.composite, single.composite,
        "the ragged (480/5) composite must equal the single-tile (1024/5) composite sample for sample"
    );
}

/// Levels 3 and 4 plan their own (40 and 80 px) margins, at tile sizes that
/// leave a ragged last tile of their own (96 and 160 px).
#[test]
fn shallower_pyramids_are_exact_under_tiling() {
    for (levels, tile_size) in [(3u32, 184u32), (4, 340)] {
        let (stack, ctx) = open_mantis_512();
        let tiled = run_fusion(
            &format!("levels-{levels}-tiled"),
            tile_size,
            levels,
            1 << 30,
            &stack,
            ctx,
            None,
        );
        let (stack, ctx) = open_mantis_512();
        let single = run_fusion(
            &format!("levels-{levels}-single"),
            1024,
            levels,
            1 << 30,
            &stack,
            ctx,
            None,
        );
        assert_eq!(tiled.zmap, single.zmap, "levels {levels}: Z-Maps differ");
        assert_eq!(
            tiled.composite, single.composite,
            "levels {levels}: composites differ"
        );
    }
}

#[test]
fn result_is_deterministic() {
    let (stack_a, ctx_a) = open_mantis_512();
    let a = run_fusion("determinism-a", 448, 5, 1, &stack_a, ctx_a, None);
    let (stack_b, ctx_b) = open_mantis_512();
    let b = run_fusion("determinism-b", 448, 5, 1 << 30, &stack_b, ctx_b, None);
    assert_eq!(
        a.composite, b.composite,
        "the composite must not depend on the in-flight tile limit"
    );
    assert_eq!(
        a.zmap, b.zmap,
        "the Z-Map must not depend on the in-flight tile limit"
    );
}

#[test]
fn golden_pin_matches_byte_for_byte() {
    let result = golden_result();
    let golden_dir = common::fixtures_dir().join("mantis-512").join("golden");
    let (golden_composite, gw, gh, gspp) = open_frame(golden_dir.join("laplacian.tif"));
    assert_eq!((gw, gh, gspp), (result.width, result.height, result.spp));
    assert_eq!(
        golden_composite, result.composite,
        "the golden composite must be byte-for-byte identical to a fresh 448/5 run"
    );

    let (golden_zmap, zw, zh, zspp) = open_frame(golden_dir.join("laplacian-zmap.tif"));
    assert_eq!((zw, zh, zspp), (result.width, result.height, 1));
    assert_eq!(
        golden_zmap, result.zmap,
        "the golden Z-Map must be byte-for-byte identical to a fresh 448/5 run"
    );
}
