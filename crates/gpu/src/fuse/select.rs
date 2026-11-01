// SPDX-License-Identifier: GPL-3.0-or-later

//! The two-pass windowed score, the per-level selection, and the residual
//! accumulation -- the GPU twins of `score_plane` and the selection/residual
//! loop in `Laplacian::fuse`.

use crate::fuse::{FusionPipelines, TileBatch};
use crate::pipeline::Pipeline;

fn score_params(width: u32, height: u32, spp: u32, radius: u32) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&width.to_le_bytes());
    bytes[4..8].copy_from_slice(&height.to_le_bytes());
    bytes[8..12].copy_from_slice(&spp.to_le_bytes());
    bytes[12..16].copy_from_slice(&radius.to_le_bytes());
    bytes
}

/// The two-pass windowed sharpness score of one Laplacian level -- the GPU
/// twin of `score_plane`. Row pass then column pass, the same pinned order
/// `score_plane` uses.
#[allow(clippy::too_many_arguments)]
pub fn score(
    batch: &mut TileBatch,
    pipelines: &FusionPipelines,
    level: &wgpu::Buffer,
    width: u32,
    height: u32,
    spp: u32,
    radius: u32,
) -> wgpu::Buffer {
    let device = batch.device();
    let pixel_count = (width as u64) * (height as u64);
    let row_sums = batch.scratch(pixel_count * 4, "focusforge-gpu-fuse-score-rows");
    {
        let pipeline = pipelines.score_rows(device);
        let params_buffer = batch.params(&score_params(width, height, spp, radius));
        let entries = [
            wgpu::BindGroupEntry {
                binding: 0,
                resource: level.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: row_sums.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: params_buffer.as_entire_binding(),
            },
        ];
        batch.record(
            pipeline,
            &entries,
            pixel_count as u32,
            "focusforge-gpu-fuse-score-rows-pass",
        );
    }

    let scores = batch.scratch(pixel_count * 4, "focusforge-gpu-fuse-score-cols");
    {
        let pipeline = pipelines.score_cols(device);
        let params_buffer = batch.params(&score_params(width, height, spp, radius));
        let entries = [
            wgpu::BindGroupEntry {
                binding: 0,
                resource: row_sums.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: scores.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: params_buffer.as_entire_binding(),
            },
        ];
        batch.record(
            pipeline,
            &entries,
            pixel_count as u32,
            "focusforge-gpu-fuse-score-cols-pass",
        );
    }

    scores
}

fn select_params(width: u32, height: u32, spp: u32, frame: u32) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&width.to_le_bytes());
    bytes[4..8].copy_from_slice(&height.to_le_bytes());
    bytes[8..12].copy_from_slice(&spp.to_le_bytes());
    bytes[12..16].copy_from_slice(&frame.to_le_bytes());
    bytes
}

/// One level's selection for one frame: strictly-greater, whole-pixel,
/// updates `best_level`/`best` in place -- the GPU twin of the CPU
/// selection loop. `best` is `array<Best>` (`struct Best { score:
/// f32, winner: u32 }`, 8-byte stride) -- the declared four-storage-buffer
/// floor, unraised.
#[allow(clippy::too_many_arguments)]
pub fn select(
    batch: &mut TileBatch,
    pipelines: &FusionPipelines,
    current_level: &wgpu::Buffer,
    current_score: &wgpu::Buffer,
    best_level: &wgpu::Buffer,
    best: &wgpu::Buffer,
    width: u32,
    height: u32,
    spp: u32,
    frame: u32,
) {
    let pipeline = pipelines.select_main(batch.device());
    let params_buffer = batch.params_once(&select_params(width, height, spp, frame));
    let entries = [
        wgpu::BindGroupEntry {
            binding: 0,
            resource: current_level.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 1,
            resource: current_score.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 2,
            resource: best_level.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 3,
            resource: best.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 4,
            resource: params_buffer.as_entire_binding(),
        },
    ];
    batch.record(
        pipeline,
        &entries,
        width * height,
        "focusforge-gpu-fuse-select-pass",
    );
}

