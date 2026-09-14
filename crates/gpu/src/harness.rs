// SPDX-License-Identifier: GPL-3.0-or-later

//! A per-sample gain and clamp kernel, deliberately arithmetically obvious
//! so a wrong kernel is never confused with a wrong buffer layout.
//!
//! Every kernel in this crate is loaded from a sibling `.wgsl` file via
//! `include_str!`, never a Rust string literal --
//! [`harness_gain.wgsl`](../src/harness_gain.wgsl) is the example.

use std::sync::OnceLock;

use focusforge_core::algorithm::{AlgorithmError, TileOutput, TileReader};
use focusforge_core::align::warp::WarpedTile;
use focusforge_core::align::{AlignError, Transform};
use focusforge_core::gpu::{BackendInfo, TileBackend};
use focusforge_core::tile::{Rect, Tile};

use crate::device::{GpuError, SharedDevice, shared_device};
use crate::fuse::{self, FusionPipelines};
use crate::pack::{packed_len, unpack_samples};
use crate::pipeline::{
    Pipeline, build_pipeline, run_pass, storage_entry, uniform_entry, upload_params,
};
use crate::transfer::{download_buffer, upload_tile};
use crate::warp::{self, WarpPipelines};

/// [`TileBackend`] for this crate's kernels: `self_check`, `fuse_tile`,
/// `warp_tile` and `warp_fuse_tile` on one type. Each pipeline is built
/// lazily on first use; bind groups are rebuilt per call because each call
/// uploads fresh buffers.
pub struct GpuBackend {
    device: &'static SharedDevice,
    pipeline: OnceLock<Pipeline>,
    fuse_pipelines: FusionPipelines,
    warp_pipelines: WarpPipelines,
}

impl GpuBackend {
    fn new(device: &'static SharedDevice) -> Self {
        GpuBackend {
            device,
            pipeline: OnceLock::new(),
            fuse_pipelines: FusionPipelines::new(),
            warp_pipelines: WarpPipelines::new(),
        }
    }

    fn pipeline(&self) -> &Pipeline {
        self.pipeline.get_or_init(|| {
            build_pipeline(
                self.device.device(),
                include_str!("harness_gain.wgsl"),
                "focusforge-gpu-harness-gain",
                "main",
                &[
                    storage_entry(0, true),
                    storage_entry(1, false),
                    uniform_entry(2),
                ],
            )
        })
    }
}

impl TileBackend for GpuBackend {
    fn info(&self) -> BackendInfo {
        self.device.info().clone()
    }

    fn self_check(
        &self,
        reader: &dyn TileReader,
        frame: usize,
        rect: Rect,
        samples_per_pixel: u32,
        gain: f32,
    ) -> Result<Tile, AlgorithmError> {
        let sample_count = (rect.width * rect.height * samples_per_pixel) as usize;
        let mut samples = vec![0u16; sample_count];
        reader.read_tile_into(frame, rect, &mut samples)?;
        let tile = Tile {
            rect,
            samples_per_pixel,
            samples,
        };

        let device = self.device.device();
        let queue = self.device.queue();

        let word_count = (packed_len(sample_count) / 4) as u32;
        let byte_len = u64::from(word_count) * 4;

        let src_buffer = upload_tile(device, queue, &tile);
        let dst_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("focusforge-gpu-harness-dst"),
            size: byte_len,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

        let mut params = [0u8; 16];
        params[0..4].copy_from_slice(&word_count.to_le_bytes());
        params[4..8].copy_from_slice(&(sample_count as u32).to_le_bytes());
        params[8..12].copy_from_slice(&gain.to_le_bytes());
        let params_buffer = upload_params(device, queue, &params);

        let pipeline = self.pipeline();
        let entries = [
            wgpu::BindGroupEntry {
                binding: 0,
                resource: src_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: dst_buffer.as_entire_binding(),
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
            "focusforge-gpu-harness-pass",
        );

        let bytes = download_buffer(device, queue, &dst_buffer, byte_len);
        let samples = unpack_samples(&bytes, sample_count);

        Ok(Tile {
            rect,
            samples_per_pixel,
            samples,
        })
    }

    fn fuse_tile(
        &self,
        reader: &dyn TileReader,
        frame_count: usize,
        rect: Rect,
        samples_per_pixel: u32,
        levels: u32,
    ) -> Result<TileOutput, AlgorithmError> {
        fuse::fuse_tile_gpu(
            self.device,
            reader,
            &self.fuse_pipelines,
            frame_count,
            rect,
            samples_per_pixel,
            levels,
        )
    }

    fn warp_tile(
        &self,
        reader: &dyn TileReader,
        frame_index: usize,
        frame: (u32, u32),
        samples_per_pixel: u32,
        dst: Rect,
        transform: &Transform,
    ) -> Result<WarpedTile, AlignError> {
        warp::warp_tile_gpu(
            self.device,
            &self.warp_pipelines,
            reader,
            frame_index,
            frame,
            samples_per_pixel,
            dst,
            transform,
        )
    }
}

static GPU_BACKEND: OnceLock<GpuBackend> = OnceLock::new();

/// The process-wide GPU backend, built once atop [`shared_device`] -- no
/// test builds its own.
pub fn gpu_backend() -> Result<&'static GpuBackend, &'static GpuError> {
    let device = shared_device()?;
    Ok(GPU_BACKEND.get_or_init(|| GpuBackend::new(device)))
}
