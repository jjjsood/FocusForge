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
///
/// The fusion kernels are measured against the same baseline, and stay
/// under it too. The widest kernel, `select` (`crates/gpu/src/fuse/select.wgsl`),
/// binds 4 storage buffers + 1 uniform (`max_storage_buffers_per_shader_stage
/// = 4`, exactly at the floor -- the natural 5-buffer selection needed
/// interleaving `best`'s score and winner into one `array<Best>` to fit).
/// Its largest `f32`-plane buffer at the golden 448/5 geometry (a
/// 352x352x3 base plane) is 352*352*3*4 = 1,486,848 bytes, two orders of
/// magnitude under `max_storage_buffer_binding_size`. Every dispatch still
/// uses `@workgroup_size(64)`, and the largest single dispatch
/// (`plane_from_u16`/`plane_to_u16` over that same tile) needs
/// `ceil(352*352*3/2/64) = 2,893` workgroups, still under
/// `max_compute_workgroups_per_dimension`, so no limit here needs raising
/// for them either.
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
