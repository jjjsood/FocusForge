// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-kernel and whole-tile parity between `crates/gpu`'s WGSL fusion
//! kernels and their `crates/core` rayon twins, over `fixtures/mantis-512`,
//! at both the golden geometry (`TilePlan::new(512,512,448,5)`, tile 0 --
//! every read rect there clamps to the frame border, so it is also the
//! overlap-edge case) and a true-interior geometry
//! (`TilePlan::new(512,512,200,3)`, tile 5), both derived from
//! `TilePlan::new` rather than guessed.

use focusforge_core::algorithm::{StackAlgorithm, TileCtx, TileOutput, TileReader};
use focusforge_core::fuse::laplacian::{EligibleMasks, Laplacian, score_plane, score_radius};
use focusforge_core::fuse::pyramid::{self, Plane};
use focusforge_core::gpu::TileBackend;
use focusforge_core::io::TiffStack;
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::tile::{Rect, Tile};
use focusforge_core::zmap::NO_SOURCE;
use focusforge_gpu::fuse::{
    FusionPipelines, TileBatch, convert, fuse_tile_gpu, pyramid as gpu_pyramid, select,
};
use focusforge_gpu::harness::gpu_backend;
use focusforge_gpu::pack::{packed_len, unpack_samples};
use focusforge_gpu::transfer::{create_storage_buffer, download_buffer, download_f32, upload_f32};

use crate::common::{NEAR_TIE_REL, assert_matches_cpu_outside_near_ties, open_stack, worst_delta};

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

/// The sample slices of `tiles`, one per frame.
fn tile_samples(tiles: &[Tile]) -> Vec<&[u16]> {
    tiles.iter().map(|t| t.samples.as_slice()).collect()
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
        let mut batch = TileBatch::new(device, queue);
        let (gpu_buffer, out_width, out_height) =
            gpu_pyramid::reduce(&mut batch, &pipelines, &src_buffer, width, height, spp);
        batch.submit();
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
    let mut batch = TileBatch::new(device, queue);
    let gpu_buffer = gpu_pyramid::expand(
        &mut batch,
        &pipelines,
        &src_buffer,
        src_width,
        src_height,
        spp,
        out_width,
        out_height,
    );
    batch.submit();
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
    let mut batch = TileBatch::new(device, queue);
    let gpu_scores_buffer = select::score(
        &mut batch,
        &pipelines,
        &level_buffer,
        level0.width(),
        level0.height(),
        level0.samples_per_pixel(),
        radius,
    );
    batch.submit();
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

/// Whole frames held in memory, each `width` pixels wide at `spp` samples.
struct InMemoryFrames {
    frames: Vec<Vec<u16>>,
    width: u32,
    spp: u32,
}

impl InMemoryFrames {
    /// One `width x height` frame per entry of `sources`, each repeating that
    /// `mantis-512` frame across the canvas.
    fn repeating_mantis(sources: &[usize], width: u32, height: u32) -> Self {
        let stack = open_stack();
        let spp = stack.header(0).samples_per_pixel;
        let source_rect = Rect {
            x: 0,
            y: 0,
            width: 512,
            height: 512,
        };
        let frames = sources
            .iter()
            .map(|&frame| {
                let source = read_tile(&stack, frame, source_rect, spp).samples;
                let mut samples = Vec::with_capacity((width * height * spp) as usize);
                for y in 0..height {
                    for x in 0..width {
                        let at = (((y % 512) * 512 + x % 512) * spp) as usize;
                        samples.extend_from_slice(&source[at..at + spp as usize]);
                    }
                }
                samples
            })
            .collect();
        InMemoryFrames { frames, width, spp }
    }
}

impl TileReader for InMemoryFrames {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), focusforge_core::algorithm::AlgorithmError> {
        let row_len = (rect.width * self.spp) as usize;
        for row in 0..rect.height {
            let from = (((rect.y + row) * self.width + rect.x) * self.spp) as usize;
            let to = row as usize * row_len;
            out[to..to + row_len].copy_from_slice(&self.frames[frame][from..from + row_len]);
        }
        Ok(())
    }
}

/// The first `count` frames of `reader` over `rect`, as CPU tiles.
fn read_in_memory_tiles(reader: &InMemoryFrames, count: usize, rect: Rect, spp: u32) -> Vec<Tile> {
    (0..count)
        .map(|frame| {
            let mut samples = vec![0u16; (rect.width * rect.height * spp) as usize];
            reader
                .read_tile_into(frame, rect, &mut samples)
                .expect("read in-memory tile");
            Tile {
                rect,
                samples_per_pixel: spp,
                samples,
            }
        })
        .collect()
}

/// Every sample of `out` equals its channel's constant.
fn assert_constant_output(out: &TileOutput, triple: [u16; 3], context: &str) {
    for (i, &sample) in out.tile.samples.iter().enumerate() {
        assert_eq!(sample, triple[i % 3], "{context}: sample {i}");
    }
}

