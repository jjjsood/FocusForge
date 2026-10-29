// SPDX-License-Identifier: GPL-3.0-or-later

//! The VRAM working-set budget: how many tiles `crate::stack_tile`'s
//! pipeline keeps in flight on the device, the device-side twin of the tile
//! scheduler's host-memory budget. Names no `wgpu` type, so it is not
//! feature-gated.
//!
//! **Probe.** wgpu 29.0.4 exposes no adapter memory size
//! (gfx-rs/wgpu#2447), so [`probe_budget`] allocates and backs off:
//! [`PROBE_START_MULTIPLE`] x [`DEFAULT_MAX_DEVICE_BYTES_IN_FLIGHT`], halved
//! per refusal down to [`PROBE_MIN_DEVICE_BYTES`], at most
//! [`PROBE_MAX_ATTEMPTS`] tries. [`buffer_limit_shortfalls`] checks single
//! buffers.
//!
//! **One frame's intermediates plus the tile's fixed buffers.** wgpu does not
//! promise that a dropped `wgpu::Buffer` is reclaimed before a later poll,
//! yet on wgpu 29.0.4 the device peak of a tile measures flat in frame count
//! (8, 32 and 128 frames), so [`device_bytes_per_tile`] takes no frame count
//! and sits at about 1.8 times the measured peak. The flatness test in
//! `tests/harness/budget_gpu.rs` fails if a driver stops reclaiming.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use focusforge_core::params::Provenance;
use focusforge_core::tile::SampleFormat;

use crate::limits::LimitShortfall;
use crate::pack::packed_len;

/// The device-side twin of `scheduler::DEFAULT_MAX_BYTES_IN_FLIGHT`, with the
/// same stated reasoning: wgpu exposes no real VRAM size to derive this from.
pub const DEFAULT_MAX_DEVICE_BYTES_IN_FLIGHT: u64 = 1 << 30;

/// `w*h*spp*4` bytes -- one resident `f32` plane at `spp` samples per pixel.
fn plane(w: u64, h: u64, spp: u64) -> u64 {
    w * h * spp * 4
}

/// `ceil(w*h*spp/2)*4` bytes -- one packed-`u32` buffer of `w*h*spp` `u16`
/// samples, exactly [`crate::pack::packed_len`]'s own byte length.
fn packed(w: u64, h: u64, spp: u64) -> u64 {
    packed_len((w * h * spp) as usize) as u64
}

/// One transferred frame buffer (upload or warp destination) at `format`:
/// packed `u16`s, or a plane of `f32`s.
fn transfer(w: u64, h: u64, spp: u64, format: SampleFormat) -> u64 {
    match format {
        SampleFormat::U16 => packed(w, h, spp),
        SampleFormat::F32 => plane(w, h, spp),
    }
}

/// `(w_k, h_k)` for `k = 0..=levels`: `dims[0]` is the tile's own
/// `(width, height)`, and every following entry is the previous one's
/// `div_ceil(2)` halving -- the same chain `gaussian_levels_gpu` walks via
/// `pyramid::reduce`.
fn level_dims(width: u32, height: u32, levels: u32) -> Vec<(u64, u64)> {
    let mut dims = Vec::with_capacity(levels as usize + 1);
    let mut w = u64::from(width);
    let mut h = u64::from(height);
    dims.push((w, h));
    for _ in 0..levels {
        w = w.div_ceil(2);
        h = h.div_ceil(2);
        dims.push((w, h));
    }
    dims
}

/// Every byte one `warp_fuse_tile_gpu` call allocates on the device for a
/// single frame's contribution: the warp destination / identity upload, the
/// warped-only coord array and re-uploaded source, the `f32` base plane, and
/// every reduce and per-level (expand/laplacian/score) intermediate the
/// pyramid builds while folding this one frame in. On [`SampleFormat::F32`]
/// the upload or warp destination is the base plane itself.
fn per_frame_bytes(
    width: u32,
    height: u32,
    spp: u32,
    levels: u32,
    warped: bool,
    format: SampleFormat,
) -> u64 {
    let dims = level_dims(width, height, levels);
    let spp64 = u64::from(spp);
    let (w0, h0) = dims[0];

    // Warp destination buffer / identity-transform upload, and the source
    // `crate::transfer::upload_tile` re-uploads when warped.
    let mut total = transfer(w0, h0, spp64, format);

    if warped {
        // Coord array, 24 bytes/destination pixel.
        total += 24 * w0 * h0;
        // Re-uploaded source; a model, not a bound, for a large transform
        // whose `Transform::source_footprint` read rect exceeds the
        // destination rect.
        total += transfer(w0, h0, spp64, format);
        // Eligibility mask (one `u32` per pixel) per level and for the residual.
        total += dims.iter().map(|&(w, h)| 4 * w * h).sum::<u64>();
    }

    // The `f32` base plane, via `plane_from_packed`.
    if format == SampleFormat::U16 {
        total += plane(w0, h0, spp64);
    }

    // Reduce: `k = 1..=levels`, `plane(w_k,h_{k-1}) + plane(w_k,h_k)`.
    for k in 1..=(levels as usize) {
        let (wk, hk) = dims[k];
        let (_, hk_minus_1) = dims[k - 1];
        total += plane(wk, hk_minus_1, spp64) + plane(wk, hk, spp64);
    }

    // Per level `k = 0..levels`: expand pair, Laplacian, two score buffers.
    for k in 0..(levels as usize) {
        let (wk, hk) = dims[k];
        let (_, hk_plus_1) = dims[k + 1];
        // Expand pair.
        total += plane(wk, hk_plus_1, spp64) + plane(wk, hk, spp64);
        // Laplacian.
        total += plane(wk, hk, spp64);
        // Score rows + cols.
        total += 2 * 4 * wk * hk;
    }

    total
}

