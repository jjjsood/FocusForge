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
//! **Every byte a tile allocates counts, not a live high-water mark.**
//! Dropping a `wgpu::Buffer` does not promise immediate reclamation, and the
//! pipeline polls only at its two per-tile readbacks (composite, Z-Map), so
//! [`device_bytes_per_tile`] treats every intermediate as live for the
//! whole tile. Conservative; the allocator-report test prints the measurement.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use focusforge_core::params::Provenance;

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
/// pyramid builds while folding this one frame in.
fn per_frame_bytes(width: u32, height: u32, spp: u32, levels: u32, warped: bool) -> u64 {
    let dims = level_dims(width, height, levels);
    let spp64 = u64::from(spp);
    let (w0, h0) = dims[0];

    // Warp destination buffer / identity-transform upload, and the source
    // `crate::transfer::upload_tile` re-uploads when warped.
    let mut total = packed(w0, h0, spp64);

    if warped {
        // Coord array, 24 bytes/destination pixel.
        total += 24 * w0 * h0;
        // Re-uploaded source; a model, not a bound, for a large transform
        // whose `Transform::source_footprint` read rect exceeds the
        // destination rect.
        total += packed(w0, h0, spp64);
    }

    // The `f32` base plane, via `plane_from_packed`.
    total += plane(w0, h0, spp64);

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
/// interleaved `(score, winner)` plane per level, the residual sum, the
/// collapse's expand-and-add per level, the packed output, and the tile's
/// two readback staging buffers.
fn once_per_tile_bytes(width: u32, height: u32, spp: u32, levels: u32) -> u64 {
    let dims = level_dims(width, height, levels);
    let spp64 = u64::from(spp);
    let (w0, h0) = dims[0];
    let mut total = 0u64;

    // Running best level + interleaved (score, winner) plane, per level.
    for &(wk, hk) in dims.iter().take(levels as usize) {
        total += plane(wk, hk, spp64) + 8 * wk * hk;
    }

    // Residual sum, at the coarsest level.
    let (wl, hl) = dims[levels as usize];
    total += plane(wl, hl, spp64);

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

/// Every byte one `crate::stack_tile::warp_fuse_tile_gpu` call allocates on
/// the device for a tile of `frame_count` frames, a `width x height`
/// destination rect at `spp` samples per pixel and `levels` pyramid depth --
/// `frame_count * per_frame + once_per_tile`, `u64` throughout (no `as u32`
/// narrowing). `warped` is a single flag for the whole tile, not per frame:
/// an approximation in the conservative direction, since an all-identity
/// tile allocates strictly less than this.
///
/// Two documented exclusions: the 16-32 B uniform params buffer each
/// dispatch allocates (`crate::pipeline::upload_params`, roughly 13 per
/// level per frame) is under 0.1% of the sum and is left out; and the
/// warped-source term above is a model, not a bound, for the same reason
/// named there. Both exclusions are dominated by orders of magnitude by
/// every other term, which is what keeps the sum conservative overall.
pub fn device_bytes_per_tile(
    frame_count: usize,
    width: u32,
    height: u32,
    spp: u32,
    levels: u32,
    warped: bool,
) -> u64 {
    let per_frame = per_frame_bytes(width, height, spp, levels, warped);
    let once = once_per_tile_bytes(width, height, spp, levels);
    (frame_count as u64) * per_frame + once
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

/// The device-side VRAM working-set budget: a byte count, where it came
/// from, and whether a live probe set it. Mirrors `HostBudget`.
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
        let mut in_flight = self.budget.in_flight.lock().unwrap();
        *in_flight -= 1;
        self.budget.condvar.notify_all();
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
        let mut in_flight = self.in_flight.lock().unwrap();
        let limit = self.limit_for(bytes_per_tile);
        while *in_flight >= limit {
            in_flight = self.condvar.wait(in_flight).unwrap();
        }
        *in_flight += 1;
        self.peak.fetch_max(*in_flight, Ordering::SeqCst);
        Permit { budget: self }
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
    use focusforge_core::params::Provenance;

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
        // 8 frames, 512x512, 3 spp, 5 levels, warped -- the geometry
        // `budget_gpu.rs`'s serialization test drives.
        let bytes = device_bytes_per_tile(8, 512, 512, 3, 5, true);
        assert_eq!(bytes, 254_661_632);
    }

    #[test]
    fn fixture_scale_in_flight_is_an_exact_count_at_least_two() {
        let bytes = device_bytes_per_tile(8, 512, 512, 3, 5, true);
        let in_flight = tiles_in_flight_for_budget(bytes, DEFAULT_MAX_DEVICE_BYTES_IN_FLIGHT, 32);
        assert_eq!(in_flight, 4);
        assert!(
            in_flight >= 2,
            "the GPU test needs a permit count to collapse"
        );
    }

    #[test]
    fn device_bytes_per_tile_at_reference_scale_exceeds_the_default_ceiling() {
        // 61 frames, same per-tile geometry.
        let bytes = device_bytes_per_tile(61, 512, 512, 3, 5, true);
        assert_eq!(bytes, 1_791_373_312);
        assert!(bytes > DEFAULT_MAX_DEVICE_BYTES_IN_FLIGHT);
    }

    #[test]
    fn reference_scale_floors_at_one_never_zero() {
        let bytes = device_bytes_per_tile(61, 512, 512, 3, 5, true);
        let in_flight = tiles_in_flight_for_budget(bytes, DEFAULT_MAX_DEVICE_BYTES_IN_FLIGHT, 32);
        assert_eq!(in_flight, 1);
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

    /// Pins the `--max-vram` ceiling `crates/cli/tests/cli_gpu.rs`'s
    /// `device_peak_in_flight_is_scoped_to_its_own_group` uses (`64MiB`),
    /// against the *ragged* read rects a 512x512 frame plans into at
    /// `--tile-size 256 --set levels=4` (208/256/240 px per axis via
    /// `axis_partition`, `margin_for_levels(4) == 48`): every combination of
    /// those three sizes on each axis is one tile's read rect in that plan's
    /// 3x3 grid. `spp = 3`, `levels = 4`, `warped = false` (the CLI test runs
    /// `--no-align`, so the warp flag is false).
    #[test]
    fn max_vram_ceiling_floors_the_heavy_group_at_one_tile_for_every_ragged_rect() {
        const CEILING: u64 = 64 << 20; // the literal `--max-vram` value the CLI test passes
        const RECT_SIZES: [u32; 3] = [208, 256, 240];
        const SPP: u32 = 3;
        const LEVELS: u32 = 4;
        const CAP: usize = 32;

        // The 2-frame group's most expensive rect still admits at least two
        // tiles at once -- not a running max, an actual concurrency.
        let mut max_bytes_2 = 0u64;
        let mut min_bytes_16 = u64::MAX;
        for &w in &RECT_SIZES {
            for &h in &RECT_SIZES {
                let bytes_2 = device_bytes_per_tile(2, w, h, SPP, LEVELS, false);
                let bytes_16 = device_bytes_per_tile(16, w, h, SPP, LEVELS, false);
                max_bytes_2 = max_bytes_2.max(bytes_2);
                min_bytes_16 = min_bytes_16.min(bytes_16);
                // Every ragged rect floors the 16-frame group at exactly one
                // tile -- including its cheapest rect, the tightest case.
                assert_eq!(
                    tiles_in_flight_for_budget(bytes_16, CEILING, CAP),
                    1,
                    "w={w} h={h} bytes_16={bytes_16} must floor at one tile"
                );
            }
        }
        assert!(
            tiles_in_flight_for_budget(max_bytes_2, CEILING, CAP) >= 2,
            "the 2-frame group's heaviest rect ({max_bytes_2} bytes) must still admit \
             two or more tiles under a {CEILING}-byte ceiling"
        );
        // Both bounds hold with margin, not by a hair: sanity-check the
        // window this literal ceiling sits inside.
        assert!(2 * max_bytes_2 <= CEILING);
        assert!(CEILING < 2 * min_bytes_16);
    }
}
