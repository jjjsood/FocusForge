// SPDX-License-Identifier: GPL-3.0-or-later
//! All GPU-to-CPU readback for the spike lives here, and nowhere else. `--dump` and
//! `--verify-pattern` are diagnostic; the display path (`main.rs`) never calls into this module.

use slint::wgpu_29::wgpu;

use crate::pattern_cpu::cpu_pattern;

const BYTES_PER_PIXEL: u32 = 4;

/// Copies `texture`'s contents back to a tightly packed `Vec<u8>` (RGBA8, row-major).
/// `width * BYTES_PER_PIXEL` must already be a multiple of `wgpu::COPY_BYTES_PER_ROW_ALIGNMENT`
/// (true for 512 px: 2048 B, alignment 256 B), so no padded staging layout is needed.
pub fn read_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let bytes_per_row = width * BYTES_PER_PIXEL;
    assert_eq!(
        bytes_per_row % wgpu::COPY_BYTES_PER_ROW_ALIGNMENT,
        0,
        "readback assumes an already-aligned row stride"
    );

    let buffer_size = (bytes_per_row * height) as u64;
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("spike-readback-staging"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("spike-readback-encoder"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));

    let slice = staging.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("device.poll failed while waiting for readback map");
    rx.recv()
        .expect("map_async callback never fired")
        .expect("failed to map readback staging buffer");

    let data = slice.get_mapped_range().to_vec();
    staging.unmap();
    data
}

/// Result of comparing a GPU readback against the CPU twin: the worst per-channel |delta|
/// and the pixel/channel it occurred at.
pub struct ParityResult {
    pub worst_delta: u8,
    pub worst_pixel: (u32, u32),
    pub worst_channel: usize,
}

/// Compares the texture's current contents against `cpu_pattern(width, height, t)`,
/// channel-wise. Tolerance is checked by the caller against the returned worst delta.
pub fn verify_pattern(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
    t: f32,
) -> ParityResult {
    let gpu = read_texture(device, queue, texture, width, height);
    let cpu = cpu_pattern(width, height, t);
    assert_eq!(gpu.len(), cpu.len(), "readback and CPU twin sizes disagree");

    let mut worst_delta = 0u8;
    let mut worst_pixel = (0u32, 0u32);
    let mut worst_channel = 0usize;

    for y in 0..height {
        for x in 0..width {
            let idx = ((y * width + x) * 4) as usize;
            for c in 0..4 {
                let delta = gpu[idx + c].abs_diff(cpu[idx + c]);
                if delta > worst_delta {
                    worst_delta = delta;
                    worst_pixel = (x, y);
                    worst_channel = c;
                }
            }
        }
    }

    ParityResult {
        worst_delta,
        worst_pixel,
        worst_channel,
    }
}

/// Writes the texture's current contents to a PNG at `path`. Diagnostic only — this is
/// what `--dump` calls, never the display path.
pub fn dump_png(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
    path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = read_texture(device, queue, texture, width, height);
    let img: image::RgbaImage = image::ImageBuffer::from_raw(width, height, data)
        .ok_or("readback buffer did not match the expected image dimensions")?;
    img.save(path)?;
    Ok(())
}
