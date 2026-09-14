// SPDX-License-Identifier: GPL-3.0-or-later

//! The VRAM working-set budget: how many tiles `crate::stack_tile`'s
//! pipeline keeps in flight on the device, the device-side twin of the tile
//! scheduler's host-memory budget. Names no `wgpu` type, so it is not
//! feature-gated.
//!
//! **Why a stated constant, not a measured VRAM size.** wgpu 29.0.4 exposes
//! no adapter memory-size query at all (`Adapter` docs:
//! <https://docs.rs/wgpu/29.0.4/wgpu/struct.Adapter.html> -- `limits`,
//! `get_info`, `features`, `get_downlevel_capabilities`,
//! `get_texture_format_features`, `get_presentation_timestamp`, nothing
//! memory-sized; gfx-rs/wgpu#2447 is open). [`DEFAULT_MAX_DEVICE_BYTES_IN_FLIGHT`]
//! is therefore a stated 1 GiB ceiling, identical reasoning to
//! `crate::tile::scheduler::DEFAULT_MAX_BYTES_IN_FLIGHT`. The adapter's
//! reported limits are used for what they *can* answer: refusing, before
//! dispatch, a single allocation that would exceed a hard device limit
//! (`buffer_limit_shortfalls`, below).
//!
//! **Why every byte allocated during a tile, not a live high-water mark.**
//! `wgpu::Buffer`'s drop does not promise immediate reclamation, and this
//! pipeline polls the device only at its two per-tile readbacks
//! (`crate::transfer::download_buffer`, called at
//! `crate::fuse::fuse_planes_gpu`'s own tail for the composite and the
//! Z-Map). [`device_bytes_per_tile`] therefore counts every intermediate a
//! tile allocates as if it stayed live for the whole tile, even the ones
//! dropped mid-tile -- deliberately conservative, proven or disproven by
//! `crates/gpu/tests/harness/budget_gpu.rs`'s allocator-report test rather
//! than assumed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

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

/// `w*h` -- a pixel count, no sample factor.
fn pix(w: u64, h: u64) -> u64 {
    w * h
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
        total += 24 * pix(w0, h0);
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
        total += 2 * 4 * pix(wk, hk);
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
        total += plane(wk, hk, spp64) + 8 * pix(wk, hk);
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
    total += 8 * pix(w0, h0);

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
        24 * pix(w0, h0)
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

    /// The in-flight limit for a tile costing `bytes_per_tile` device
    /// bytes -- the live path [`Self::acquire`] itself calls, not a
    /// test-only helper.
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

    /// The high-water mark of tiles admitted at once since construction --
    /// the observable the GPU-gated serialization test asserts on, the same
    /// high-water shape `TileRunReport::peak_in_flight` already has.
    pub fn peak_in_flight(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::describe_shortfalls;

    // Mirrors `crates/core/src/tile/scheduler.rs`'s three budget cases,
    // plus the zero short-circuit `scheduler.rs` covers inline rather than
    // as its own named test.

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
}
