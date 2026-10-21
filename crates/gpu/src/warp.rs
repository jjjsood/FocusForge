// SPDX-License-Identifier: GPL-3.0-or-later

//! The tile warp in WGSL: the GPU twin of
//! `focusforge_core::align::warp::{warp_from_tile, warp_tile}`. The host
//! (`crate::warp_coords::map_coords`) decides coverage and the mapped source
//! coordinate in `f64`, as `crates/core` does, and refuses a too-small
//! source tile before anything here is encoded; this module only uploads,
//! dispatches the bicubic kernel and reads the result back.

use std::sync::OnceLock;

use focusforge_core::algorithm::TileReader;
use focusforge_core::align::warp::WarpedTile;
use focusforge_core::align::{AlignError, SourceFootprint, Transform};
use focusforge_core::tile::{Rect, Tile};
use focusforge_core::timing::{TimingCounter, gpu_tile_stage};

use crate::device::SharedDevice;
use crate::pack::{packed_len, unpack_samples};
use crate::pipeline::{
    Pipeline, build_pipeline, run_pass, storage_entry, uniform_entry, upload_params,
};
use crate::transfer::{create_storage_buffer, download_buffer, upload_storage, upload_tile};
use crate::warp_coords::map_coords;

/// The bicubic warp kernel's pipeline, built on first use: 3 storage + 1
/// uniform binding. At a 512x512x3 destination the coord buffer is
/// 6,291,456 bytes (24-byte `Coord` per pixel), the packed destination
/// 1,572,864 bytes, and the dispatch 6,144 workgroups, all within
/// `crate::limits::required_limits`.
#[derive(Default)]
pub struct WarpPipelines {
    bicubic: OnceLock<Pipeline>,
}

impl WarpPipelines {
    pub fn new() -> Self {
        Self::default()
    }

    fn bicubic(&self, device: &wgpu::Device) -> &Pipeline {
        self.bicubic.get_or_init(|| {
            build_pipeline(
                device,
                include_str!("warp_bicubic.wgsl"),
                "focusforge-gpu-warp-bicubic",
                "main",
                &[
                    storage_entry(0, true),
                    storage_entry(1, true),
                    storage_entry(2, false),
                    uniform_entry(3),
                ],
            )
        })
    }
}

fn params_bytes(
    word_count: u32,
    sample_count: u32,
    src: &Rect,
    spp: u32,
    frame: (u32, u32),
) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[0..4].copy_from_slice(&word_count.to_le_bytes());
    bytes[4..8].copy_from_slice(&sample_count.to_le_bytes());
    bytes[8..12].copy_from_slice(&src.x.to_le_bytes());
    bytes[12..16].copy_from_slice(&src.y.to_le_bytes());
    bytes[16..20].copy_from_slice(&src.width.to_le_bytes());
    bytes[20..24].copy_from_slice(&spp.to_le_bytes());
    bytes[24..28].copy_from_slice(&frame.0.to_le_bytes());
    bytes[28..32].copy_from_slice(&frame.1.to_le_bytes());
    bytes
}

/// Warps `dst` out of `src` (whose own `rect` is in the same absolute
/// `frame`-pixel coordinate space) through `t`, entirely on the GPU -- the
/// twin of `focusforge_core::align::warp::warp_from_tile`. The host-side
/// [`map_coords`] runs first and can return
/// [`AlignError::SourceTileTooSmall`] before anything is uploaded or
/// encoded; `covered` in the returned [`WarpedTile`] is the host's own
/// choice, never a GPU readback.
pub fn warp_from_tile_gpu(
    shared: &SharedDevice,
    pipelines: &WarpPipelines,
    src: &Tile,
    frame: (u32, u32),
    dst: Rect,
    t: &Transform,
) -> Result<WarpedTile, AlignError> {
    let sample_count = dst.width as usize * dst.height as usize * src.samples_per_pixel as usize;
    let (dst_buffer, covered) = warp_dispatch(shared, pipelines, src, frame, dst, t)?;

    let device = shared.device();
    let queue = shared.queue();
    let word_count = (packed_len(sample_count) / 4) as u32;
    let bytes = download_buffer(device, queue, &dst_buffer, u64::from(word_count) * 4)?;
    let samples = unpack_samples(&bytes, sample_count);

    Ok(WarpedTile {
        tile: Tile {
            rect: dst,
            samples_per_pixel: src.samples_per_pixel,
            samples,
        },
        covered,
    })
}

