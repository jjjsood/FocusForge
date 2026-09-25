// SPDX-License-Identifier: GPL-3.0-or-later

//! The shared device's own error handlers, and the state they leave behind.
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

/// Set by the device-lost callback and never cleared: a lost device stays
/// lost for the life of the process, and no new device is created.
static DEVICE_LOST: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The label of the last kernel dispatched on this thread, reused in
    /// place so a dispatch costs no allocation.
    static LAST_DISPATCH: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Installs the device's uncaptured-error handler and its device-lost
/// callback. Called once, on the one device this process creates.
pub(crate) fn install_handlers(device: &wgpu::Device) {
    device.on_uncaptured_error(Arc::new(|error: wgpu::Error| {
        tracing::error!(
            kind = error_kind(&error),
            detail = %error,
            "graphics error outside every error scope"
        );
    }));
    device.set_device_lost_callback(|reason, message| {
        DEVICE_LOST.store(true, Ordering::SeqCst);
        tracing::error!(
            reason = ?reason,
            detail = %message,
            "graphics device lost; the tiles still to do run on the host"
        );
    });
}

fn error_kind(error: &wgpu::Error) -> &'static str {
    match error {
        wgpu::Error::OutOfMemory { .. } => "out-of-memory",
        wgpu::Error::Validation { .. } => "validation",
        wgpu::Error::Internal { .. } => "internal",
    }
}

/// `true` once the device-lost callback has fired. wgpu queues that
/// callback and runs it from a later `Device::poll`, so a caller that wants
/// the freshest answer polls first -- [`crate::device::SharedDevice::lost`]
/// does; this bare read is for a caller on a hot path (a redraw) that must
/// not poll on every call and relies on something else keeping the flag
/// fresh.
pub fn device_lost() -> bool {
    DEVICE_LOST.load(Ordering::SeqCst)
}

/// Records `label` as the kernel this thread is dispatching, for a
/// validation error raised while [`with_validation_scope`] is open.
pub(crate) fn record_dispatch(label: &str) {
    LAST_DISPATCH.with_borrow_mut(|last| {
        last.clear();
        last.push_str(label);
    });
}

/// Runs `f` inside a `Validation` error scope, logging a captured error
/// with the kernel this thread last dispatched.
///
/// The `kernel` field names the dispatch in flight, which is the dispatch
/// that raised the error for every validation failure wgpu reports from
/// inside a compute pass. A validation error raised by a plain buffer
/// allocation between two passes is attributed to the pass before it.
pub fn with_validation_scope<T>(device: &wgpu::Device, f: impl FnOnce() -> T) -> T {
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let value = f();
    if let Some(error) = crate::device::block_on(scope.pop()) {
        LAST_DISPATCH.with_borrow(|kernel| {
            tracing::error!(
                kernel = %kernel,
                detail = %error,
                "graphics kernel failed validation"
            );
        });
    }
    value
}
