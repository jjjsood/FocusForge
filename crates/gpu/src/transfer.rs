// SPDX-License-Identifier: GPL-3.0-or-later

//! The upload/readback half of the packed-`u32` storage-buffer path
//! (`pack.rs` is the arithmetic half). Feature-gated: this is the only file
//! in the crate that turns packed bytes into a `wgpu::Buffer` and back.
//!
//! **Alignment.** `wgpu::COPY_BYTES_PER_ROW_ALIGNMENT` (256 bytes) governs
//! buffer<->texture copies only; it does not apply here. `map_async` /
//! `get_mapped_range` panic unless the mapped slice starts at an offset
//! aligned to `wgpu::MAP_ALIGNMENT` (8 bytes) and its length is a multiple
//! of 4 bytes. Mapping the whole buffer from offset 0 satisfies the first
//! unconditionally; packing to `u32` words satisfies the second by
//! construction, including the odd-sample-count tail (`pack.rs`: 5 samples
//! -> 3 words -> 12 bytes, a multiple of 4, not of 8, and legal).

use focusforge_core::tile::Tile;

use crate::pack::{pack_samples, packed_len};

/// Uploads `tile`'s packed samples into a new `STORAGE | COPY_DST | COPY_SRC`
/// buffer, via `create_buffer` + `queue.write_buffer` (never
/// `mapped_at_creation`, so the same path works with a tile of any size).
pub fn upload_tile(device: &wgpu::Device, queue: &wgpu::Queue, tile: &Tile) -> wgpu::Buffer {
    let packed = pack_samples(&tile.samples);
    debug_assert_eq!(packed.len(), packed_len(tile.samples.len()));

    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("focusforge-gpu-tile-upload"),
        size: packed.len() as u64,
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_DST
            | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    queue.write_buffer(&buffer, 0, &packed);
    buffer
}

/// Copies `byte_len` bytes of `src` back to the host through a
/// `COPY_DST | MAP_READ` staging buffer.
pub fn download_buffer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    src: &wgpu::Buffer,
    byte_len: u64,
) -> Vec<u8> {
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("focusforge-gpu-readback-staging"),
        size: byte_len,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("focusforge-gpu-readback-encoder"),
    });
    encoder.copy_buffer_to_buffer(src, 0, &staging, 0, byte_len);
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