/// Every byte one `warp_fuse_tile_gpu` call allocates on the device exactly
/// once per tile, independent of frame count: the running best level and its
/// interleaved `(score, winner)` plane per level, the residual sum pair and
/// its eligible-frame count, the collapse's expand-and-add per level, the packed
/// output, and the tile's two readback staging buffers.
fn once_per_tile_bytes(width: u32, height: u32, spp: u32, levels: u32) -> u64 {
    let dims = level_dims(width, height, levels);
    let spp64 = u64::from(spp);
    let (w0, h0) = dims[0];
    let mut total = 0u64;

    // Running best level + interleaved (score, winner) plane, per level.
    for &(wk, hk) in dims.iter().take(levels as usize) {
        total += plane(wk, hk, spp64) + 8 * wk * hk;
    }

    // Residual sum and its compensation term, at the coarsest level.
    let (wl, hl) = dims[levels as usize];
    total += 2 * plane(wl, hl, spp64);
    // Eligible-frame count per residual pixel.
    total += 4 * wl * hl;

    // Collapse: expand pair + add, per level.
    for k in 0..(levels as usize) {
        let (wk, hk) = dims[k];
        let (_, hk_plus_1) = dims[k + 1];
        total += plane(wk, hk_plus_1, spp64) + plane(wk, hk, spp64);
        total += plane(wk, hk, spp64);
    }

    // Packed output.
    total += packed(w0, h0, spp64);

    // Two readback staging buffers.
    total += packed(w0, h0, spp64);
    total += 8 * w0 * h0;

    total
}

/// The device bytes one `crate::stack_tile::warp_fuse_tile_gpu` call holds
/// at its peak for a `width x height` rect at `spp` samples per pixel and
/// `levels` pyramid depth, independent of frame count: one frame's
/// intermediates plus the tile's fixed buffers. A 512x512, 3 spp, 5-level
/// warped tile peaks at 29.0 MB over the allocator baseline; the model says
/// 53.1 MB. `warped` is one flag for the whole tile, in the conservative direction.
///
/// Two exclusions: the 16-32 B uniform params buffers
/// (`crate::pipeline::upload_params`; a tile shares one per distinct content,
/// so a frame adds only its select uniform per level) are under 0.1% of the
/// sum, and the warped-source term is a model, not a bound,
/// for the reason named there. Every other term dominates both by orders of
/// magnitude.
pub fn device_bytes_per_tile(
    width: u32,
    height: u32,
    spp: u32,
    levels: u32,
    warped: bool,
    format: SampleFormat,
) -> u64 {
    let per_frame = per_frame_bytes(width, height, spp, levels, warped, format);
    let once = once_per_tile_bytes(width, height, spp, levels);
    per_frame + once
}

/// The account of a `width x height` tile whose `bytes` exceed the device
/// `budget` and so runs on the host; `None` when it fits.
pub fn tile_shortfall_message(width: u32, height: u32, bytes: u64, budget: u64) -> Option<String> {
    (bytes > budget).then(|| {
        format!(
            "one {width}x{height} tile needs {bytes} B on the device, above the device budget of \
             {budget} B; continuing on the CPU path"
        )
    })
}

/// The single largest buffer one `warp_fuse_tile_gpu` call allocates, at any
/// realistic geometry: the warped coord array (24 bytes/pixel) when
/// `warped`, otherwise the `f32` base plane. Sound to check against both
/// hard limits with one number because `max_buffer_size` is never below
/// `max_storage_buffer_binding_size` in `downlevel_defaults()`.
pub fn largest_buffer_bytes(width: u32, height: u32, spp: u32, warped: bool) -> u64 {
    let w0 = u64::from(width);
    let h0 = u64::from(height);
    if warped {
        24 * w0 * h0
    } else {
        plane(w0, h0, u64::from(spp))
    }
}

