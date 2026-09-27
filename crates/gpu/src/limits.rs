// SPDX-License-Identifier: GPL-3.0-or-later

//! The required features and limits, named in exactly one place and checked
//! against the adapter *before* the device is requested (`device.rs`), so a
//! missing limit produces a named error instead of a pipeline creation
//! failure deep inside a kernel.
//!
//! [`LimitShortfall`] and [`describe_shortfalls`] name no `wgpu` type, so
//! `crate::budget` reuses them without the `gpu` feature. `REQUIRED_FEATURES`,
//! `required_limits`, `shortfalls` and `device_limits` are `gpu`-gated, so
//! they are not linked here.
//!
//! **`device_limits` differs from `required_limits`.** `required_limits` is
//! the adapter floor the compute kernels are measured against, checked in
//! `crate::device::init`. `device_limits` is what the device is created
//! with: that floor plus the adapter's texture resolution, because
//! `downlevel_defaults()` caps 2D at 2048 px.

/// This crate's kernels use no optional `wgpu` feature.
#[cfg(feature = "gpu")]
pub const REQUIRED_FEATURES: wgpu::Features = wgpu::Features::empty();

/// wgpu's downlevel-compatible baseline, unraised; every kernel fits it.
/// Harness kernel: 2 storage + 1 uniform (limit 4), 1 bind group (limit 4),
/// `@workgroup_size(64)` (limit 256); a 512x512x3 tile packs to 393,216
/// words = 1.5 MiB (limits 128 MiB binding, 256 MiB buffer) over 6,144
/// workgroups (limit 65,535). No optional `Features` are needed.
#[cfg(feature = "gpu")]
pub fn required_limits() -> wgpu::Limits {
    wgpu::Limits::downlevel_defaults()
}

/// The limits the shared device is created with: [`required_limits`] with
/// only `max_texture_dimension_{1d,2d,3d}` raised to the adapter's, since a
/// maximised image can exceed `downlevel_defaults()`'s 2048 px. Every
/// compute and buffer limit stays at `required_limits()`.
#[cfg(feature = "gpu")]
pub fn device_limits(adapter_limits: &wgpu::Limits) -> wgpu::Limits {
    wgpu::Limits::downlevel_defaults().using_resolution(adapter_limits.clone())
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
#[cfg(feature = "gpu")]
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
/// `GpuError::LimitsShort` (`crate::device`, behind the `gpu` feature, so
/// not linked from this unconditional function) message name the limit that
/// was short instead of describing the failure only in aggregate.
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
