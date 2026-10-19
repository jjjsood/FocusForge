// SPDX-License-Identifier: GPL-3.0-or-later

//! The real allocate-and-back-off probe over a live `wgpu::Device`, the
//! reservation `crate::budget::probe_budget` drives. Feature-gated
//! (`gpu`), registered in `lib.rs`'s `#[cfg(feature = "gpu")]` block, unlike
//! `crate::budget` itself which stays wgpu-free so its arithmetic is
//! unit-tested without a GPU.
//!
//! wgpu 29.0.4 facts this relies on: `ErrorScopeGuard` is `!Send`, and
//! scopes are thread-local (keyed by `ThreadId`), so push and pop run on one
//! thread and catch only this function's `create_buffer` OOMs.
//! `pop_error_scope` panics on an empty stack, so each pop pairs with the
//! push made before the allocation loop. The pop future is ready on first
//! poll, so `crate::device::block_on` resolves it immediately.

use crate::device::block_on;

/// Wraps a live `&wgpu::Device`: [`WgpuAllocProbe::try_reserve`] actually
/// allocates `bytes` worth of `COPY_DST`-only buffers (never `STORAGE`, so
/// only `max_buffer_size` applies, not the smaller
/// `max_storage_buffer_binding_size` -- `crate::limits`), chunked at the
/// device's own `max_buffer_size`, inside a
/// `push_error_scope(OutOfMemory)`/`pop` pair.
pub struct WgpuAllocProbe<'a> {
    device: &'a wgpu::Device,
}

impl<'a> WgpuAllocProbe<'a> {
    pub fn new(device: &'a wgpu::Device) -> Self {
        WgpuAllocProbe { device }
    }

    /// Attempts to reserve `bytes` on the device, returning `Err` naming why
    /// on refusal.
    pub fn try_reserve(&self, bytes: u64) -> Result<(), String> {
        let device = self.device;
        let chunk = device.limits().max_buffer_size.max(1);

        // Pushed before any buffer is created, so the scope catches every
        // OOM this loop can raise -- popped once, after the last buffer.
        let scope = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);

        let mut remaining = bytes;
        let mut buffers = Vec::new();
        while remaining > 0 {
            let this_chunk = remaining.min(chunk);
            buffers.push(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("focusforge-gpu-vram-probe"),
                size: this_chunk,
                usage: wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            remaining -= this_chunk;
        }

        let error = block_on(scope.pop());

        // Drop every probe buffer and poll before returning, so the
        // allocator has reclaimed before the next attempt.
        drop(buffers);
        let _ = device.poll(wgpu::PollType::wait_indefinitely());

        match error {
            Some(err) => Err(err.to_string()),
            None => Ok(()),
        }
    }
}
