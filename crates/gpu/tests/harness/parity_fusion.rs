// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-kernel and whole-tile parity between `crates/gpu`'s WGSL fusion
//! kernels and their `crates/core` rayon twins, over `fixtures/mantis-512`,
//! at both the golden geometry (`TilePlan::new(512,512,448,5)`, tile 0 --
//! every read rect there clamps to the frame border, so it is also the
//! overlap-edge case) and a true-interior geometry
//! (`TilePlan::new(512,512,200,3)`, tile 5), both derived from
//! `TilePlan::new` rather than guessed.

use focusforge_core::algorithm::{StackAlgorithm, TileCtx, TileReader};
use focusforge_core::fuse::laplacian::{Laplacian, score_plane, score_radius};
use focusforge_core::fuse::pyramid::{self, Plane};
use focusforge_core::gpu::TileBackend;
use focusforge_core::io::TiffStack;
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::tile::{Rect, Tile};
use focusforge_gpu::fuse::{FusionPipelines, convert, pyramid as gpu_pyramid, select};
use focusforge_gpu::harness::gpu_backend;
use focusforge_gpu::pack::{packed_len, unpack_samples};
use focusforge_gpu::transfer::{create_storage_buffer, download_buffer, download_f32, upload_f32};

use crate::common::{open_stack, worst_delta};

/// `128 * f32::EPSILON`: the near-tie bound -- the separable score is 68
/// `f32` adds per pixel at `radius = 8`, so two independently accumulated
/// pipelines can differ by at most ~68 ulps of the score, rounded up to a
/// power of two.
const NEAR_TIE_REL: f32 = 128.0 * f32::EPSILON;

fn read_tile(stack: &TiffStack, frame: usize, rect: Rect, spp: u32) -> Tile {
    let mut samples = vec![0u16; (rect.width * rect.height * spp) as usize];
    let reader: &dyn TileReader = stack;
    reader
        .read_tile_into(frame, rect, &mut samples)
        .expect("read fixture tile");
    Tile {
        rect,
        samples_per_pixel: spp,
        samples,
    }
}

fn read_all_frames(stack: &TiffStack, rect: Rect, spp: u32) -> Vec<Tile> {
    (0..stack.len())
        .map(|f| read_tile(stack, f, rect, spp))
        .collect()
}

/// Uploads `plane`'s `f32` data through [`upload_f32`], mirroring what
/// [`focusforge_gpu::fuse::convert::plane_from_u16`] would have produced --
/// used by the per-kernel tests below that exercise one kernel in
/// isolation, without also depending on the conversion kernel's own
/// correctness.
fn upload_plane_f32(device: &wgpu::Device, queue: &wgpu::Queue, plane: &Plane) -> wgpu::Buffer {
    upload_f32(device, queue, plane.data())
}

#[test]
fn plane_conversion_matches_cpu() {
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let device = shared.device();
    let queue = shared.queue();
    let pipelines = FusionPipelines::new();

    let stack = open_stack();
    let spp = stack.header(0).samples_per_pixel;
    // 201x137 on frame 03: odd sample count, exercising the packed-`u32`
    // tail on the conversion path too.
    let rect = Rect {
        x: 50,
        y: 50,
        width: 201,
        height: 137,
    };
    let tile = read_tile(&stack, 3, rect, spp);
    let sample_count = tile.samples.len();
    let word_count = (packed_len(sample_count) / 4) as u32;

    let packed = focusforge_gpu::pack::pack_samples(&tile.samples);
    let packed_buffer = create_storage_buffer(device, packed.len() as u64, "test-fuse-packed-in");
    queue.write_buffer(&packed_buffer, 0, &packed);

    let f32_buffer = convert::plane_from_u16(
        device,
        queue,
        &pipelines,
        &packed_buffer,
        word_count,
        sample_count as u32,
    );
    let gpu_f32 = download_f32(device, queue, &f32_buffer, sample_count).expect("download_f32");

    let cpu_plane =
        Plane::from_u16(&tile.samples, rect.width, rect.height, spp).expect("cpu from_u16");
    assert_eq!(
        gpu_f32,
        cpu_plane.data(),
        "plane_from_u16 must be bit-identical to Plane::from_u16"
    );

    let packed_out = convert::plane_to_u16(
        device,
        queue,
        &pipelines,
        &f32_buffer,
        word_count,
        sample_count as u32,
    );
    let out_bytes = download_buffer(device, queue, &packed_out, (word_count as u64) * 4)
        .expect("download_buffer");
    let gpu_u16 = unpack_samples(&out_bytes, sample_count);
    assert_eq!(
        gpu_u16,
        cpu_plane.to_u16(),
        "plane_to_u16 must be bit-identical to Plane::to_u16_into"
    );
}

