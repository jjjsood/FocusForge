// SPDX-License-Identifier: GPL-3.0-or-later

//! Its own binary because it destroys the shared device
//! (`SharedDevice::force_loss`); see `compute_loss.rs`.

#![cfg(feature = "gpu-tests")]

use focusforge_gpu::device::shared_device_or_skip;

/// A loss forced while a submission is still in flight is observable the
/// moment `force_loss` returns: the flag does not wait for a later poll.
#[test]
fn forced_loss_is_observable_with_work_in_flight() {
    const BYTES: u64 = 256 << 20;
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    let device = shared.device();
    let make = || {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("loss-in-flight"),
            size: BYTES,
            usage: wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    };
    let (from, to) = (make(), make());
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_buffer_to_buffer(&from, 0, &to, 0, BYTES);
    shared.queue().submit(Some(encoder.finish()));

    shared.force_loss();

    assert!(shared.lost());
}