/// The tile scheduler's `in_flight_for_budget`, by shape: floored at 1 so a
/// budget smaller than one tile still progresses, and capped at `cap`
/// (`available_parallelism()`, as the scheduler passes it; floored at 1).
pub fn tiles_in_flight_for_budget(bytes_per_tile: u64, max_device_bytes: u64, cap: usize) -> usize {
    let cap = cap.max(1);
    if bytes_per_tile == 0 {
        return cap;
    }
    let raw = (max_device_bytes / bytes_per_tile).max(1);
    raw.min(cap as u64) as usize
}

/// Every hard device limit `largest` (one buffer's byte size) exceeds.
/// Plain `u64` limits in (both wgpu fields are `u64` in 29.0.4), so it is
/// unit-testable without a GPU.
pub fn buffer_limit_shortfalls(
    largest: u64,
    max_storage_buffer_binding_size: u64,
    max_buffer_size: u64,
) -> Vec<LimitShortfall> {
    let mut out = Vec::new();
    if largest > max_storage_buffer_binding_size {
        out.push(LimitShortfall {
            limit: "max_storage_buffer_binding_size",
            required: largest,
            available: max_storage_buffer_binding_size,
        });
    }
    if largest > max_buffer_size {
        out.push(LimitShortfall {
            limit: "max_buffer_size",
            required: largest,
            available: max_buffer_size,
        });
    }
    out
}

/// Invocations per workgroup in every 1-D kernel (`@workgroup_size(64)`).
const INVOCATIONS_PER_WORKGROUP: u64 = 64;

/// The `max_compute_workgroups_per_dimension` shortfall of a 1-D dispatch of
/// `invocations` invocations, or `None` when it fits: `crate::pipeline::run_pass`
/// dispatches `ceil(invocations / 64)` workgroups on x alone.
pub fn dispatch_limit_shortfall(
    invocations: u64,
    max_workgroups_per_dimension: u32,
) -> Option<LimitShortfall> {
    let required = invocations.div_ceil(INVOCATIONS_PER_WORKGROUP);
    (required > u64::from(max_workgroups_per_dimension)).then_some(LimitShortfall {
        limit: "max_compute_workgroups_per_dimension",
        required,
        available: u64::from(max_workgroups_per_dimension),
    })
}

/// The largest square read-rect side that both [`dispatch_limit_shortfall`]
/// (one invocation per sample, `side * side * samples_per_pixel`) and
/// [`buffer_limit_shortfalls`] (over [`largest_buffer_bytes`] for a warped
/// tile) accept under the given limits.
pub fn max_tile_side(
    samples_per_pixel: u32,
    max_workgroups_per_dimension: u32,
    max_storage_buffer_binding_size: u64,
    max_buffer_size: u64,
) -> u32 {
    let spp = samples_per_pixel.max(1);
    let by_dispatch =
        u64::from(max_workgroups_per_dimension) * INVOCATIONS_PER_WORKGROUP / u64::from(spp);
    let bytes_per_pixel = largest_buffer_bytes(1, 1, spp, true);
    let by_buffer = max_storage_buffer_binding_size.min(max_buffer_size) / bytes_per_pixel;
    u32::try_from(by_dispatch.min(by_buffer).isqrt()).unwrap_or(u32::MAX)
}

/// The device-side VRAM working-set budget: a byte count and where it came
/// from. Mirrors `HostBudget`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceBudget {
    bytes: u64,
    provenance: Provenance,
}

impl DeviceBudget {
    /// A budget [`probe_budget`] actually settled on.
    pub fn probed(bytes: u64) -> Self {
        DeviceBudget {
            bytes,
            provenance: Provenance::Host,
        }
    }

    /// A caller-supplied budget: never probes -- the constructor
    /// `GpuBackend::with_max_device_bytes` uses for a test that needs an
    /// explicit ceiling without touching the real device, mirroring
    /// `HostBudget::explicit`.
    pub fn explicit(bytes: u64) -> Self {
        DeviceBudget {
            bytes,
            provenance: Provenance::Explicit,
        }
    }