fn residual_params(len: u32) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&len.to_le_bytes());
    bytes[4..8].copy_from_slice(&1.0f32.to_le_bytes());
    bytes
}

/// `(hi, lo) += value`, elementwise, as an exact two-sum -- the GPU twin of
/// the CPU residual sum. Every frame (including frame 0) runs through this
/// identically; `hi` and `lo` start zeroed, and `hi + lo` is the running sum.
pub fn accumulate(
    batch: &mut TileBatch,
    pipelines: &FusionPipelines,
    hi: &wgpu::Buffer,
    lo: &wgpu::Buffer,
    value: &wgpu::Buffer,
    len: u32,
) {
    let pipeline = pipelines.residual_accumulate(batch.device());
    let params_buffer = batch.params(&residual_params(len));
    let entries = [
        wgpu::BindGroupEntry {
            binding: 0,
            resource: value.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 1,
            resource: hi.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 2,
            resource: params_buffer.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 3,
            resource: lo.as_entire_binding(),
        },
    ];
    batch.record(
        pipeline,
        &entries,
        len,
        "focusforge-gpu-fuse-residual-accumulate-pass",
    );
}

fn eligibility_params(len: u32, spp: u32, fill: f32) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&len.to_le_bytes());
    bytes[4..8].copy_from_slice(&spp.to_le_bytes());
    bytes[8..12].copy_from_slice(&fill.to_le_bytes());
    bytes
}

/// Writes `fill` over every sample of the pixels `mask` (one `u32` per pixel)
/// marks 0, in place -- `-inf` into a score plane or `0.0` into a residual.
/// The GPU twin of skipping an ineligible frame in `Laplacian::fuse`.
#[allow(clippy::too_many_arguments)]
pub fn exclude(
    batch: &mut TileBatch,
    pipelines: &FusionPipelines,
    data: &wgpu::Buffer,
    mask: &wgpu::Buffer,
    len: u32,
    spp: u32,
    fill: f32,
) {
    let device = batch.device();
    run_eligibility(
        batch,
        pipelines.eligibility_exclude(device),
        data,
        mask,
        eligibility_params(len, spp, fill),
        len,
        "focusforge-gpu-fuse-eligibility-exclude-pass",
    );
}

/// `hi = hi / count + lo / count` per pixel, in place, 0 where `count` (one
/// `u32` per pixel) is 0 -- the GPU twin of the residual mean in
/// `Laplacian::fuse`, leaving the mean in `hi`.
pub fn mean_by_count(
    batch: &mut TileBatch,
    pipelines: &FusionPipelines,
    hi: &wgpu::Buffer,
    lo: &wgpu::Buffer,
    count: &wgpu::Buffer,
    len: u32,
    spp: u32,
) {
    let pipeline = pipelines.eligibility_mean_by_count(batch.device());
    let params_buffer = batch.params(&eligibility_params(len, spp, 0.0));
    let entries = [
        wgpu::BindGroupEntry {
            binding: 0,
            resource: count.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 1,
            resource: hi.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 2,
            resource: params_buffer.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 3,
            resource: lo.as_entire_binding(),
        },
    ];
    batch.record(
        pipeline,
        &entries,
        len,
        "focusforge-gpu-fuse-eligibility-mean-by-count-pass",
    );
}

#[allow(clippy::too_many_arguments)]
fn run_eligibility(
    batch: &mut TileBatch,
    pipeline: &Pipeline,
    data: &wgpu::Buffer,
    per_pixel: &wgpu::Buffer,
    params: [u8; 16],
    len: u32,
    label: &str,
) {
    let params_buffer = batch.params(&params);
    let entries = [
        wgpu::BindGroupEntry {
            binding: 0,
            resource: per_pixel.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 1,
            resource: data.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 2,
            resource: params_buffer.as_entire_binding(),
        },
    ];
    batch.record(pipeline, &entries, len, label);
}