const CONSTANT_TRIPLES: [[u16; 3]; 3] = [[40001; 3], [39835, 49858, 60138], [73, 3073, 5001]];

/// 999 frames of one constant triple on a 128x128 canvas.
fn constant_frames(triple: [u16; 3]) -> InMemoryFrames {
    let (width, height, count) = (128u32, 128u32, 999usize);
    let samples: Vec<u16> = triple
        .iter()
        .copied()
        .cycle()
        .take((width * height * 3) as usize)
        .collect();
    InMemoryFrames {
        frames: vec![samples; count],
        width,
        spp: 3,
    }
}

#[test]
fn residual_large_n_constant_stacks_export_the_constant() {
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let backend = gpu_backend().expect("gpu backend");
    let rect = Rect {
        x: 0,
        y: 0,
        width: 128,
        height: 128,
    };
    let (levels, spp) = (5u32, 3u32);
    let pipelines = FusionPipelines::new();
    // Frame 0 is unmasked and every odd frame excludes the left 16 columns, so
    // the eligible count differs between residual pixels.
    let uncovered_left = Some(
        (0..128u32)
            .flat_map(|_| (0..128u32).map(|x| x >= 16))
            .collect::<Vec<bool>>(),
    );
    let coverage: Vec<Option<Vec<bool>>> = (0..999)
        .map(|f| {
            if f % 2 == 1 {
                uncovered_left.clone()
            } else {
                Some(vec![true; 128 * 128])
            }
        })
        .collect();
    for triple in CONSTANT_TRIPLES {
        let reader = constant_frames(triple);
        let unmasked = backend
            .fuse_tile(&reader, 999, rect, spp, levels)
            .expect("gpu fuse_tile");
        assert_constant_output(&unmasked, triple, &format!("{triple:?} unmasked"));
        let masked = fuse_tile_gpu(
            shared, &reader, &pipelines, 999, rect, spp, levels, &coverage,
        )
        .expect("gpu fuse_tile_gpu");
        assert_constant_output(&masked, triple, &format!("{triple:?} masked"));
    }
}

