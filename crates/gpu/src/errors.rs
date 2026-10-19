// SPDX-License-Identifier: GPL-3.0-or-later

//! Error handlers for two devices -- the render device a caller hands in,
//! and the headless compute device this crate owns -- and the state each
//! leaves behind.
//!
//! With no matching error scope and no installed handler, wgpu routes an
//! error into `default_error_handler`, which panics. [`install_handlers`]
//! replaces that with a log record, so a diagnosable failure never takes
//! the process down.
//!
//! Error scopes and the dispatch label are both **thread-local**: wgpu keys
//! scopes by `ThreadId` and routes an error to the innermost matching scope
//! of the raising thread, so [`with_validation_scope`] and
//! [`record_dispatch`] have to run on the same thread as the dispatch they
//! describe -- which they do, since a tile's whole GPU pipeline runs on the
//! one worker that owns it.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::device::GpuError;

/// Set by the render device's device-lost callback and never cleared: a lost
/// device stays lost for the life of the process, and no new device is
/// created. The render device is Slint's own -- [`install_handlers`], called
/// once from the GUI's rendering-setup notifier.
static DEVICE_LOST: AtomicBool = AtomicBool::new(false);

/// Set by the headless compute device's device-lost callback -- the second,
/// process-wide device [`crate::device::shared_device`] creates. Its own
/// flag, so a compute loss (finish the remaining tiles on the host) never
/// trips whatever a caller does with [`device_lost`] for the render device.
static COMPUTE_DEVICE_LOST: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The label of the last kernel dispatched on this thread, reused in
    /// place so a dispatch costs no allocation.
    static LAST_DISPATCH: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Installs `device`'s uncaptured-error handler and its device-lost
/// callback, the latter storing into `lost_flag`.
fn install_handlers_on(device: &wgpu::Device, lost_flag: &'static AtomicBool) {
    device.on_uncaptured_error(Arc::new(|error: wgpu::Error| {
        tracing::error!(
            kind = error_kind(&error),
            detail = %error,
            "graphics error outside every error scope"
        );
    }));
    device.set_device_lost_callback(move |reason, message| {
        lost_flag.store(true, Ordering::SeqCst);
        tracing::error!(
            reason = ?reason,
            detail = %message,
            "graphics device lost; the tiles still to do run on the host"
        );
    });
}

/// Installs the render device's error handlers, latching [`DEVICE_LOST`] on
/// loss. Called once, from the GUI's own rendering-setup notifier.
pub fn install_handlers(device: &wgpu::Device) {
    install_handlers_on(device, &DEVICE_LOST);
}

/// Installs the headless compute device's error handlers, latching
/// [`COMPUTE_DEVICE_LOST`] on loss. Called once, from
/// [`crate::device::shared_device`]'s own initialiser.
pub(crate) fn install_compute_handlers(device: &wgpu::Device) {
    install_handlers_on(device, &COMPUTE_DEVICE_LOST);
}

fn error_kind(error: &wgpu::Error) -> &'static str {
    match error {
        wgpu::Error::OutOfMemory { .. } => "out-of-memory",
        wgpu::Error::Validation { .. } => "validation",
        wgpu::Error::Internal { .. } => "internal",
    }
}

/// `true` once the render device's device-lost callback has fired. wgpu
/// queues that callback and runs it from a later `Device::poll`, so a caller
/// that wants the freshest answer polls first; this bare read is for a
/// caller on a hot path (a redraw) that must not poll on every call and
/// relies on something else keeping the flag fresh.
pub fn device_lost() -> bool {
    DEVICE_LOST.load(Ordering::SeqCst)
}

/// `true` once the headless compute device's device-lost callback has fired.
/// [`crate::device::SharedDevice::lost`] polls first for the freshest answer
/// before reading this.
pub(crate) fn compute_device_lost() -> bool {
    COMPUTE_DEVICE_LOST.load(Ordering::SeqCst)
}

/// Records `label` as the kernel this thread is dispatching, for a
/// validation error raised while [`with_validation_scope`] is open.
pub(crate) fn record_dispatch(label: &str) {
    LAST_DISPATCH.with_borrow_mut(|last| {
        last.clear();
        last.push_str(label);
    });
}

/// Runs `f` inside a `Validation` error scope. A captured error is logged and
/// returned as [`GpuError::KernelValidation`]: `f`'s value came from buffers a
/// failed dispatch never wrote, so it is discarded.
///
/// The `kernel` field names the dispatch in flight, which is the dispatch
/// that raised the error for every validation failure wgpu reports from
/// inside a compute pass. A validation error raised by a plain buffer
/// allocation between two passes is attributed to the pass before it.
pub fn with_validation_scope<T>(
    device: &wgpu::Device,
    f: impl FnOnce() -> T,
) -> Result<T, GpuError> {
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let value = f();
    let Some(error) = crate::device::block_on(scope.pop()) else {
        return Ok(value);
    };
    let kernel = LAST_DISPATCH.with_borrow(|kernel| kernel.clone());
    tracing::error!(
        kernel = %kernel,
        detail = %error,
        "graphics kernel failed validation"
    );
    Err(GpuError::KernelValidation {
        kernel,
        detail: error.to_string(),
    })
}
