// SPDX-License-Identifier: GPL-3.0-or-later

//! The WGSL twin of [`focusforge_core::retouch::recomposite::recomposite`]:
//! the `select.wgsl` one-dispatch-per-frame pattern, since the natural
//! one-storage-buffer-per-named-frame layout exceeds
//! `max_storage_buffers_per_shader_stage = 4` in `downlevel_defaults()`.

use focusforge_core::algorithm::AlgorithmError;
use focusforge_core::retouch::RetouchError;
use focusforge_core::retouch::recomposite::{Sources, named_frames};
use focusforge_core::tile::Rect;

use crate::device::SharedDevice;
use crate::pack::pack_samples;
use crate::pipeline::{
    Pipeline, build_pipeline, run_pass_2d, storage_entry, uniform_entry, upload_params,
};
use crate::transfer::{create_storage_buffer, download_buffer};

const WGSL_SRC: &str = include_str!("recomposite.wgsl");

/// The kernel's uniform: 16 bytes. Explicit `to_le_bytes`, never `bytemuck`
/// -- the workspace denies `unsafe_code`.
struct Params {
    width: u32,
    height: u32,
    spp: u32,
    frame: u32,
}

impl Params {
    fn to_bytes(&self) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&self.width.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.height.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.spp.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.frame.to_le_bytes());
        bytes
    }
}

fn recomposite_pipeline(device: &wgpu::Device) -> Pipeline {
    build_pipeline(
        device,
        WGSL_SRC,
        "focusforge-gpu-recomposite",
        "main",
        &[
            storage_entry(0, true),
            storage_entry(1, true),
            storage_entry(2, false),
            uniform_entry(3),
        ],
    )
}

/// Runs the recomposite kernel over `region`: one dispatch per frame
/// [`named_frames`] names, each invocation writing its pixel's samples only
/// where the pixel's index equals that dispatch's frame. Refuses a region
/// whose `u32`-per-sample output would exceed
/// `shared.limits().max_storage_buffer_binding_size`. `indices.len()` must
/// equal `region`'s area, or [`RetouchError::TileBufferSize`].
pub fn recomposite(
    shared: &SharedDevice,
    indices: &[u16],
    region: Rect,
    sources: &Sources,
) -> Result<Vec<u16>, RetouchError> {
    let area = region.width as usize * region.height as usize;
    if indices.len() != area {
        return Err(RetouchError::TileBufferSize {
            expected: area,
            actual: indices.len(),
        });
    }
    let spp = sources.samples_per_pixel as usize;
    let out_byte_len = (area * spp * 4) as u64;
    let limit = shared.limits().max_storage_buffer_binding_size;
    if out_byte_len > limit {
        return Err(RetouchError::Source(AlgorithmError::Failed {
            algorithm: "gpu-recomposite".to_string(),
            detail: format!(
                "output buffer of {out_byte_len} bytes exceeds \
                 max_storage_buffer_binding_size={limit}"
            ),
        }));
    }

    let device = shared.device();
    let queue = shared.queue();

    let frames = named_frames(indices, sources.frame_count)?;

    let indices_packed = pack_samples(indices);
    let indices_buffer = create_storage_buffer(
        device,
        indices_packed.len() as u64,
        "focusforge-gpu-recomposite-indices",
    );
    queue.write_buffer(&indices_buffer, 0, &indices_packed);

    let out_buffer = create_storage_buffer(device, out_byte_len, "focusforge-gpu-recomposite-out");
    queue.write_buffer(&out_buffer, 0, &vec![0u8; out_byte_len as usize]);

    if !frames.is_empty() {
        let pipeline = recomposite_pipeline(device);
        let frame_rect = Rect {
            x: region.x + sources.origin.0,
            y: region.y + sources.origin.1,
            width: region.width,
            height: region.height,
        };
        let mut frame_buf = vec![0u16; area * spp];
        for &frame in &frames {
            sources
                .reader
                .read_tile_into(frame as usize, frame_rect, &mut frame_buf)?;
            let src_packed = pack_samples(&frame_buf);
            let src_buffer = create_storage_buffer(
                device,
                src_packed.len() as u64,
                "focusforge-gpu-recomposite-src",
            );
            queue.write_buffer(&src_buffer, 0, &src_packed);

            let params = Params {
                width: region.width,
                height: region.height,
                spp: spp as u32,
                frame: u32::from(frame),
            };
            let params_buffer = upload_params(device, queue, &params.to_bytes());

            run_pass_2d(
                device,
                queue,
                &pipeline,
                &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: indices_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: src_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: out_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: params_buffer.as_entire_binding(),
                    },
                ],
                region.width,
                region.height,
                "focusforge-gpu-recomposite",
            );
        }
    }

    let out_bytes = download_buffer(device, queue, &out_buffer, out_byte_len)
        .map_err(AlgorithmError::from)
        .map_err(RetouchError::Source)?;
    Ok(out_bytes
        .chunks_exact(4)
        .map(|word| {
            u32::from_le_bytes(
                word.try_into()
                    .expect("chunks_exact(4) always yields 4 bytes"),
            ) as u16
        })
        .collect())
}