/// The dispatch half of [`warp_from_tile_gpu`], split off so the fused
/// pipeline can keep the destination buffer resident on the GPU across the
/// warp -> fusion boundary instead of reading it back here. Returns the
/// packed-`u32` destination buffer **unread** plus the host's own coverage
/// choice (never re-derived from a GPU readback).
pub(crate) fn warp_dispatch(
    shared: &SharedDevice,
    pipelines: &WarpPipelines,
    src: &Tile,
    frame: (u32, u32),
    dst: Rect,
    t: &Transform,
) -> Result<(wgpu::Buffer, Vec<bool>), AlignError> {
    let _stage = gpu_tile_stage(TimingCounter::GpuTileWarpNs);
    let mapped = map_coords(src.rect, frame, dst, t)?;

    let device = shared.device();
    let queue = shared.queue();
    let spp = src.samples_per_pixel;
    let pixel_count = dst.width as usize * dst.height as usize;
    let sample_count = pixel_count * spp as usize;
    let word_count = (packed_len(sample_count) / 4) as u32;

    let src_buffer = upload_tile(device, queue, src);
    let coords_buffer = upload_storage(device, queue, &mapped.bytes, "focusforge-gpu-warp-coords");
    let dst_buffer =
        create_storage_buffer(device, u64::from(word_count) * 4, "focusforge-gpu-warp-dst");

    let params = params_bytes(word_count, sample_count as u32, &src.rect, spp, frame);
    let params_buffer = upload_params(device, queue, &params);

    let pipeline = pipelines.bicubic(device);
    let entries = [
        wgpu::BindGroupEntry {
            binding: 0,
            resource: src_buffer.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 1,
            resource: coords_buffer.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 2,
            resource: dst_buffer.as_entire_binding(),
        },
        wgpu::BindGroupEntry {
            binding: 3,
            resource: params_buffer.as_entire_binding(),
        },
    ];
    run_pass(
        device,
        queue,
        pipeline,
        &entries,
        word_count,
        "focusforge-gpu-warp-pass",
    );

    Ok((dst_buffer, mapped.covered))
}

/// Warps destination tile `dst` of `frame_index` (a `frame`-sized frame)
/// through `t`, reading only [`Transform::source_footprint`]'s reported
/// rectangle -- the twin of
/// `focusforge_core::align::warp::warp_tile`. On
/// [`SourceFootprint::Outside`], returns an all-uncovered, all-zero
/// [`WarpedTile`] with no read and no dispatch.
#[allow(clippy::too_many_arguments)]
pub fn warp_tile_gpu(
    shared: &SharedDevice,
    pipelines: &WarpPipelines,
    reader: &dyn TileReader,
    frame_index: usize,
    frame: (u32, u32),
    samples_per_pixel: u32,
    dst: Rect,
    t: &Transform,
) -> Result<WarpedTile, AlignError> {
    let sample_count = dst.width as usize * dst.height as usize * samples_per_pixel as usize;
    let (dst_buffer, covered) = warp_tile_dispatch(
        shared,
        pipelines,
        reader,
        frame_index,
        frame,
        samples_per_pixel,
        dst,
        t,
    )?;

    let device = shared.device();
    let queue = shared.queue();
    let word_count = (packed_len(sample_count) / 4) as u32;
    let bytes = download_buffer(device, queue, &dst_buffer, u64::from(word_count) * 4)?;
    let samples = unpack_samples(&bytes, sample_count);

    Ok(WarpedTile {
        tile: Tile {
            rect: dst,
            samples_per_pixel,
            samples,
        },
        covered,
    })
}

/// The dispatch half of [`warp_tile_gpu`], split off for the same reason as
/// [`warp_dispatch`]. On [`SourceFootprint::Outside`], returns
/// a fresh, device-zero-initialised [`create_storage_buffer`] and `covered:
/// vec![false; ..]`, with no read and no dispatch.
#[allow(clippy::too_many_arguments)]
pub(crate) fn warp_tile_dispatch(
    shared: &SharedDevice,
    pipelines: &WarpPipelines,
    reader: &dyn TileReader,
    frame_index: usize,
    frame: (u32, u32),
    samples_per_pixel: u32,
    dst: Rect,
    t: &Transform,
) -> Result<(wgpu::Buffer, Vec<bool>), AlignError> {
    match t.source_footprint(dst, frame)? {
        SourceFootprint::Outside => {
            let pixel_count = dst.width as usize * dst.height as usize;
            let sample_count = pixel_count * samples_per_pixel as usize;
            let word_count = (packed_len(sample_count) / 4) as u64;
            let buffer = create_storage_buffer(
                shared.device(),
                word_count * 4,
                "focusforge-gpu-warp-dst-zeroed",
            );
            Ok((buffer, vec![false; pixel_count]))
        }
        SourceFootprint::Covers { read, .. } => {
            let mut buf =
                vec![0u16; read.width as usize * read.height as usize * samples_per_pixel as usize];
            {
                let _stage = gpu_tile_stage(TimingCounter::GpuTileReadNs);
                reader
                    .read_tile_into(frame_index, read, &mut buf)
                    .map_err(|source| AlignError::Read {
                        frame: frame_index,
                        rect: read,
                        source: Box::new(source),
                    })?;
            }
            let src = Tile {
                rect: read,
                samples_per_pixel,
                samples: buf,
            };
            warp_dispatch(shared, pipelines, &src, frame, dst, t)
        }
    }
}