#[test]
fn gaussian_reduce_matches_cpu() {
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let device = shared.device();
    let queue = shared.queue();
    let pipelines = FusionPipelines::new();

    let stack = open_stack();
    let spp = stack.header(0).samples_per_pixel;

    // An even and an odd source extent, so both the `ceil`-halving and the
    // clamp-to-edge tail are exercised.
    for &(width, height) in &[(200u32, 136u32), (201u32, 137u32)] {
        let rect = Rect {
            x: 50,
            y: 50,
            width,
            height,
        };
        let tile = read_tile(&stack, 3, rect, spp);
        let cpu_plane = Plane::from_u16(&tile.samples, width, height, spp).expect("from_u16");
        let cpu_reduced = pyramid::reduce(&cpu_plane);

        let src_buffer = upload_plane_f32(device, queue, &cpu_plane);
        let (gpu_buffer, out_width, out_height) =
            gpu_pyramid::reduce(device, queue, &pipelines, &src_buffer, width, height, spp);
        assert_eq!(out_width, cpu_reduced.width());
        assert_eq!(out_height, cpu_reduced.height());
        let gpu_data = download_f32(device, queue, &gpu_buffer, cpu_reduced.data().len())
            .expect("download_f32");

        assert_eq!(
            gpu_data,
            cpu_reduced.data(),
            "reduce must be bit-identical to pyramid::reduce at {width}x{height}"
        );
    }
}

#[test]
fn laplacian_expand_matches_cpu() {
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let device = shared.device();
    let queue = shared.queue();
    let pipelines = FusionPipelines::new();

    let stack = open_stack();
    let spp = stack.header(0).samples_per_pixel;

    // Source 101x69 (itself a `ceil`-halved size), target 201x137 -- **not**
    // `2 * src` (202x138), the ambiguity `pyramid::expand`'s doc names.
    let (src_width, src_height) = (101u32, 69u32);
    let (out_width, out_height) = (201u32, 137u32);
    let rect = Rect {
        x: 50,
        y: 50,
        width: src_width,
        height: src_height,
    };
    let tile = read_tile(&stack, 3, rect, spp);
    let cpu_plane = Plane::from_u16(&tile.samples, src_width, src_height, spp).expect("from_u16");
    let cpu_expanded = pyramid::expand(&cpu_plane, out_width, out_height);

    let src_buffer = upload_plane_f32(device, queue, &cpu_plane);
    let gpu_buffer = gpu_pyramid::expand(
        device,
        queue,
        &pipelines,
        &src_buffer,
        src_width,
        src_height,
        spp,
        out_width,
        out_height,
    );
    let gpu_data =
        download_f32(device, queue, &gpu_buffer, cpu_expanded.data().len()).expect("download_f32");

    assert_eq!(
        gpu_data,
        cpu_expanded.data(),
        "expand must be bit-identical to pyramid::expand at explicit non-2x target dims"
    );
}

#[test]
fn score_plane_matches_cpu() {
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let device = shared.device();
    let queue = shared.queue();
    let pipelines = FusionPipelines::new();

    let stack = open_stack();
    let spp = stack.header(0).samples_per_pixel;
    let levels = 5u32;
    let rect = Rect {
        x: 0,
        y: 0,
        width: 352,
        height: 352,
    };
    let tile = read_tile(&stack, 3, rect, spp);
    let pyr = pyramid::build(&tile.samples, rect.width, rect.height, spp, levels).expect("build");
    let level0 = &pyr.levels()[0];
    let radius = score_radius(levels, 0);
    assert_eq!(radius, 8, "score_radius(5, 0) must be 8");

    let cpu_scores = score_plane(level0, radius);

    let level_buffer = upload_f32(device, queue, level0.data());
    let gpu_scores_buffer = select::score(
        device,
        queue,
        &pipelines,
        &level_buffer,
        level0.width(),
        level0.height(),
        level0.samples_per_pixel(),
        radius,
    );
    let gpu_scores =
        download_f32(device, queue, &gpu_scores_buffer, cpu_scores.len()).expect("download_f32");

    assert_eq!(
        gpu_scores, cpu_scores,
        "the two-pass windowed score must be bit-identical to score_plane"
    );
}

/// One whole-tile fusion comparison: the CPU result
/// (`Laplacian::fuse(&TileCtx::new(rect, &tiles), cfg)`) against the GPU
/// result (`GpuBackend::fuse_tile`), both at the same `(rect, levels)`.
struct Comparison {
    cpu_composite: Vec<u16>,
    gpu_composite: Vec<u16>,
    cpu_zmap: Vec<u16>,
    gpu_zmap: Vec<u16>,
    tiles: Vec<Tile>,
    rect: Rect,
    spp: u32,
    levels: u32,
}

