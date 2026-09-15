// SPDX-License-Identifier: GPL-3.0-or-later

//! The real allocate-and-back-off probe over a live `wgpu::Device` --
//! `crate::budget::DeviceAllocProbe`'s only real implementor. Feature-gated
//! (`gpu`), registered in `lib.rs`'s `#[cfg(feature = "gpu")]` block, unlike
//! `crate::budget` itself which stays wgpu-free so its arithmetic is
//! unit-tested without a GPU.
//!
//! Four pinned-version (wgpu 29.0.4) facts this module is written *against*:
//!
//! - `ErrorScopeGuard` is `!Send`/`!Sync` by construction
//!   (`wgpu-29.0.4/src/api/device.rs`, `assert_not_impl_any!`), so push and
//!   pop below are one statement block on one thread -- never handed
//!   across threads.
//! - Error scopes are thread-local: the sink keys scopes by `ThreadId`
//!   (`wgpu-29.0.4/src/backend/wgpu_core.rs`) and routes each error to the
//!   innermost matching scope *of the raising thread*. A scope pushed here
//!   catches only an OOM raised by the `create_buffer` calls this same
//!   function makes.
//! - An **uncaptured** OOM is fatal -- with no matching scope and no
//!   `on_uncaptured_error` handler, wgpu calls `default_error_handler(err)
//!   -> !`, which panics. The push/pop pair below is exactly what turns
//!   that panic into the `Result` this probe returns.
//! - `pop_error_scope` itself panics on an empty or out-of-order stack, so
//!   a pop without its own push is a bug in this code, never a device
//!   condition -- [`WgpuAllocProbe::try_reserve`] always pushes immediately
//!   before it pops. On the wgpu-core backend the returned future is
//!   already `ready(..)`, so `crate::device::block_on` resolves it on the
//!   first poll.

use crate::budget::DeviceAllocProbe;
use crate::device::block_on;

/// Wraps a live `&wgpu::Device`: [`DeviceAllocProbe::try_reserve`] actually
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
}

impl DeviceAllocProbe for WgpuAllocProbe<'_> {
    fn try_reserve(&self, bytes: u64) -> Result<(), String> {
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
        // allocator has actually reclaimed before the next attempt --
        // the same poll call `transfer.rs`'s `download_buffer` already
        // makes.
        drop(buffers);
        let _ = device.poll(wgpu::PollType::wait_indefinitely());

        match error {
            Some(err) => Err(err.to_string()),
            None => Ok(()),
        }
    }
}