    /// Applies a ceiling: lowers to `ceiling` (reported `Explicit`) only when
    /// it is below the current value; otherwise changes nothing, provenance
    /// included. Same rule as `HostBudget::capped_to`.
    pub fn capped_to(self, ceiling: Option<u64>) -> Self {
        match ceiling {
            Some(ceiling) if ceiling < self.bytes => DeviceBudget {
                bytes: ceiling,
                provenance: Provenance::Explicit,
            },
            _ => self,
        }
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn provenance(&self) -> Provenance {
        self.provenance
    }
}

/// The optimistic starting multiple of [`DEFAULT_MAX_DEVICE_BYTES_IN_FLIGHT`]
/// [`probe_budget`] first asks for -- bounded, not unbounded, because a
/// Vulkan adapter without `VK_EXT_memory_budget` never refuses at all, so an
/// unbounded start would settle at "as much as
/// possible" instead of a stated ceiling. The in-flight *tile* count stays
/// capped independently by `VramBudget`'s `cap` field.
pub const PROBE_START_MULTIPLE: u64 = 8;

/// The floor the probe backs off to and stops: one `max_buffer_size` chunk
/// at `downlevel_defaults()` (`crate::limits::required_limits`,
/// `wgpu::Limits::downlevel_defaults().max_buffer_size == 256 MiB`).
pub const PROBE_MIN_DEVICE_BYTES: u64 = 256 << 20;

/// The number of attempts [`probe_budget`] makes before giving up -- exactly
/// the number of halvings from `PROBE_START_MULTIPLE *
/// DEFAULT_MAX_DEVICE_BYTES_IN_FLIGHT` (8 GiB) down to
/// [`PROBE_MIN_DEVICE_BYTES`] (256 MiB), inclusive of the first,
/// unhalved attempt: `8 GiB / 256 MiB == 32 == 2^5`, five halvings plus the
/// initial request is six attempts. Derived from the other two constants
/// rather than picked independently.
pub const PROBE_MAX_ATTEMPTS: u32 = 6;

/// What [`probe_budget`] settled on: a byte count and how many attempts it
/// took, or -- refused all the way down to [`PROBE_MIN_DEVICE_BYTES`] -- the
/// floor and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    Settled { bytes: u64, attempts: u32 },
    Exhausted { floor: u64, detail: String },
}

/// Allocate-and-back-off: starts at `PROBE_START_MULTIPLE *
/// DEFAULT_MAX_DEVICE_BYTES_IN_FLIGHT`, halving on each refusal down to
/// `PROBE_MIN_DEVICE_BYTES`, for at most `PROBE_MAX_ATTEMPTS` attempts. The
/// request never grows after a refusal, so it cannot oscillate.
/// `try_reserve` attempts to reserve that many bytes on the device and
/// returns `Err` naming why on refusal.
pub fn probe_budget(mut try_reserve: impl FnMut(u64) -> Result<(), String>) -> ProbeOutcome {
    let mut request = PROBE_START_MULTIPLE * DEFAULT_MAX_DEVICE_BYTES_IN_FLIGHT;
    let mut last_detail = String::new();
    for attempt in 1..=PROBE_MAX_ATTEMPTS {
        match try_reserve(request) {
            Ok(()) => {
                return ProbeOutcome::Settled {
                    bytes: request,
                    attempts: attempt,
                };
            }
            Err(detail) => {
                last_detail = detail;
                if request <= PROBE_MIN_DEVICE_BYTES {
                    break;
                }
                request = (request / 2).max(PROBE_MIN_DEVICE_BYTES);
            }
        }
    }
    ProbeOutcome::Exhausted {
        floor: PROBE_MIN_DEVICE_BYTES,
        detail: last_detail,
    }
}

/// An RAII permit from [`VramBudget::acquire`]: dropping it always
/// decrements the in-flight count and wakes every waiter, so an early `?`
/// return from inside the wrapped call releases the permit exactly as
/// reliably as falling off the end of it.
pub struct Permit<'a> {
    budget: &'a VramBudget,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.budget.release();
    }
}

/// A [`Permit`] that owns its budget, for a holder that outlives the call
/// that admitted it (an ECC session). Same release-on-drop guarantee.
pub struct OwnedPermit {
    budget: Arc<VramBudget>,
}

impl Drop for OwnedPermit {
    fn drop(&mut self) {
        self.budget.release();
    }
}

/// A counting gate bounding how many tiles' device allocations are live at
/// once, mirroring the tile scheduler's host-memory budget one crate over.
/// Blocking a rayon worker inside [`Self::acquire`] cannot deadlock here:
/// the permit is released by an RAII guard on the same thread, nothing
/// nested inside the permit region re-enters the pool, and the scheduler's
/// pool always has at least one thread.
///
/// With heterogeneous tile sizes the gate is approximate: the admitting
/// limit is computed from the *entering* tile's own byte cost, so a
/// full-size tile entering while smaller edge tiles are live can overshoot
/// the ceiling slightly. Accepted: the ceiling is itself a stated constant,
/// not a measured device capacity.
pub struct VramBudget {
    max_device_bytes: u64,
    cap: usize,
    in_flight: Mutex<usize>,
    condvar: Condvar,
    peak: AtomicUsize,
}

impl VramBudget {
    /// `cap` is `std::thread::available_parallelism()` (unwrap_or 1) at
    /// construction time, the same source the scheduler's own budget reads.
    pub fn new(max_device_bytes: u64) -> Self {
        let cap = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        VramBudget {
            max_device_bytes,
            cap,
            in_flight: Mutex::new(0),
            condvar: Condvar::new(),
            peak: AtomicUsize::new(0),
        }
    }

    /// The in-flight limit for a tile costing `bytes_per_tile` device bytes,
    /// as [`Self::acquire`] computes it.
    pub fn limit_for(&self, bytes_per_tile: u64) -> usize {
        tiles_in_flight_for_budget(bytes_per_tile, self.max_device_bytes, self.cap)
    }