#[test]
fn residual_large_n_matches_cpu() {
    let Some(_shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let backend = gpu_backend().expect("gpu backend");
    let (spp, levels) = (3u32, 5u32);
    let rect = Rect {
        x: 0,
        y: 0,
        width: 128,
        height: 128,
    };
    let sources: Vec<usize> = (0..999).map(|i| i % 8).collect();
    let reader = InMemoryFrames::repeating_mantis(&sources, 128, 128);
    let tiles = read_in_memory_tiles(&reader, sources.len(), rect, spp);
    let mut cfg = ParamValues::new();
    cfg.insert("levels".to_string(), ParamValue::Int(i64::from(levels)));
    let cpu = Laplacian
        .fuse(&TileCtx::new(rect, &tiles), &cfg)
        .expect("cpu fuse");
    let gpu = backend
        .fuse_tile(&reader, sources.len(), rect, spp, levels)
        .expect("gpu fuse_tile");
    assert_matches_cpu_outside_near_ties(
        "large-n",
        &cpu,
        &gpu,
        &tile_samples(&tiles),
        rect,
        spp,
        levels,
    );
}

#[test]
fn tile_at_the_dispatch_limit_matches_cpu() {
    let Some(_shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let backend = gpu_backend().expect("gpu backend");
    let spp = 3;
    let levels = 5;
    // The widest square read rect the device accepts, one row short so the
    // height is odd at the full-resolution level.
    let side = backend.max_tile_size(spp);
    let rect = Rect {
        x: 0,
        y: 0,
        width: side,
        height: side - 1,
    };
    let reader = InMemoryFrames::repeating_mantis(&[0, 2], side, side);
    let tiles = read_in_memory_tiles(&reader, 2, rect, spp);

    let mut cfg = ParamValues::new();
    cfg.insert("levels".to_string(), ParamValue::Int(i64::from(levels)));
    let cpu = Laplacian
        .fuse(&TileCtx::new(rect, &tiles), &cfg)
        .expect("cpu fuse");
    let gpu = backend
        .fuse_tile(&reader, 2, rect, spp, levels)
        .expect("gpu fuse_tile at the dispatch limit");

    assert_matches_cpu_outside_near_ties(
        "at-limit",
        &cpu,
        &gpu,
        &tile_samples(&tiles),
        rect,
        spp,
        levels,
    );
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

/// Frames 0, 2, 4, 5 and 7 of `mantis-512`, cropped to a non-square
/// `352x224` rect, with the coverage cases the eligibility rule has to
/// honour: frame 0 uncovered in a 20-column left band (so it cannot seed the
/// selection), frame 1 uncovered everywhere, frame 2 uncovered in a 20-column
/// right band, and a bottom-right block no frame covers.
fn masked_case() -> (InMemoryFrames, Rect, Vec<Option<Vec<bool>>>) {
    let (width, height) = (352u32, 224u32);
    let reader = InMemoryFrames::repeating_mantis(&[0, 2, 4, 5, 7], width, height);
    let rect = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let mask = |uncovered: &dyn Fn(u32, u32) -> bool| -> Option<Vec<bool>> {
        Some(
            (0..height)
                .flat_map(|y| (0..width).map(move |x| (x, y)))
                .map(|(x, y)| !(uncovered(x, y) || x >= width - 40 && y >= height - 40))
                .collect(),
        )
    };
    let coverage = vec![
        mask(&|x, _| x < 20),
        mask(&|_, _| true),
        mask(&|x, _| x >= width - 20),
        mask(&|_, _| false),
        mask(&|_, _| false),
    ];
    (reader, rect, coverage)
}

#[test]
fn masked_fuse_matches_cpu() {
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let (reader, rect, coverage) = masked_case();
    let (spp, levels, frame_count) = (reader.spp, 5u32, reader.frames.len());
    let tiles = read_in_memory_tiles(&reader, frame_count, rect, spp);
    let mut cfg = ParamValues::new();
    cfg.insert("levels".to_string(), ParamValue::Int(i64::from(levels)));
    let cpu = Laplacian
        .fuse(&TileCtx::new(rect, &tiles).with_coverage(&coverage), &cfg)
        .expect("cpu fuse");
    let gpu = fuse_tile_gpu(
        shared,
        &reader,
        &FusionPipelines::new(),
        frame_count,
        rect,
        spp,
        levels,
        &coverage,
    )
    .expect("gpu fuse_tile_gpu");

    let uncovered = cpu.zmap.iter().filter(|&&z| z == NO_SOURCE).count();
    assert!(
        uncovered > 0,
        "the corner block must leave pixels without a source"
    );

    // A pixel may differ only where the two best frames eligible there score
    // within the near-tie bound.
    let radius = score_radius(levels, 0);
    let eligible: Vec<Vec<bool>> = coverage
        .iter()
        .map(|mask| {
            let covered = mask.clone().expect("every frame carries a mask");
            EligibleMasks::new(&covered, rect.width, rect.height, levels).levels[0].clone()
        })
        .collect();
    let scores: Vec<Vec<f32>> = tiles
        .iter()
        .map(|tile| {
            let pyr =
                pyramid::build(&tile.samples, rect.width, rect.height, spp, levels).expect("build");
            score_plane(&pyr.levels()[0], radius)
        })
        .collect();
    let mut near_ties = std::collections::HashSet::new();
    for (pixel, (&c, &g)) in cpu.zmap.iter().zip(&gpu.zmap).enumerate() {
        if c == g {
            continue;
        }
        let mut at: Vec<f32> = (0..frame_count)
            .filter(|&f| eligible[f][pixel])
            .map(|f| scores[f][pixel])
            .collect();
        at.sort_by(|a, b| b.partial_cmp(a).expect("scores are always finite"));
        assert!(
            at.len() >= 2 && at[0] - at[1] <= NEAR_TIE_REL * at[0],
            "pixel {pixel} differs (gpu={g}, cpu={c}) but is not a proven near-tie among the \
             eligible frames: {at:?}"
        );
        near_ties.insert(pixel);
    }
    println!(
        "masked parity: {uncovered} pixels without a source, {} near-tie index(es)",
        near_ties.len()
    );

    let worst = cpu
        .tile
        .samples
        .chunks_exact(spp as usize)
        .zip(gpu.tile.samples.chunks_exact(spp as usize))
        .enumerate()
        .filter(|(pixel, _)| !near_ties.contains(pixel))
        .flat_map(|(_, (cpu, gpu))| cpu.iter().zip(gpu))
        .map(|(&c, &g)| (i32::from(c) - i32::from(g)).abs())
        .max()
        .expect("a non-empty composite");
    println!("masked composite parity: worst |delta|={worst} outside near-tie pixels");
    assert!(worst <= 1, "composite parity exceeded 1 LSB: worst={worst}");
}

#[test]
fn masked_fuse_with_full_coverage_matches_unmasked() {
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let (reader, rect, _) = masked_case();
    let frame_count = reader.frames.len();
    let pipelines = FusionPipelines::new();
    let full = vec![Some(vec![true; (rect.width * rect.height) as usize]); frame_count];
    let run = |coverage: &[Option<Vec<bool>>]| {
        fuse_tile_gpu(
            shared,
            &reader,
            &pipelines,
            frame_count,
            rect,
            reader.spp,
            5,
            coverage,
        )
        .expect("gpu fuse_tile_gpu")
    };
    let unmasked = run(&[]);
    let masked = run(&full);
    assert_eq!(masked.tile.samples, unmasked.tile.samples);
    assert_eq!(masked.zmap, unmasked.zmap);
}