fn run_comparison(rect: Rect, levels: u32) -> Comparison {
    let stack = open_stack();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let tiles = read_all_frames(&stack, rect, spp);

    let ctx = TileCtx::new(rect, &tiles);
    let mut cfg = ParamValues::new();
    cfg.insert("levels".to_string(), ParamValue::Int(levels as i64));
    let cpu_out = Laplacian.fuse(&ctx, &cfg).expect("cpu fuse");

    let backend = gpu_backend().expect("gpu backend");
    let gpu_out = backend
        .fuse_tile(&stack, frame_count, rect, spp, levels)
        .expect("gpu fuse_tile");

    Comparison {
        cpu_composite: cpu_out.tile.samples,
        gpu_composite: gpu_out.tile.samples,
        cpu_zmap: cpu_out.zmap,
        gpu_zmap: gpu_out.zmap,
        tiles,
        rect,
        spp,
        levels,
    }
}

/// For a Z-Map pixel where the GPU and CPU disagree: the CPU's own
/// level-0 winning and runner-up scores at that pixel, recomputed from
/// every frame's own pyramid.
fn near_tie_margin(cmp: &Comparison, pixel: usize) -> (f32, f32) {
    let radius = score_radius(cmp.levels, 0);
    let mut scores: Vec<f32> = cmp
        .tiles
        .iter()
        .map(|tile| {
            let pyr = pyramid::build(
                &tile.samples,
                cmp.rect.width,
                cmp.rect.height,
                cmp.spp,
                cmp.levels,
            )
            .expect("build");
            score_plane(&pyr.levels()[0], radius)[pixel]
        })
        .collect();
    scores.sort_by(|a, b| b.partial_cmp(a).expect("scores are always finite"));
    (scores[0], scores[1])
}

fn assert_zmap_exact_or_near_tie(cmp: &Comparison) {
    let mut differing = 0usize;
    for (p, (&g, &c)) in cmp.gpu_zmap.iter().zip(cmp.cpu_zmap.iter()).enumerate() {
        if g == c {
            continue;
        }
        differing += 1;
        let (best, runner_up) = near_tie_margin(cmp, p);
        let bound = NEAR_TIE_REL * best;
        println!(
            "zmap parity: pixel {p} differs (gpu={g}, cpu={c}), cpu margin={} bound={bound}",
            best - runner_up
        );
        assert!(
            best - runner_up <= bound,
            "pixel {p} differs (gpu={g}, cpu={c}) but is not a proven near-tie: \
             best={best} runner_up={runner_up} bound={bound}"
        );
    }
    println!("zmap parity: {differing} differing index(es), all proven near-ties");
}

#[test]
fn composite_matches_cpu_within_one_lsb() {
    let Some(_shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let rect = Rect {
        x: 0,
        y: 0,
        width: 352,
        height: 352,
    };
    let cmp = run_comparison(rect, 5);
    let (worst, worst_index) = worst_delta(&cmp.gpu_composite, &cmp.cpu_composite);
    println!("composite parity: worst |delta|={worst} at sample index {worst_index}");
    assert!(
        worst <= 1,
        "composite parity exceeded 1 LSB: worst={worst} at index {worst_index}"
    );
}

#[test]
fn zmap_matches_cpu_exactly() {
    let Some(_shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let rect = Rect {
        x: 0,
        y: 0,
        width: 352,
        height: 352,
    };
    let cmp = run_comparison(rect, 5);
    assert_zmap_exact_or_near_tie(&cmp);
}

#[test]
fn overlap_edge_tile_matches_cpu() {
    let Some(_shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    // TilePlan::new(512,512,448,5) tile 0: read rect (0,0,352,352) --
    // every read rect at this geometry clamps to the frame border.
    let rect = Rect {
        x: 0,
        y: 0,
        width: 352,
        height: 352,
    };
    let cmp = run_comparison(rect, 5);
    let (worst, worst_index) = worst_delta(&cmp.gpu_composite, &cmp.cpu_composite);
    println!("overlap-edge composite parity: worst |delta|={worst} at sample index {worst_index}");
    assert!(
        worst <= 1,
        "overlap-edge composite parity exceeded 1 LSB: worst={worst} at index {worst_index}"
    );
    assert_zmap_exact_or_near_tie(&cmp);
}

#[test]
fn interior_tile_matches_cpu() {
    let Some(_shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    // TilePlan::new(512,512,200,3) tile 5: read rect (128,128,200,200) --
    // touches no frame border.
    let rect = Rect {
        x: 128,
        y: 128,
        width: 200,
        height: 200,
    };
    let cmp = run_comparison(rect, 3);
    let (worst, worst_index) = worst_delta(&cmp.gpu_composite, &cmp.cpu_composite);
    println!("interior composite parity: worst |delta|={worst} at sample index {worst_index}");
    assert!(
        worst <= 1,
        "interior composite parity exceeded 1 LSB: worst={worst} at index {worst_index}"
    );
    assert_zmap_exact_or_near_tie(&cmp);
}
