// SPDX-License-Identifier: GPL-3.0-or-later

//! Adapter selection, the single-device invariant, and the limits check --
//! `#![cfg(feature = "gpu-tests")]` lives on the crate-root `tests/harness.rs`.

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
fn a_freshly_created_device_is_not_lost() {
    let Some(device) = shared_device_or_skip() else {
        return;
    };
    assert!(
        !device.lost(),
        "a device nothing has destroyed must not report itself lost"
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

#[test]
fn device_limits_raise_only_resolution() {
    // Pure: a synthetic "adapter-like" limits value, no adapter touched.
    let adapter_like = wgpu::Limits {
        max_texture_dimension_1d: 16_384,
        max_texture_dimension_2d: 16_384,
        max_texture_dimension_3d: 4_096,
        ..wgpu::Limits::default()
    };

    let got = limits::device_limits(&adapter_like);

    let mut expected = limits::required_limits();
    expected.max_texture_dimension_1d = adapter_like.max_texture_dimension_1d;
    expected.max_texture_dimension_2d = adapter_like.max_texture_dimension_2d;
    expected.max_texture_dimension_3d = adapter_like.max_texture_dimension_3d;

    assert_eq!(
        got, expected,
        "device_limits must raise exactly the three texture-dimension fields"
    );
    assert_ne!(
        got.max_texture_dimension_2d,
        limits::required_limits().max_texture_dimension_2d,
        "the adapter's resolution must actually have been applied"
    );
}
