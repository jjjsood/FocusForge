// SPDX-License-Identifier: GPL-3.0-or-later

//! Its own binary because it destroys the shared device
//! (`SharedDevice::force_loss`): `tests/harness.rs` shares one process-wide
//! device across every module it lists, and a destroyed device there would
//! fail every test after it.

#![cfg(feature = "gpu-tests")]

use focusforge_gpu::device::shared_device_or_skip;
use focusforge_gpu::errors;

#[test]
fn compute_loss_does_not_latch_the_render_flag() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    assert!(!shared.lost());
    assert!(!errors::device_lost());

    shared.force_loss();

    assert!(shared.lost());
    assert!(!errors::device_lost());
}
