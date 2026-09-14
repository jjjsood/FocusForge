// SPDX-License-Identifier: GPL-3.0-or-later

//! The required features and limits, named in exactly one place and checked
//! against the adapter *before* the device is requested (`device.rs`), so a
//! missing limit produces a named error instead of a pipeline creation
//! failure deep inside a kernel.

/// This crate's kernels use no optional `wgpu` feature.
pub const REQUIRED_FEATURES: wgpu::Features = wgpu::Features::empty();

/// wgpu's downlevel-compatible baseline limits, deliberately **unraised**
/// (the one call to that constructor lives in this function's body,
/// below).
///
/// Measured against that baseline for the one harness
/// kernel here: it binds 2 storage buffers + 1 uniform buffer (limit
/// `max_storage_buffers_per_shader_stage = 4`), one bind group (limit
/// `max_bind_groups = 4`), a 512x512x3-sample tile packs to 393,216 `u32`
/// words = 1.5 MiB (limits `max_storage_buffer_binding_size = 128 MiB`,
/// `max_buffer_size = 256 MiB`), `@workgroup_size(64)` (limit
/// `max_compute_invocations_per_workgroup = 256`) and at most 6,144
/// workgroups for that tile (limit
/// `max_compute_workgroups_per_dimension = 65,535`). Every headroom above is
/// at least 8x, so nothing here needs raising; a kernel that needs more
/// raises a specific limit from this stated baseline.
pub fn required_limits() -> wgpu::Limits {
    wgpu::Limits::downlevel_defaults()
}

/// One named limit the adapter falls short on: `limit` is the field name
/// `wgpu` reports it under, `required` is what this crate asked for and
/// `available` is what the adapter actually offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitShortfall {
    pub limit: &'static str,
    pub required: u64,
    pub available: u64,
}

/// Every limit in `required` that `available` falls short of, each one
/// named -- never a pipeline-creation failure with no named cause.
///
/// Pure: takes two [`wgpu::Limits`] values, touches no adapter, and is
/// therefore unit-testable (`device::missing_limit_is_named`) without a GPU.
pub fn shortfalls(required: &wgpu::Limits, available: &wgpu::Limits) -> Vec<LimitShortfall> {
    let mut out = Vec::new();
    required.check_limits_with_fail_fn(available, false, |limit, needed, provided| {
        out.push(LimitShortfall {
            limit,
            required: needed,
            available: provided,
        });
    });
    out
}

/// One line per shortfall, each naming its own limit -- what makes a
/// [`super::device::GpuError::LimitsShort`] message name the limit that was
/// short instead of describing the failure only in aggregate.
pub fn describe_shortfalls(shortfalls: &[LimitShortfall]) -> String {
    shortfalls
        .iter()
        .map(|s| {
            format!(
                "{} requires {} but the adapter provides {}",
                s.limit, s.required, s.available
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}
