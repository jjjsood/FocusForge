// SPDX-License-Identifier: GPL-3.0-or-later

//! The upload/readback half of the packed-`u32` storage-buffer path
//! (`pack.rs` is the arithmetic half).
//!
//! **Alignment.** `wgpu::COPY_BYTES_PER_ROW_ALIGNMENT` (256 bytes) governs
//! buffer<->texture copies only; it does not apply here. `map_async` /
//! `get_mapped_range` panic unless the mapped slice starts at an offset
//! aligned to `wgpu::MAP_ALIGNMENT` (8 bytes) and its length is a multiple
//! of 4 bytes. Mapping the whole buffer from offset 0 satisfies the first
//! unconditionally; packing to `u32` words satisfies the second by
//! construction, including the odd-sample-count tail (`pack.rs`: 5 samples
//! -> 3 words -> 12 bytes, a multiple of 4, not of 8, and legal).

use std::sync::atomic::{AtomicU64, Ordering};

use focusforge_core::tile::Tile;

use crate::device::GpuError;
use crate::pack::{pack_samples, packed_len};

/// Incremented once per [`download_buffer`] call -- the counter
/// `warp_to_fusion_never_reads_back` reads through [`readback_count`]
/// to prove a whole warped tile costs exactly 2 readbacks (composite +
/// Z-Map), not one per frame.
static READBACK_COUNT: AtomicU64 = AtomicU64::new(0);

/// The number of [`download_buffer`] calls made so far, process-wide. Not
/// reset between calls: a test that needs a delta reads this before and
/// after the operation it measures.
pub fn readback_count() -> u64 {
    READBACK_COUNT.load(Ordering::SeqCst)
}

/// Creates a `STORAGE | COPY_DST | COPY_SRC` buffer of exactly `byte_len`
/// bytes, labelled `label`, with the same usage flags as [`upload_tile`].
/// Callers that need zeros write them explicitly.
pub fn create_storage_buffer(device: &wgpu::Device, byte_len: u64, label: &str) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: byte_len,
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_DST
            | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    })
}

/// Uploads `data` into a new [`create_storage_buffer`] buffer via
/// `queue.write_buffer`, as explicit `f32::to_le_bytes` (the workspace
/// denies `unsafe_code`). Pyramid levels stay `f32` on both sides: packing
/// them to `u16` would lose the working precision parity rests on.
pub fn upload_f32(device: &wgpu::Device, queue: &wgpu::Queue, data: &[f32]) -> wgpu::Buffer {
    let mut bytes = Vec::with_capacity(data.len() * 4);
    for value in data {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    let buffer =
        create_storage_buffer(device, bytes.len() as u64, "focusforge-gpu-fuse-f32-upload");
    queue.write_buffer(&buffer, 0, &bytes);
    buffer
}

/// The `f32` counterpart of [`download_buffer`]: reads back exactly `len`
/// `f32`s from `src` through a `COPY_DST | MAP_READ` staging buffer,
/// decoded with explicit `f32::from_le_bytes`.
pub fn download_f32(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    src: &wgpu::Buffer,
    len: usize,
) -> Result<Vec<f32>, GpuError> {
    let byte_len = (len * 4) as u64;
    let bytes = download_buffer(device, queue, src, byte_len)?;
    Ok(bytes
        .chunks_exact(4)
        .map(|word| {
            f32::from_le_bytes(
                word.try_into()
                    .expect("chunks_exact(4) always yields 4 bytes"),
            )
        })
        .collect())
}

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
/// `COPY_DST | MAP_READ` staging buffer. `Err(GpuError::Readback)` if
/// `device.poll` fails, the `map_async` callback never fires, or the
/// mapping it delivers is an error.
pub fn download_buffer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    src: &wgpu::Buffer,
    byte_len: u64,
) -> Result<Vec<u8>, GpuError> {
    READBACK_COUNT.fetch_add(1, Ordering::SeqCst);
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
        .map_err(|err| GpuError::Readback {
            detail: format!("device.poll failed while waiting for readback map: {err}"),
        })?;
    rx.recv()
        .map_err(|_| GpuError::Readback {
            detail: "map_async callback never fired".to_string(),
        })?
        .map_err(|err| GpuError::Readback {
            detail: format!("failed to map readback staging buffer: {err}"),
        })?;

    let data = slice.get_mapped_range().to_vec();
    staging.unmap();
    Ok(data)
}