    /// Blocks until fewer than `limit_for(bytes_per_tile)` tiles are
    /// in-flight, then admits one and returns its RAII permit. The limit is
    /// recomputed on every call from the entering tile's own byte cost, not
    /// cached from a previous call.
    pub fn acquire(&self, bytes_per_tile: u64) -> Permit<'_> {
        self.admit(bytes_per_tile);
        Permit { budget: self }
    }

    /// [`Self::acquire`] returning a permit that keeps the budget alive.
    pub fn acquire_owned(self: &Arc<Self>, bytes_per_tile: u64) -> OwnedPermit {
        self.admit(bytes_per_tile);
        OwnedPermit {
            budget: Arc::clone(self),
        }
    }

    fn admit(&self, bytes_per_tile: u64) {
        let mut in_flight = self.in_flight.lock().unwrap();
        let limit = self.limit_for(bytes_per_tile);
        while *in_flight >= limit {
            in_flight = self.condvar.wait(in_flight).unwrap();
        }
        *in_flight += 1;
        self.peak.fetch_max(*in_flight, Ordering::SeqCst);
    }

    fn release(&self) {
        let mut in_flight = self.in_flight.lock().unwrap();
        *in_flight -= 1;
        self.condvar.notify_all();
    }

    /// The most tiles admitted at once since construction or the last
    /// [`Self::reset_peak`].
    pub fn peak_in_flight(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    /// Restarts the high-water interval [`Self::peak_in_flight`] reads.
    /// Call only while no permit is live: a reset racing a concurrent
    /// `acquire`'s `fetch_max` would lose a real admission.
    pub fn reset_peak(&self) {
        self.peak.store(0, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::describe_shortfalls;
    use focusforge_core::algorithm::StackAlgorithm;
    use focusforge_core::fuse::laplacian::Laplacian;
    use focusforge_core::params::{ParamValue, ParamValues, Provenance};
    use focusforge_core::tile::TilePlan;

    // --- DeviceBudget ---

    #[test]
    fn probed_reports_host_provenance() {
        let budget = DeviceBudget::probed(12_345);
        assert_eq!(budget.bytes(), 12_345);
        assert_eq!(budget.provenance(), Provenance::Host);
    }

    #[test]
    fn capped_to_lowers_when_the_ceiling_is_smaller() {
        let budget = DeviceBudget::probed(1_000).capped_to(Some(100));
        assert_eq!(budget.bytes(), 100);
        assert_eq!(budget.provenance(), Provenance::Explicit);
    }

    #[test]
    fn capped_to_leaves_a_higher_ceiling_unchanged() {
        let original = DeviceBudget::probed(1_000);
        let capped = original.capped_to(Some(u64::MAX));
        assert_eq!(capped, original);
        assert_eq!(capped.provenance(), Provenance::Host);
    }

    #[test]
    fn capped_to_none_leaves_the_budget_unchanged() {
        let original = DeviceBudget::probed(1_000);
        assert_eq!(original.capped_to(None), original);
    }

    // --- probe_budget ---

    /// A scripted reservation: refuses any request strictly above
    /// `accepts_up_to`, recording every requested byte count it was asked
    /// to try.
    struct FakeProbe {
        accepts_up_to: u64,
        requests: Mutex<Vec<u64>>,
    }

    impl FakeProbe {
        fn new(accepts_up_to: u64) -> Self {
            FakeProbe {
                accepts_up_to,
                requests: Mutex::new(Vec::new()),
            }
        }

        fn try_reserve(&self, bytes: u64) -> Result<(), String> {
            self.requests.lock().unwrap().push(bytes);
            if bytes <= self.accepts_up_to {
                Ok(())
            } else {
                Err(format!("refused {bytes} bytes"))
            }
        }
    }

    #[test]
    fn probe_settles_immediately_when_the_optimistic_start_is_accepted() {
        let probe = FakeProbe::new(u64::MAX);
        let outcome = probe_budget(|bytes| probe.try_reserve(bytes));
        match outcome {
            ProbeOutcome::Settled { bytes, attempts } => {
                assert_eq!(
                    bytes,
                    PROBE_START_MULTIPLE * DEFAULT_MAX_DEVICE_BYTES_IN_FLIGHT
                );
                assert_eq!(attempts, 1);
            }
            ProbeOutcome::Exhausted { .. } => panic!("expected Settled, got Exhausted"),
        }
        assert_eq!(probe.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_injected_refusal_shrinks_and_settles_at_the_largest_accepted_value() {
        // Refuses the optimistic start (8 GiB) but accepts everything at or
        // below one halving down (4 GiB): the loop must shrink exactly once
        // and settle there.
        let ceiling = PROBE_START_MULTIPLE * DEFAULT_MAX_DEVICE_BYTES_IN_FLIGHT / 2;
        let probe = FakeProbe::new(ceiling);
        let outcome = probe_budget(|bytes| probe.try_reserve(bytes));
        match outcome {
            ProbeOutcome::Settled { bytes, attempts } => {
                assert_eq!(bytes, ceiling);
                assert_eq!(attempts, 2);
            }
            ProbeOutcome::Exhausted { .. } => panic!("expected Settled, got Exhausted"),
        }
    }

    #[test]
    fn probe_converges_within_the_attempt_bound_and_never_raises_the_request() {
        // Never accepts anything -- the worst case for both the attempt
        // bound and monotonicity.
        let probe = FakeProbe::new(0);
        let outcome = probe_budget(|bytes| probe.try_reserve(bytes));
        let requests = probe.requests.lock().unwrap().clone();
        assert!(
            requests.len() as u32 <= PROBE_MAX_ATTEMPTS,
            "probe made {} attempts, bound is {PROBE_MAX_ATTEMPTS}",
            requests.len()
        );
        for pair in requests.windows(2) {
            assert!(
                pair[1] <= pair[0],
                "request size must never grow: {:?}",
                requests
            );
        }
        match outcome {
            ProbeOutcome::Exhausted { floor, .. } => {
                assert_eq!(floor, PROBE_MIN_DEVICE_BYTES);
            }
            ProbeOutcome::Settled { .. } => panic!("expected Exhausted, got Settled"),
        }
    }

    #[test]
    fn refused_all_the_way_down_yields_exhausted_naming_the_floor() {
        let probe = FakeProbe::new(0);
        let outcome = probe_budget(|bytes| probe.try_reserve(bytes));
        match outcome {
            ProbeOutcome::Exhausted { floor, detail } => {
                assert_eq!(floor, PROBE_MIN_DEVICE_BYTES);
                assert!(!detail.is_empty());
            }
            ProbeOutcome::Settled { .. } => panic!("expected Exhausted, got Settled"),
        }
        // Exactly `PROBE_MAX_ATTEMPTS` halvings from the optimistic start
        // down to the floor -- the bound is derived, not picked
        // independently.
        let requests = probe.requests.lock().unwrap();
        assert_eq!(requests.len() as u32, PROBE_MAX_ATTEMPTS);
        assert_eq!(*requests.last().unwrap(), PROBE_MIN_DEVICE_BYTES);
    }

    #[test]
    fn device_bytes_per_tile_at_fixture_scale() {
        // 512x512, 3 spp, 5 levels, warped -- the geometry `budget_gpu.rs`'s
        // serialization and allocator tests drive.
        let bytes = device_bytes_per_tile(512, 512, 3, 5, true, SampleFormat::U16);
        assert_eq!(bytes, 53_101_568);
    }

    #[test]
    fn fixture_scale_in_flight_is_an_exact_count_at_least_two() {
        let bytes = device_bytes_per_tile(512, 512, 3, 5, true, SampleFormat::U16);
        let in_flight = tiles_in_flight_for_budget(bytes, DEFAULT_MAX_DEVICE_BYTES_IN_FLIGHT, 32);
        assert_eq!(in_flight, 20);
        assert!(
            in_flight >= 2,
            "the GPU test needs a permit count to collapse"
        );
    }

    /// Largest allocator-report delta over baseline measured for one 512x512
    /// 3 spp warped tile (RTX 5070 Ti, wgpu 29.0.4), at 8, 32 and 128 frames.
    const MEASURED_TILE_PEAK_BYTES: u64 = 29_007_120;

    #[test]
    fn device_bytes_per_tile_is_about_twice_the_measured_peak() {
        let bytes = device_bytes_per_tile(512, 512, 3, 5, true, SampleFormat::U16);
        let tenths = bytes * 10 / MEASURED_TILE_PEAK_BYTES;
        assert!(
            (18..=22).contains(&tenths),
            "{bytes} B is {tenths}/10 x the measured peak"
        );
    }

    #[test]
    fn device_bytes_per_tile_scales_with_the_rect() {
        assert_eq!(
            device_bytes_per_tile(704, 704, 3, 5, true, SampleFormat::U16),
            100_395_152
        );
        assert_eq!(
            device_bytes_per_tile(300, 187, 3, 5, true, SampleFormat::U16),
            11_384_244
        );
        assert_eq!(
            device_bytes_per_tile(512, 512, 3, 5, false, SampleFormat::U16),
            43_839_488
        );
    }

    #[test]
    fn f32_tile_bytes_count_four_bytes_per_transfer_sample() {
        // 301x187x3 is an odd sample count, so the packed form rounds up.
        for (w, h) in [(512u32, 512u32), (301, 187)] {
            let samples = u64::from(w) * u64::from(h) * 3;
            let plane = 4 * samples;
            let packed = packed_len(samples as usize) as u64;
            let u16_warped = device_bytes_per_tile(w, h, 3, 5, true, SampleFormat::U16);
            let f32_warped = device_bytes_per_tile(w, h, 3, 5, true, SampleFormat::F32);
            // Destination and re-uploaded source are `plane`s; the base plane is the destination.
            assert_eq!(
                f32_warped + 2 * packed,
                u16_warped + plane,
                "{w}x{h} warped"
            );
            let u16_identity = device_bytes_per_tile(w, h, 3, 5, false, SampleFormat::U16);
            let f32_identity = device_bytes_per_tile(w, h, 3, 5, false, SampleFormat::F32);
            assert_eq!(f32_identity + packed, u16_identity, "{w}x{h} identity");
        }
        // The `f32` tile costs no more than the `u16` one at the fixture geometry.
        assert_eq!(
            device_bytes_per_tile(512, 512, 3, 5, true, SampleFormat::F32),
            53_101_568
        );
        assert_eq!(
            device_bytes_per_tile(512, 512, 3, 5, false, SampleFormat::F32),
            43_839_488 - 1_572_864
        );
    }

    #[test]
    fn a_tile_above_the_budget_names_its_bytes_and_the_host_path() {
        assert_eq!(
            tile_shortfall_message(512, 256, 53_098_496, 53_098_496),
            None
        );
        let message = tile_shortfall_message(512, 256, 53_098_497, 53_098_496)
            .expect("one byte over the budget is a shortfall");
        assert_eq!(
            message,
            "one 512x256 tile needs 53098497 B on the device, above the device budget of \
             53098496 B; continuing on the CPU path"
        );
    }

    /// The 576 px read-rect size and the `--max-vram` value (55 MB) the
    /// device-peak and device-loss CLI tests run with on the 512x512 fixture
    /// at five levels.
    #[test]
    fn the_cli_one_tile_cap_covers_the_asked_rect_and_stays_below_two_planned_tiles() {
        const TILE_SIZE: u32 = 576;
        const CAP: u64 = 55_000_000;
        const LEVELS: i64 = 5;

        let mut cfg = ParamValues::new();
        cfg.insert("levels".to_string(), ParamValue::Int(LEVELS));
        let support = Laplacian.tile_support(&cfg);
        let plan = TilePlan::new(512, 512, TILE_SIZE, support).expect("the fixture plans");
        assert!(plan.len() > 1, "{} tiles", plan.len());

        // The rect `pipeline::run` asks about; a warped run costs more than
        // an unwarped one, so the warped figure is the one the cap covers.
        let asked = TILE_SIZE.min(512);
        let asked_bytes =
            device_bytes_per_tile(asked, asked, 3, LEVELS as u32, true, SampleFormat::U16);
        assert!(asked_bytes <= CAP, "{asked_bytes} B above the cap");

        // The cheapest planned read rect is an unwarped one; two of them
        // must not fit.
        let cheapest = plan
            .tiles()
            .iter()
            .map(|t| {
                let read = t.read();
                device_bytes_per_tile(
                    read.width,
                    read.height,
                    3,
                    LEVELS as u32,
                    false,
                    SampleFormat::U16,
                )
            })
            .min()
            .expect("the plan has tiles");
        assert!(CAP < 2 * cheapest, "{CAP} B admits two {cheapest} B tiles");
        assert_eq!(tiles_in_flight_for_budget(cheapest, CAP, 32), 1);
    }

    #[test]
    fn in_flight_never_exceeds_the_cap() {
        // A tiny bytes-per-tile so the budget alone would allow hundreds
        // live, but the cap must still win.
        let in_flight = tiles_in_flight_for_budget(4, u64::MAX, 4);
        assert_eq!(in_flight, 4);
    }

    #[test]
    fn zero_bytes_per_tile_short_circuits_to_the_cap() {
        let in_flight = tiles_in_flight_for_budget(0, DEFAULT_MAX_DEVICE_BYTES_IN_FLIGHT, 7);
        assert_eq!(in_flight, 7);
    }

    #[test]
    fn cap_floors_at_one_even_if_available_parallelism_reports_zero() {
        let in_flight = tiles_in_flight_for_budget(0, DEFAULT_MAX_DEVICE_BYTES_IN_FLIGHT, 0);
        assert_eq!(in_flight, 1);
    }

    #[test]
    fn largest_buffer_bytes_is_the_coord_array_when_warped() {
        assert_eq!(largest_buffer_bytes(512, 512, 3, true), 24 * 512 * 512);
    }

    #[test]
    fn largest_buffer_bytes_is_the_base_plane_when_not_warped() {
        assert_eq!(largest_buffer_bytes(512, 512, 3, false), 512 * 512 * 3 * 4);
    }

    #[test]
    fn buffer_limit_shortfalls_names_the_short_limit_with_both_numbers() {
        let shortfalls = buffer_limit_shortfalls(400 << 20, 128 << 20, 256 << 20);
        assert!(!shortfalls.is_empty());
        let message = describe_shortfalls(&shortfalls);
        assert!(message.contains("max_storage_buffer_binding_size"));
        assert!(message.contains(&(400u64 << 20).to_string()));
        assert!(message.contains(&(128u64 << 20).to_string()));
    }

    #[test]
    fn buffer_limit_shortfalls_is_empty_when_in_budget() {
        let shortfalls = buffer_limit_shortfalls(64 << 20, 128 << 20, 256 << 20);
        assert!(shortfalls.is_empty());
    }

    const DEFAULT_BINDING: u64 = 128 << 20;
    const DEFAULT_BUFFER: u64 = 256 << 20;
    const DEFAULT_WORKGROUPS: u32 = 65_535;

    #[test]
    fn max_tile_side_under_default_limits_is_dispatch_bound() {
        for (spp, side) in [(1, 2047), (3, 1182), (4, 1023)] {
            assert_eq!(
                max_tile_side(spp, DEFAULT_WORKGROUPS, DEFAULT_BINDING, DEFAULT_BUFFER),
                side,
                "samples per pixel {spp}"
            );
        }
    }

    #[test]
    fn max_tile_side_is_buffer_bound_when_workgroups_are_plentiful() {
        assert_eq!(
            max_tile_side(3, u32::MAX, DEFAULT_BINDING, DEFAULT_BUFFER),
            2364
        );
        // The smaller of the two buffer limits binds.
        assert_eq!(
            max_tile_side(3, u32::MAX, DEFAULT_BUFFER, DEFAULT_BINDING),
            2364
        );
    }

    #[test]
    fn max_tile_side_under_a_tiny_workgroup_limit() {
        assert_eq!(max_tile_side(1, 16, DEFAULT_BINDING, DEFAULT_BUFFER), 32);
        assert_eq!(max_tile_side(3, 16, DEFAULT_BINDING, DEFAULT_BUFFER), 18);
    }

    #[test]
    fn max_tile_side_is_the_largest_side_both_checks_accept() {
        for spp in [1, 3, 4] {
            let side = max_tile_side(spp, DEFAULT_WORKGROUPS, DEFAULT_BINDING, DEFAULT_BUFFER);
            let accepts = |side: u32| {
                dispatch_limit_shortfall(
                    u64::from(side) * u64::from(side) * u64::from(spp),
                    DEFAULT_WORKGROUPS,
                )
                .is_none()
                    && buffer_limit_shortfalls(
                        largest_buffer_bytes(side, side, spp, true),
                        DEFAULT_BINDING,
                        DEFAULT_BUFFER,
                    )
                    .is_empty()
            };
            assert!(accepts(side), "spp {spp}: {side} must be accepted");
            assert!(
                !accepts(side + 1),
                "spp {spp}: {} must be refused",
                side + 1
            );
        }
    }

    #[test]
    fn dispatch_limit_shortfall_is_none_at_the_limit_and_names_it_one_past() {
        assert_eq!(
            dispatch_limit_shortfall(4_194_240, DEFAULT_WORKGROUPS),
            None
        );
        assert_eq!(dispatch_limit_shortfall(0, DEFAULT_WORKGROUPS), None);
        let past = dispatch_limit_shortfall(4_194_241, DEFAULT_WORKGROUPS)
            .expect("one invocation past the limit needs one workgroup more");
        assert_eq!(past.limit, "max_compute_workgroups_per_dimension");
        assert_eq!(past.required, 65_536);
        assert_eq!(past.available, 65_535);
    }

    #[test]
    fn a_tiny_budget_serializes_two_threads_to_one_at_a_time() {
        // No GPU needed: `VramBudget` itself names no `wgpu` type. Two
        // threads both try to acquire a one-permit budget; this proves the
        // gate actually blocks rather than merely computing a limit.
        let budget = std::sync::Arc::new(VramBudget::new(1));
        let live = std::sync::Arc::new(AtomicUsize::new(0));
        let observed_concurrent = std::sync::Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let budget = budget.clone();
            let live = live.clone();
            let observed_concurrent = observed_concurrent.clone();
            handles.push(std::thread::spawn(move || {
                let _permit = budget.acquire(1);
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                observed_concurrent.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(5));
                live.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(observed_concurrent.load(Ordering::SeqCst), 1);
        assert_eq!(budget.peak_in_flight(), 1);
        assert_eq!(budget.limit_for(1), 1);
    }

    #[test]
    fn reset_peak_restarts_the_high_water_interval() {
        // One permit at a time on one thread: `cap` comes from
        // `available_parallelism`, so a nested `acquire` on a single-core
        // host would block forever.
        let budget = VramBudget::new(1);

        let permit = budget.acquire(1);
        drop(permit);
        assert_eq!(budget.peak_in_flight(), 1);

        budget.reset_peak();
        assert_eq!(budget.peak_in_flight(), 0);

        let permit = budget.acquire(1);
        drop(permit);
        // The second interval is measured on its own, not as the max over
        // both.
        assert_eq!(budget.peak_in_flight(), 1);
    }
}
