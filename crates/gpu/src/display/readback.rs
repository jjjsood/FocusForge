// SPDX-License-Identifier: GPL-3.0-or-later

//! The only place a display texture is copied back to the host, used only
//! by the tonemap parity test. `crates/gui` never maps or copies a display
//! texture back.

use super::DisplayTexture;

/// Reads `target`'s full extent back as RGBA8 bytes, row-major. Any width
/// works: rows are padded to `wgpu::COPY_BYTES_PER_ROW_ALIGNMENT` on the
/// copy and the padding is stripped on read.
pub fn read_display_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    target: &DisplayTexture,
) -> Vec<u8> {
    let width = target.width();
    let height = target.height();
    let unpadded_bytes_per_row = width * 4;
    let padded_bytes_per_row =
        unpadded_bytes_per_row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);

    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("focusforge-gpu-display-readback-staging"),
        size: (padded_bytes_per_row * height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("focusforge-gpu-display-readback-encoder"),
    });
    encoder.copy_texture_to_buffer(
        target.texture().as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row),
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

    let padded = slice.get_mapped_range().to_vec();
    staging.unmap();

    let mut out = Vec::with_capacity((unpadded_bytes_per_row * height) as usize);
    for row in 0..height as usize {
        let start = row * padded_bytes_per_row as usize;
        let end = start + unpadded_bytes_per_row as usize;
        out.extend_from_slice(&padded[start..end]);
    }
    out
}
