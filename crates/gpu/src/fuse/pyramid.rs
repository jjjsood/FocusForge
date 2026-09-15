// SPDX-License-Identifier: GPL-3.0-or-later

//! Gaussian reduce, Laplacian expand and the elementwise combine -- the GPU
//! twins of `focusforge_core::fuse::pyramid::{reduce, expand}` and the
//! Laplacian level's `G_k - expand(G_{k+1})` / `A_k + expand(...)` combine.
//! Every dispatch allocates and returns a fresh buffer.

use crate::fuse::{FusionPipelines, Pipeline, run_pass, upload_params};
use crate::transfer::create_storage_buffer;

/// Byte layout shared by `reduce.wgsl` and `expand.wgsl`'s `Params`
/// structs: both declare `(dim0, dim1, spp, out_width, out_height)` in that
/// order (named `width, height, ...` in one file and `src_width,
/// src_height, ...` in the other, but the same five `u32`s at the same
/// offsets).
fn dims_params(dim0: u32, dim1: u32, spp: u32, out_width: u32, out_height: u32) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[0..4].copy_from_slice(&dim0.to_le_bytes());
    bytes[4..8].copy_from_slice(&dim1.to_le_bytes());
    bytes[8..12].copy_from_slice(&spp.to_le_bytes());
    bytes[12..16].copy_from_slice(&out_width.to_le_bytes());
    bytes[16..20].copy_from_slice(&out_height.to_le_bytes());
    bytes
}

#[allow(clippy::too_many_arguments)]
fn dispatch(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &Pipeline,
    src: &wgpu::Buffer,
    dst: &wgpu::Buffer,
    params_bytes: &[u8],
    invocation_count: u32,
    label: &str,
) {
    let params_buffer = upload_params(device, queue, params_bytes);
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
    run_pass(device, queue, pipeline, &entries, invocation_count, label);
}

/// Gaussian-blurs and decimates by two: `(width, height, spp)` ->
/// `(ceil(width/2), ceil(height/2), spp)`. Two dispatches, horizontal then
/// vertical, matching `pyramid::reduce`'s pinned pass order. Returns the
/// new buffer and its `(width, height)`.
pub fn reduce(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipelines: &FusionPipelines,
    src: &wgpu::Buffer,
    width: u32,
    height: u32,
    spp: u32,
) -> (wgpu::Buffer, u32, u32) {
    let out_width = width.div_ceil(2);
    let out_height = height.div_ceil(2);

    let h_dst = create_storage_buffer(
        device,
        (out_width as u64) * (height as u64) * (spp as u64) * 4,
        "focusforge-gpu-fuse-reduce-h",
    );
    dispatch(
        device,
        queue,
        pipelines.reduce_horizontal(device),
        src,
        &h_dst,
        &dims_params(width, height, spp, out_width, out_height),
        height * out_width * spp,
        "focusforge-gpu-fuse-reduce-horizontal-pass",
    );

    let v_dst = create_storage_buffer(
        device,
        (out_width as u64) * (out_height as u64) * (spp as u64) * 4,
        "focusforge-gpu-fuse-reduce-v",
    );
    // `width` here is the already-halved `out_width` the horizontal pass
    // produced; `height` is the original, pre-decimation height horizontal
    // left untouched (reduce.wgsl's module docs).
    dispatch(
        device,
        queue,
        pipelines.reduce_vertical(device),
        &h_dst,
        &v_dst,
        &dims_params(out_width, height, spp, out_width, out_height),
        out_height * out_width * spp,
        "focusforge-gpu-fuse-reduce-vertical-pass",
    );

    (v_dst, out_width, out_height)
}

/// Upsamples to exactly `(out_width, out_height)` -- explicit target
/// dimensions, never inferred. Two dispatches,
/// horizontal to `(out_width, height)` then vertical to `(out_width,
/// out_height)`, matching `pyramid::expand`'s pinned pass order.
#[allow(clippy::too_many_arguments)]
pub fn expand(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipelines: &FusionPipelines,
    src: &wgpu::Buffer,
    width: u32,
    height: u32,
    spp: u32,
    out_width: u32,
    out_height: u32,
) -> wgpu::Buffer {
    let h_dst = create_storage_buffer(
        device,
        (out_width as u64) * (height as u64) * (spp as u64) * 4,
        "focusforge-gpu-fuse-expand-h",
    );
    dispatch(
        device,
        queue,
        pipelines.expand_horizontal(device),
        src,
        &h_dst,
        &dims_params(width, height, spp, out_width, out_height),
        height * out_width * spp,
        "focusforge-gpu-fuse-expand-horizontal-pass",
    );

    let v_dst = create_storage_buffer(
        device,
        (out_width as u64) * (out_height as u64) * (spp as u64) * 4,
        "focusforge-gpu-fuse-expand-v",
    );
    dispatch(
        device,
        queue,
        pipelines.expand_vertical(device),
        &h_dst,
        &v_dst,
        &dims_params(width, height, spp, out_width, out_height),
        out_height * out_width * spp,
        "focusforge-gpu-fuse-expand-vertical-pass",
    );

    v_dst
}

fn combine_params(len: u32) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&len.to_le_bytes());
    bytes
}

fn combine_dispatch(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &Pipeline,
    a: &wgpu::Buffer,
    b: &wgpu::Buffer,
    len: u32,
    label: &str,
) -> wgpu::Buffer {
    let out = create_storage_buffer(device, (len as u64) * 4, "focusforge-gpu-fuse-combine-out");
    let params_buffer = upload_params(device, queue, &combine_params(len));
    let entries = [
        wgpu::BindGroupEntry {
            binding: 0,
            resource: a.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 1,
            resource: b.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 2,
            resource: out.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 3,
            resource: params_buffer.as_entire_binding(),
        },
    ];
    run_pass(device, queue, pipeline, &entries, len, label);
    out
}

/// `a - b`, elementwise, over `len` `f32`s -- `L_k = G_k - expand(G_{k+1})`.
pub fn sub(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipelines: &FusionPipelines,
    a: &wgpu::Buffer,
    b: &wgpu::Buffer,
    len: u32,
) -> wgpu::Buffer {
    combine_dispatch(
        device,
        queue,
        pipelines.combine_sub(device),
        a,
        b,
        len,
        "focusforge-gpu-fuse-combine-sub-pass",
    )
}

/// `a + b`, elementwise, over `len` `f32`s -- `A_k = L_k + expand(A_{k+1})`.
pub fn add(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipelines: &FusionPipelines,
    a: &wgpu::Buffer,
    b: &wgpu::Buffer,
    len: u32,
) -> wgpu::Buffer {
    combine_dispatch(
        device,
        queue,
        pipelines.combine_add(device),
        a,
        b,
        len,
        "focusforge-gpu-fuse-combine-add-pass",
    )
}
