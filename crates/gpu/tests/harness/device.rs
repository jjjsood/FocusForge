// SPDX-License-Identifier: GPL-3.0-or-later

//! Adapter selection, the single-device invariant, and the limits check --
//! `#![cfg(feature = "gpu")]` lives on the crate-root `tests/harness.rs`.

use focusforge_gpu::device::{GpuError, adapter_report, shared_device, shared_device_or_skip};
use focusforge_gpu::limits;

#[test]
fn adapter_selection_is_reported() {
    let Some(device) = shared_device_or_skip() else {
        return;
    };

    let report = adapter_report(device.info());
    println!("{report}");

    assert!(report.contains(&device.info().name));
    assert!(report.contains("backend="));
    assert!(report.contains("driver="));
}

#[test]
fn shared_device_is_a_single_process_wide_instance() {
    let Some(first) = shared_device_or_skip() else {
        return;
    };
    let second = shared_device().expect("second call must observe the same cached result");

    assert!(
        std::ptr::eq(first, second),
        "shared_device() must hand out the same instance on every call"
    );
}

#[test]
fn missing_limit_is_named() {
    // Pure: two synthetic `wgpu::Limits` values, no adapter touched.
    let mut required = limits::required_limits();
    required.max_storage_buffers_per_shader_stage = 100;
    let available = limits::required_limits();

    let shorts = limits::shortfalls(&required, &available);
    assert!(
        shorts
            .iter()
            .any(|s| s.limit == "max_storage_buffers_per_shader_stage"),
        "expected a shortfall naming max_storage_buffers_per_shader_stage, got {shorts:?}"
    );

    let err = GpuError::limits_short(&shorts);
    let message = err.to_string();
    assert!(
        message.contains("max_storage_buffers_per_shader_stage"),
        "failure text must name the limit that was short, got: {message}"
    );
}
