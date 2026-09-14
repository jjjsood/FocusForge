// SPDX-License-Identifier: GPL-3.0-or-later

//! `u16` <-> `f32` plane conversion: the GPU twins of `Plane::from_u16` and
//! `Plane::to_u16_into`. Both kernels operate one packed word (two `u16`
//! samples) per invocation, so both take `word_count` (the dispatch size)
//! and `sample_count` (the odd-tail guard) as their only shape parameters.

use crate::fuse::{FusionPipelines, run_pass, upload_params};
use crate::transfer::create_storage_buffer;

fn params(word_count: u32, sample_count: u32) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&word_count.to_le_bytes());
    bytes[4..8].copy_from_slice(&sample_count.to_le_bytes());
    bytes
}

/// Unpacks `packed` (a packed-`u32` buffer of `word_count` words) into a
/// freshly allocated `f32` buffer of `sample_count` samples --
/// `Plane::from_u16`'s `u16 as f32`, exactly.
pub fn plane_from_u16(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipelines: &FusionPipelines,
    packed: &wgpu::Buffer,
    word_count: u32,
    sample_count: u32,
) -> wgpu::Buffer {
    let dst = create_storage_buffer(
        device,
        (sample_count as u64) * 4,
        "focusforge-gpu-fuse-plane-from-u16-dst",
    );
    let pipeline = pipelines.plane_from_u16(device);
    let params_buffer = upload_params(device, queue, &params(word_count, sample_count));
    let entries = [
        wgpu::BindGroupEntry {
            binding: 0,
            resource: packed.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 1,
            resource: dst.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 2,
            resource: params_buffer.as_entire_binding(),
        },
    ];
    run_pass(
        device,
        queue,
        pipeline,
        &entries,
        word_count,
        "focusforge-gpu-fuse-plane-from-u16-pass",
    );
    dst
}

/// Packs an `f32` buffer of `sample_count` samples into a freshly allocated
/// packed-`u32` buffer of `word_count` words -- `Plane::to_u16_into`'s
/// `floor(clamp(v, 0.0, 65535.0) + 0.5)`, exactly (never WGSL's `round`).
pub fn plane_to_u16(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipelines: &FusionPipelines,
    src: &wgpu::Buffer,
    word_count: u32,
    sample_count: u32,
) -> wgpu::Buffer {
    let dst = create_storage_buffer(
        device,
        (word_count as u64) * 4,
        "focusforge-gpu-fuse-plane-to-u16-dst",
    );
    let pipeline = pipelines.plane_to_u16(device);
    let params_buffer = upload_params(device, queue, &params(word_count, sample_count));
    let entries = [
        wgpu::BindGroupEntry {
            binding: 0,
            resource: src.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 1,
            resource: dst.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 2,
            resource: params_buffer.as_entire_binding(),
        },
    ];
    run_pass(
        device,
        queue,
        pipeline,
        &entries,
        word_count,
        "focusforge-gpu-fuse-plane-to-u16-pass",
    );
    dst
}
