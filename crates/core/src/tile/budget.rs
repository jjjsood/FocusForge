// SPDX-License-Identifier: GPL-3.0-or-later

//! The host-memory twin of `crates/gpu/src/budget.rs`: how many frame-tile
//! sets the tile scheduler keeps in flight at once, and the runtime back-off
//! that shrinks that limit when this process's own memory footprint says the
//! starting estimate was too optimistic. Same `Permit`-on-`Mutex`+`Condvar`
//! shape as `VramBudget`/`Permit`, so the two budgets read as one pattern
//! one crate apart.
//!
//! Unlike the GPU side, the byte budget here is a pure **admission gate**,
//! not the rayon pool size: the pool is sized from
//! `std::thread::available_parallelism()` regardless of the budget, so a
//! large stack on a large host keeps more than one worker busy even when
//! the budget alone would floor a thread-count-based limit to one.
//!
//! # The back-off signal
//!
//! This process's RSS, read through [`crate::host::HostProbe`]: no new
//! `#[global_allocator]`, no `unsafe_code`. The back-off rule (a private
//! `back_off_now` function) fires when both halves agree: the process has
//! eaten more than
//! [`PRESSURE_SHARE_PERCENT`] of the headroom the host reported available
//! when the run started (the RSS half), *and* the scheduler's own
//! self-accounted reserved bytes exceed one tile (the "this pressure is
//! ours to answer" half) -- with only one tile reserved, shrinking
//! cannot give anything back, so pressure that is not this run's own is not
//! acted on.
//!
//! Polled at most once per [`PRESSURE_POLL_INTERVAL_MS`], compare-and-swap
//! on the last-poll timestamp so exactly one worker pays for each read and
//! every other worker admitted in the same window skips the probe (and its
//! `Mutex`) entirely rather than blocking on it. A single confirming poll
//! never shrinks the limit -- [`PRESSURE_CONFIRMATIONS`] consecutive polls
//! must agree, so a transient spike costs nothing. A poll that does not
//! confirm resets the streak to zero. Once shrunk, the limit is never raised
//! again for the rest of the run: nothing here can tell "pressure gone" from
//! "pressure paused", and a limit that oscillates costs more in
//! re-allocation churn than the recovered concurrency is worth.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::host::{FactsOrigin, HostFacts, HostProbe};
use crate::params::Provenance;
use crate::tile::control::{ADMISSION_RECHECK, TileControl};
use crate::tile::scheduler::{DEFAULT_MAX_BYTES_IN_FLIGHT, InFlightBound, in_flight_for_budget};

/// The share of the available memory beyond [`HOST_FIXED_BYTES`] that a
/// host-derived budget claims: `share * (usable_available - fixed) / 100`.
///
/// Peak memory is the fixed part plus at most 0.39 B per budget byte (GPU,
/// 0.04 on CPU). At the smallest RAW budget (586 MB, 2463 MB available) the
/// CPU path peaks at 1587 MB (64 %), the GPU path at 1035 MB (42 %), under 85 %.
pub const HOST_BUDGET_SHARE_PERCENT: u64 = 65;

/// The non-reclaimable memory a run holds regardless of its budget (driver
/// and host buffers, process base), taken from the worst measured path: the
/// CPU path of a RAW stack peaked at 1563 MB with the budget at its
/// smallest, against 807 MB (RAW, GPU), 921 MB (TIFF, GPU), 1465 MB (TIFF,
/// CPU) and 24 MB (export). A host with no more than this available gets no
/// host-derived budget.
pub const HOST_FIXED_BYTES: u64 = 1_563_000_000;

/// The decoded-frame cache receives `1 / DECODED_CACHE_SHARE_DIVISOR` of a
/// capped host budget; the scheduler keeps the rest. A RAW cache takes at
/// least its largest frame's decode reservation instead.
pub const DECODED_CACHE_SHARE_DIVISOR: u64 = 4;

/// What limited a memory bound, so a refusal can tell the user which
/// resource to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryBound {
    /// The bound derives from the host's free RAM:
    /// `(free RAM - fixed_bytes) * share_percent / (100 * divisor)`.
    FreeRam {
        share_percent: u64,
        fixed_bytes: u64,
        divisor: u64,
    },
    /// The bound derives from a fixed memory limit: `limit / divisor`.
    Limit { divisor: u64 },
}

/// The share of the *starting* available memory this process may eat before
/// the back-off rule's RSS half fires: three quarters of the headroom the
/// host said it had when the run started is gone, and the remaining quarter
/// is not enough slack to keep trusting a starting estimate.
pub const PRESSURE_SHARE_PERCENT: u64 = 75;

/// The minimum gap between two pressure polls, in milliseconds. A refresh is
/// one `/proc/meminfo` read plus one `/proc/<pid>` read
/// (`crate::host::SysinfoProbe::facts`) and takes the probe's own `Mutex`;
/// an unthrottled read on every tile admission would serialize workers on a
/// lock that has nothing to do with the work.
pub const PRESSURE_POLL_INTERVAL_MS: u64 = 200;

/// The number of consecutive confirming polls required before the limit
/// actually shrinks. One poll under a transient spike costs the run
/// nothing.
pub const PRESSURE_CONFIRMATIONS: u32 = 2;

/// A source of elapsed time, substitutable for tests. An integration test
/// in `crates/core/tests/` cannot reach a `#[cfg(test)]` fake, so
/// [`ManualClock`] is a shipped public test double, exactly as
/// `crate::host::FixedFacts` is.
pub trait Clock: Send + Sync {
    fn elapsed_millis(&self) -> u64;
}

/// The real clock: milliseconds elapsed since construction.
pub struct MonotonicClock {
    start: Instant,
}

impl MonotonicClock {
    pub fn new() -> Self {
        MonotonicClock {
            start: Instant::now(),
        }
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MonotonicClock {
    fn elapsed_millis(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

/// A fake clock a test drives explicitly with [`ManualClock::advance`],
/// starting at zero.
#[derive(Default)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    pub fn new() -> Self {
        ManualClock(AtomicU64::new(0))
    }

    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn elapsed_millis(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// The substitution seam for both the host-facts probe and the clock: what
/// [`TileSchedulerOptions`](super::scheduler::TileSchedulerOptions) carries
/// so a test can script both without touching the real machine.
#[derive(Clone)]
pub struct HostSources {
    pub probe: Arc<dyn HostProbe>,
    pub clock: Arc<dyn Clock>,
}

impl Default for HostSources {
    fn default() -> Self {
        HostSources {
            probe: Arc::new(crate::host::SysinfoProbe::new()),
            clock: Arc::new(MonotonicClock::new()),
        }
    }
}

/// The host in-flight-byte budget: a number of bytes, where it came from,
/// and whether it was actually derived from a live probe (which is what
/// arms the runtime back-off -- an `explicit` budget, and a cap applied to
/// one, never probes unless the reader reports a page-cache working set,
/// keeping every other test deterministic).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostBudget {
    bytes: u64,
    provenance: Provenance,
    host_probed: bool,
}

/// The shares of one capped host budget used by decoded TIFF input and tile
/// scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedCacheBudget {
    /// Bytes available to decoded TIFF segments.
    pub cache: HostBudget,
    /// Bytes available to tile scheduling.
    pub scheduler: HostBudget,
}

impl DecodedCacheBudget {
    /// What bounds the decoded-frame cache share.
    pub fn cache_bound(&self) -> MemoryBound {
        self.cache.memory_bound_of(DECODED_CACHE_SHARE_DIVISOR)
    }
}

impl HostBudget {
    /// Derives a budget from host facts: [`HOST_BUDGET_SHARE_PERCENT`] of
    /// what the host currently reports available beyond [`HOST_FIXED_BYTES`]
    /// (zero at or under it) when the facts were actually probed, or the
    /// stated [`DEFAULT_MAX_BYTES_IN_FLIGHT`] constant -- never deleted, the
    /// degraded answer -- when the probe was unavailable.
    pub fn host_derived(facts: &HostFacts) -> Self {
        match facts.origin() {
            FactsOrigin::Probed => HostBudget {
                bytes: facts.usable_available().saturating_sub(HOST_FIXED_BYTES)
                    * HOST_BUDGET_SHARE_PERCENT
                    / 100,
                provenance: Provenance::Host,
                host_probed: true,
            },
            FactsOrigin::Fallback => HostBudget {
                bytes: DEFAULT_MAX_BYTES_IN_FLIGHT as u64,
                provenance: Provenance::HostFallback,
                host_probed: false,
            },
        }
    }

    /// A caller-supplied budget: never arms the back-off, always floors at 1
    /// tile rather than refusing.
    pub fn explicit(bytes: u64) -> Self {
        HostBudget {
            bytes,
            provenance: Provenance::Explicit,
            host_probed: false,
        }
    }

    /// Applies a ceiling: when `ceiling` is below the current budget, the
    /// result is that ceiling, reported as `Explicit` -- a ceiling is a
    /// caller choice, not a host-derived one. A ceiling at or above the
    /// current budget changes nothing, including its provenance and its
    /// `host_probed` bit: the phase's binding constraint ("the flag can
    /// never raise the budget") expressed in one function.
    pub fn capped_to(self, ceiling: Option<u64>) -> Self {
        match ceiling {
            Some(ceiling) if ceiling < self.bytes => HostBudget {
                bytes: ceiling,
                provenance: Provenance::Explicit,
                host_probed: self.host_probed,
            },
            _ => self,
        }
    }

    /// Splits an already-capped budget between decoded TIFF segments and the
    /// tile scheduler. The cache receives one quarter; the scheduler receives
    /// the exact remainder while retaining this budget's source and pressure
    /// behavior.
    pub fn partition_for_decoded_cache(self) -> DecodedCacheBudget {
        let cache_bytes = self.bytes / DECODED_CACHE_SHARE_DIVISOR;
        DecodedCacheBudget {
            cache: HostBudget {
                bytes: cache_bytes,
                ..self
            },
            scheduler: HostBudget {
                bytes: self.bytes - cache_bytes,
                ..self
            },
        }
    }

    /// What bounds a read made against this whole budget: free RAM for a
    /// host-derived budget, a fixed limit for an explicit, capped or fallback
    /// one.
    pub fn memory_bound(&self) -> MemoryBound {
        self.memory_bound_of(1)
    }

    fn memory_bound_of(&self, divisor: u64) -> MemoryBound {
        match self.provenance {
            Provenance::Host => MemoryBound::FreeRam {
                share_percent: HOST_BUDGET_SHARE_PERCENT,
                fixed_bytes: HOST_FIXED_BYTES,
                divisor,
            },
            _ => MemoryBound::Limit { divisor },
        }
    }

    /// This budget less `bytes` (floored at 0), keeping its provenance and
    /// probe state.
    pub fn reduced_by(self, bytes: u64) -> Self {
        HostBudget {
            bytes: self.bytes.saturating_sub(bytes),
            ..self
        }
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn provenance(&self) -> Provenance {
        self.provenance
    }

    /// Whether a budget below one tile should refuse the run outright
    /// rather than floor at 1: only when this budget is host-derived *and*
    /// uncapped. An explicitly supplied or flag-capped budget always floors
    /// instead.
    pub fn refuses_below_one_tile(&self) -> bool {
        self.provenance == Provenance::Host
    }

    /// Whether this budget arms the runtime back-off: only a budget that
    /// actually came from a live probe watches for pressure. An `explicit`
    /// budget, and a cap applied to one, never watches, keeping every test
    /// that passes a fixed budget deterministic. A cap applied to a
    /// host-derived budget keeps it armed.
    pub fn arms_back_off(&self) -> bool {
        self.host_probed
    }
}

/// The back-off rule, as one free function so it is unit-testable without a
/// gate. `None` for either RSS reading means "no signal" and always answers
/// `false` -- a process-RSS read that failed disarms the back-off rather
/// than triggering it.
fn back_off_now(
    rss_now: Option<u64>,
    rss_at_start: Option<u64>,
    available_at_start: u64,
    reserved_bytes: u64,
    bytes_per_tile: u64,
) -> bool {
    let (Some(rss_now), Some(rss_at_start)) = (rss_now, rss_at_start) else {
        return false;
    };
    let grown = rss_now.saturating_sub(rss_at_start);
    let threshold = available_at_start * PRESSURE_SHARE_PERCENT / 100;
    grown > threshold && reserved_bytes > bytes_per_tile
}

/// How many tile columns' file-cache working sets fit the memory limit once
/// [`HOST_FIXED_BYTES`] and the heap of `in_flight` tiles of `bytes_per_tile`
/// are set aside, floored at 1. The limit is the cgroup's, else the host's
/// available memory (a cgroup's free figure counts the cache itself). `None`
/// without a working set or a real probe.
fn band_columns(
    facts: &HostFacts,
    in_flight: usize,
    bytes_per_tile: u64,
    working_set_bytes: u64,
) -> Option<usize> {
    if working_set_bytes == 0 || facts.origin() == FactsOrigin::Fallback {
        return None;
    }
    let limit = facts
        .cgroup_limit()
        .unwrap_or_else(|| facts.available_memory());
    let heap = (in_flight as u64).saturating_mul(bytes_per_tile);
    let cache = limit.saturating_sub(HOST_FIXED_BYTES).saturating_sub(heap);
    Some(
        usize::try_from(cache / working_set_bytes)
            .unwrap_or(usize::MAX)
            .max(1),
    )
}

struct GateState {
    in_flight: usize,
    limit: usize,
}

/// An RAII permit from [`HostBudgetGate::acquire`]: dropping it always
/// decrements the in-flight count and wakes every waiter, so an early `?`
/// return from inside the tile body releases the permit exactly as reliably
/// as falling off the end of it.
pub struct HostPermit<'a> {
    gate: &'a HostBudgetGate,
}

impl Drop for HostPermit<'_> {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap();
        state.in_flight -= 1;
        self.gate.condvar.notify_all();
    }
}

/// The admission gate: how many frame tiles may be live at once, and the
/// pressure state that can shrink that limit during a run. Blocking a rayon
/// worker inside [`Self::acquire`] cannot deadlock: every permit is released
/// by an RAII guard on the same thread, and nothing re-enters the pool
/// inside a permit region.
pub struct HostBudgetGate {
    state: Mutex<GateState>,
    condvar: Condvar,
    initial_limit: usize,
    initial_bound: InFlightBound,
    band_columns: Option<usize>,
    bytes_per_tile: u64,
    peak: AtomicUsize,
    sources: HostSources,
    arms_back_off: bool,
    last_poll_ms: AtomicU64,
    consecutive_hits: AtomicU32,
    rss_at_start: Option<u64>,
    available_at_start: u64,
}

impl HostBudgetGate {
    /// `cap` is the pool's own thread count
    /// (`std::thread::available_parallelism`, the same source
    /// `TileScheduler::new` already reads) -- never more in-flight tiles
    /// than there are workers to hold them.
    ///
    /// `working_set_bytes` is the file-cache memory one tile's reads keep hot
    /// across all frames (zero for a reader that has none). A non-zero value
    /// leaves the initial limit alone and sets [`Self::band_columns`].
    pub fn new(
        budget: HostBudget,
        bytes_per_tile: u64,
        working_set_bytes: u64,
        cap: usize,
        sources: HostSources,
    ) -> Self {
        let (initial_limit, initial_bound) =
            in_flight_for_budget(bytes_per_tile, budget.bytes(), cap);
        let arms_back_off = budget.arms_back_off();
        // The probe is read only for a host-derived budget (capped or not) or
        // a reader with a page-cache working set: an `explicit` budget, or a
        // cap applied to one, over a reader without one never reads
        // `sources.probe`, so a test that supplies no probe worth calling
        // stays deterministic.
        let facts = (arms_back_off || working_set_bytes > 0).then(|| sources.probe.facts());
        let band_columns = facts.as_ref().and_then(|facts| {
            band_columns(facts, initial_limit, bytes_per_tile, working_set_bytes)
        });
        let (rss_at_start, available_at_start) = match facts {
            Some(facts) if arms_back_off => (facts.process_rss(), facts.usable_available()),
            _ => (None, 0),
        };
        HostBudgetGate {
            state: Mutex::new(GateState {
                in_flight: 0,
                limit: initial_limit,
            }),
            condvar: Condvar::new(),
            initial_limit,
            initial_bound,
            band_columns,
            bytes_per_tile,
            peak: AtomicUsize::new(0),
            sources,
            arms_back_off,
            last_poll_ms: AtomicU64::new(0),
            consecutive_hits: AtomicU32::new(0),
            rss_at_start,
            available_at_start,
        }
    }

    /// Blocks until fewer than the smaller of [`Self::limit`] and the
    /// control's admission cap tiles are in flight, then admits one and
    /// returns its RAII permit. Polls pressure first (subject to its own
    /// cadence and CAS), so a limit shrink is visible to this very call's
    /// own admission check. The cap is never written into the limit, so it
    /// cannot raise it or hide a back-off. While the cap binds, the wait
    /// re-checks every 10 ms.
    pub fn acquire(&self, control: &dyn TileControl) -> HostPermit<'_> {
        if self.arms_back_off {
            self.maybe_poll_pressure();
        }
        let mut state = self.state.lock().unwrap();
        loop {
            let cap = control.admission_cap();
            if state.in_flight < state.limit.min(cap) {
                break;
            }
            state = if cap < state.limit {
                self.condvar
                    .wait_timeout(state, ADMISSION_RECHECK)
                    .unwrap()
                    .0
            } else {
                self.condvar.wait(state).unwrap()
            };
        }
        state.in_flight += 1;
        self.peak.fetch_max(state.in_flight, Ordering::SeqCst);
        HostPermit { gate: self }
    }

    /// The admission limit in force right now -- the initial limit until a
    /// back-off shrinks it, and never above the initial limit again.
    pub fn limit(&self) -> usize {
        self.state.lock().unwrap().limit
    }

    /// The limit this gate was constructed with, before any back-off.
    pub fn initial_limit(&self) -> usize {
        self.initial_limit
    }

    /// The term that set [`Self::initial_limit`] before the floor of 1.
    pub fn initial_bound(&self) -> InFlightBound {
        self.initial_bound
    }

    /// How many tile columns' working sets the page cache holds, when the
    /// reader reports a working set and the host facts are real; the width of
    /// the vertical bands tiles are admitted in. Never below 1.
    pub fn band_columns(&self) -> Option<usize> {
        self.band_columns
    }

    /// The high-water mark of tiles admitted at once since construction.
    pub fn peak_in_flight(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    /// Whether the limit has ever shrunk below its initial value.
    pub fn backed_off(&self) -> bool {
        self.limit() < self.initial_limit
    }

    fn maybe_poll_pressure(&self) {
        let now = self.sources.clock.elapsed_millis();
        let last = self.last_poll_ms.load(Ordering::SeqCst);
        if now.saturating_sub(last) < PRESSURE_POLL_INTERVAL_MS {
            return;
        }
        if self
            .last_poll_ms
            .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            // Another worker already claimed this interval's poll.
            return;
        }

        let facts = self.sources.probe.facts();
        let reserved_bytes = {
            let state = self.state.lock().unwrap();
            state.in_flight as u64 * self.bytes_per_tile
        };
        let hit = back_off_now(
            facts.process_rss(),
            self.rss_at_start,
            self.available_at_start,
            reserved_bytes,
            self.bytes_per_tile,
        );
        if hit {
            let hits = self.consecutive_hits.fetch_add(1, Ordering::SeqCst) + 1;
            if hits >= PRESSURE_CONFIRMATIONS {
                self.consecutive_hits.store(0, Ordering::SeqCst);
                let mut state = self.state.lock().unwrap();
                state.limit = (state.limit / 2).max(1);
            }
        } else {
            self.consecutive_hits.store(0, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::control::NoControl;
    use std::sync::Mutex as StdMutex;

    fn probed_facts(available: u64, process_rss: Option<u64>) -> HostFacts {
        HostFacts::new(available * 3, available, None, None, 4).with_process_rss(process_rss)
    }

    #[test]
    fn host_budget_share_is_pinned_to_its_measured_value() {
        assert_eq!(HOST_BUDGET_SHARE_PERCENT, 65);
        assert_eq!(HOST_FIXED_BYTES, 1_563_000_000);
    }

    #[test]
    fn host_derived_claims_the_share_of_available_beyond_the_fixed_part() {
        const GIB: u64 = 1 << 30;
        let budget = HostBudget::host_derived(&probed_facts(3 * GIB, Some(1_000)));
        assert_eq!(budget.bytes(), 1_077_846_556);
        let budget = HostBudget::host_derived(&probed_facts(8 * GIB, Some(1_000)));
        assert_eq!(budget.bytes(), 4_567_507_484);
    }

    #[test]
    fn host_derived_gives_no_budget_at_or_under_the_fixed_part() {
        for available in [0, 1_000_000_000, 1_563_000_000] {
            let budget = HostBudget::host_derived(&probed_facts(available, Some(1_000)));
            assert_eq!(budget.bytes(), 0, "available {available}");
            assert_eq!(budget.provenance(), Provenance::Host);
            assert!(budget.refuses_below_one_tile());
        }
    }

    #[test]
    fn host_derived_claims_the_share_of_the_cgroup_clamped_available() {
        let facts = HostFacts::new(
            64_000_000_000,
            20_000_000_000,
            Some(8_000_000_000),
            Some(5_000_000_000),
            4,
        );
        assert_eq!(HostBudget::host_derived(&facts).bytes(), 2_234_050_000);
    }

    #[test]
    fn host_derived_claims_the_stated_share_of_usable_available() {
        let facts = probed_facts(10_000_000_000, Some(1_000));
        let budget = HostBudget::host_derived(&facts);
        assert_eq!(budget.bytes(), 5_484_050_000);
        assert_eq!(budget.provenance(), Provenance::Host);
        assert!(budget.refuses_below_one_tile());
        assert!(budget.arms_back_off());
        assert_ne!(budget.bytes(), DEFAULT_MAX_BYTES_IN_FLIGHT as u64);
    }

    #[test]
    fn host_derived_falls_back_to_the_stated_default_on_fallback_origin() {
        let budget = HostBudget::host_derived(&HostFacts::fallback());
        assert_eq!(budget.bytes(), DEFAULT_MAX_BYTES_IN_FLIGHT as u64);
        assert_eq!(budget.provenance(), Provenance::HostFallback);
        assert!(!budget.refuses_below_one_tile());
        assert!(!budget.arms_back_off());
    }

    #[test]
    fn explicit_never_refuses_and_never_arms_back_off() {
        let budget = HostBudget::explicit(1);
        assert_eq!(budget.bytes(), 1);
        assert_eq!(budget.provenance(), Provenance::Explicit);
        assert!(!budget.refuses_below_one_tile());
        assert!(!budget.arms_back_off());
    }

    #[test]
    fn capped_to_lowers_when_the_ceiling_is_smaller() {
        let facts = probed_facts(10_000_000_000, Some(1_000));
        let budget = HostBudget::host_derived(&facts).capped_to(Some(1_000));
        assert_eq!(budget.bytes(), 1_000);
        assert_eq!(budget.provenance(), Provenance::Explicit);
    }

    #[test]
    fn capped_to_leaves_a_higher_ceiling_unchanged() {
        let facts = probed_facts(10_000_000_000, Some(1_000));
        let original = HostBudget::host_derived(&facts);
        let capped = original.capped_to(Some(u64::MAX));
        assert_eq!(capped, original);
        assert_eq!(capped.provenance(), Provenance::Host);
    }

    #[test]
    fn capped_to_none_leaves_the_budget_unchanged() {
        let facts = probed_facts(10_000_000_000, Some(1_000));
        let original = HostBudget::host_derived(&facts);
        assert_eq!(original.capped_to(None), original);
    }

    #[test]
    fn memory_bound_maps_provenance_to_free_ram_or_limit() {
        let host = HostBudget::host_derived(&probed_facts(10_000_000_000, Some(1_000)));
        assert_eq!(
            host.memory_bound(),
            MemoryBound::FreeRam {
                share_percent: HOST_BUDGET_SHARE_PERCENT,
                fixed_bytes: HOST_FIXED_BYTES,
                divisor: 1
            }
        );
        let limit = MemoryBound::Limit { divisor: 1 };
        assert_eq!(HostBudget::explicit(5).memory_bound(), limit);
        assert_eq!(
            HostBudget::host_derived(&HostFacts::fallback()).memory_bound(),
            limit
        );
        assert_eq!(host.capped_to(Some(1_000)).memory_bound(), limit);
    }

    #[test]
    fn memory_bound_of_the_cache_share_carries_the_partition_divisor() {
        let host = HostBudget::host_derived(&probed_facts(10_000_000_000, Some(1_000)));
        assert_eq!(
            host.partition_for_decoded_cache().cache_bound(),
            MemoryBound::FreeRam {
                share_percent: HOST_BUDGET_SHARE_PERCENT,
                fixed_bytes: HOST_FIXED_BYTES,
                divisor: 4
            }
        );
        assert_eq!(
            HostBudget::explicit(40)
                .partition_for_decoded_cache()
                .cache_bound(),
            MemoryBound::Limit { divisor: 4 }
        );
    }

    #[test]
    fn reduced_by_keeps_provenance_and_saturates_at_zero() {
        let host = HostBudget::host_derived(&probed_facts(10_000_000_000, Some(1_000)));
        let reduced = host.reduced_by(1_000);
        assert_eq!(reduced.bytes(), host.bytes() - 1_000);
        assert_eq!(reduced.provenance(), Provenance::Host);
        assert!(reduced.arms_back_off());
        assert_eq!(host.reduced_by(u64::MAX).bytes(), 0);
        assert_eq!(
            HostBudget::explicit(10).reduced_by(3).provenance(),
            Provenance::Explicit
        );
    }

    #[test]
    fn back_off_now_rss_half_alone_is_not_enough() {
        // Grown past threshold, but reserved_bytes == bytes_per_tile
        // (not strictly greater): the second half must fail.
        assert!(!back_off_now(Some(2_000), Some(1_000), 1_000, 10, 10));
    }

    #[test]
    fn back_off_now_reserved_half_alone_is_not_enough() {
        // reserved_bytes exceeds one tile, but RSS growth stays under
        // threshold: the first half must fail.
        assert!(!back_off_now(Some(1_100), Some(1_000), 1_000, 20, 10));
    }

    #[test]
    fn back_off_now_fires_when_both_halves_agree() {
        assert!(back_off_now(Some(2_000), Some(1_000), 1_000, 20, 10));
    }

    #[test]
    fn back_off_now_is_false_with_no_rss_signal() {
        assert!(!back_off_now(None, Some(1_000), 1_000, 20, 10));
        assert!(!back_off_now(Some(2_000), None, 1_000, 20, 10));
    }

    /// A [`HostProbe`] double that answers fixed facts and counts its reads.
    struct CountingProbe {
        facts: HostFacts,
        calls: AtomicUsize,
    }

    impl CountingProbe {
        fn new(facts: HostFacts) -> Arc<Self> {
            Arc::new(CountingProbe {
                facts,
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl HostProbe for CountingProbe {
        fn facts(&self) -> HostFacts {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.facts
        }
    }

    fn sources_of(probe: Arc<CountingProbe>) -> HostSources {
        HostSources {
            probe,
            clock: Arc::new(ManualClock::new()),
        }
    }

    /// 64 GB host, `available` free, an optional cgroup limit with
    /// `cgroup_free` left under it.
    fn host_facts(available: u64, cgroup: Option<(u64, u64)>) -> HostFacts {
        HostFacts::new(
            64_000_000_000,
            available,
            cgroup.map(|(limit, _)| limit),
            cgroup.map(|(_, free)| free),
            16,
        )
        .with_process_rss(Some(1_000))
    }

    const TILE: u64 = 100_000_000;
    const WORKING_SET: u64 = 300_000_000;

    #[test]
    fn band_columns_divide_the_remainder_after_the_in_flight_tiles() {
        let facts = host_facts(50_000_000_000, Some((4_000_000_000, 10)));
        // (4.0e9 - 1.563e9 - 6 x 1.0e8) / 3.0e8 = 6.12
        assert_eq!(band_columns(&facts, 6, TILE, WORKING_SET), Some(6));
        // Fewer tiles in flight leave more cache: 2.237e9 / 3.0e8 = 7.46
        assert_eq!(band_columns(&facts, 2, TILE, WORKING_SET), Some(7));
        // Smaller tiles do too: (2.437e9 - 6 x 5.0e7) / 3.0e8 = 7.12
        assert_eq!(band_columns(&facts, 6, 50_000_000, WORKING_SET), Some(7));
        // A larger working set is the divisor: 1.837e9 / 6.0e8 = 3.06
        assert_eq!(band_columns(&facts, 6, TILE, 600_000_000), Some(3));
    }

    #[test]
    fn band_columns_use_available_memory_when_there_is_no_cgroup() {
        // (5.0e9 - 1.563e9 - 4 x 1.0e8) / 3.0e8 = 10.12, whatever the host total is
        let facts = host_facts(5_000_000_000, None);
        assert_eq!(band_columns(&facts, 4, TILE, WORKING_SET), Some(10));
    }

    #[test]
    fn band_columns_read_the_cgroup_limit_not_its_free_memory() {
        // The cgroup's free memory is depressed by the scratch's own cache,
        // and a huge host MemAvailable must not lift the limit either.
        let facts = host_facts(60_000_000_000, Some((4_000_000_000, 1)));
        assert_eq!(band_columns(&facts, 6, TILE, WORKING_SET), Some(6));
    }

    #[test]
    fn band_columns_are_none_without_a_working_set_or_a_probe() {
        let facts = host_facts(50_000_000_000, Some((4_000_000_000, 10)));
        assert_eq!(band_columns(&facts, 6, TILE, 0), None);
        assert_eq!(
            band_columns(&HostFacts::fallback(), 6, TILE, WORKING_SET),
            None
        );
    }

    #[test]
    fn band_columns_floor_at_one_below_the_fixed_part_or_one_working_set() {
        let under_fixed = host_facts(50_000_000_000, Some((1 << 30, 0)));
        assert_eq!(band_columns(&under_fixed, 6, TILE, WORKING_SET), Some(1));
        // 200 MB above the fixed part, one 100 MB tile in flight: 100 MB of
        // cache, less than one working set.
        let barely_over = host_facts(50_000_000_000, Some((HOST_FIXED_BYTES + 200_000_000, 0)));
        assert_eq!(band_columns(&barely_over, 1, TILE, WORKING_SET), Some(1));
        // The in-flight tiles alone exceed the limit.
        assert_eq!(band_columns(&barely_over, 32, TILE, WORKING_SET), Some(1));
        let no_memory = host_facts(0, None);
        assert_eq!(band_columns(&no_memory, 6, TILE, WORKING_SET), Some(1));
    }

    #[test]
    fn page_cache_working_set_leaves_the_in_flight_limit() {
        let facts = host_facts(50_000_000_000, Some((4_000_000_000, 3_000_000_000)));
        let host = HostBudget::host_derived(&facts);
        // (3.0e9 - 1.563e9) * 65 % / 1.0e8 = 9 tiles by the budget alone.
        assert_eq!(in_flight_for_budget(TILE, host.bytes(), 32).0, 9);
        // (name, budget, limit, band columns for that limit)
        for (name, budget, limit, bands) in [
            ("host", host, 9, 5),
            ("explicit", HostBudget::explicit(500_000_000), 5, 6),
            ("capped", host.capped_to(Some(800_000_000)), 8, 5),
        ] {
            let probe = CountingProbe::new(facts);
            let gate = HostBudgetGate::new(budget, TILE, WORKING_SET, 32, sources_of(probe));
            assert_eq!(gate.initial_limit(), limit, "{name}");
            assert_eq!(gate.band_columns(), Some(bands), "{name}");
        }
        // The thread count still bounds the limit, and the bands follow it.
        let roomy = host_facts(100_000_000_000, None);
        let gate = HostBudgetGate::new(
            HostBudget::explicit(u64::MAX),
            TILE,
            WORKING_SET,
            4,
            sources_of(CountingProbe::new(roomy)),
        );
        assert_eq!(gate.initial_limit(), 4);
        // (100e9 - 1.563e9 - 4 x 1.0e8) / 3.0e8 = 326.8
        assert_eq!(gate.band_columns(), Some(326));
    }

    #[test]
    fn band_columns_follow_the_frame_count_through_the_working_set() {
        let facts = host_facts(50_000_000_000, Some((4_000_000_000, 10)));
        // 100 frames x 3 MB against 200 frames x 3 MB per tile, 8 in flight.
        for (frames, expected) in [(100u64, 5usize), (200, 2)] {
            let gate = HostBudgetGate::new(
                HostBudget::explicit(u64::MAX),
                TILE,
                3_000_000 * frames,
                8,
                sources_of(CountingProbe::new(facts)),
            );
            // (2.437e9 - 8 x 1.0e8) / (3e6 * frames): 5.46 and 2.73
            assert_eq!(gate.band_columns(), Some(expected), "{frames} frames");
        }
    }

    #[test]
    fn zero_working_set_keeps_the_budget_limit_and_never_probes_a_fixed_budget() {
        let facts = host_facts(1_000_000_000, Some((1 << 30, 0)));
        for budget in [
            HostBudget::explicit(500_000_000),
            HostBudget::explicit(u64::MAX).capped_to(Some(500_000_000)),
        ] {
            let probe = CountingProbe::new(facts);
            let gate = HostBudgetGate::new(budget, TILE, 0, 32, sources_of(probe.clone()));
            assert_eq!(
                gate.initial_limit(),
                in_flight_for_budget(TILE, 500_000_000, 32).0
            );
            assert_eq!(probe.calls(), 0, "{:?}", budget.provenance());
            assert_eq!(gate.band_columns(), None);
        }
    }

    #[test]
    fn zero_working_set_reads_the_probe_once_for_a_host_budget_as_before() {
        let facts = host_facts(50_000_000_000, None);
        let budget = HostBudget::host_derived(&facts);
        let probe = CountingProbe::new(facts);
        let gate = HostBudgetGate::new(budget, TILE, 0, 32, sources_of(probe.clone()));
        assert_eq!(
            gate.initial_limit(),
            in_flight_for_budget(TILE, budget.bytes(), 32).0
        );
        assert_eq!(probe.calls(), 1);
        assert_eq!(gate.band_columns(), None);
    }

    #[test]
    fn page_cache_working_set_reads_the_probe_once_under_a_host_budget() {
        let facts = host_facts(50_000_000_000, None);
        let probe = CountingProbe::new(facts);
        let gate = HostBudgetGate::new(
            HostBudget::host_derived(&facts),
            TILE,
            WORKING_SET,
            32,
            sources_of(probe.clone()),
        );
        assert!(gate.initial_limit() >= 1);
        assert_eq!(
            probe.calls(),
            1,
            "one read serves the band width and the back-off start"
        );
    }

    /// A [`HostProbe`] double that plays back a fixed script of facts:
    /// advances one entry per call, then repeats its last entry forever.
    struct ScriptedProbe {
        facts: StdMutex<Vec<HostFacts>>,
    }

    impl ScriptedProbe {
        fn new(facts: Vec<HostFacts>) -> Self {
            assert!(!facts.is_empty());
            ScriptedProbe {
                facts: StdMutex::new(facts),
            }
        }
    }

    impl HostProbe for ScriptedProbe {
        fn facts(&self) -> HostFacts {
            let mut facts = self.facts.lock().unwrap();
            if facts.len() > 1 {
                facts.remove(0)
            } else {
                facts[0]
            }
        }
    }

    /// Drives the gate through a scripted rising-then-recovering RSS signal
    /// under an explicit `ManualClock`. The workload is modelled by two
    /// permits held throughout (`reserved_bytes = 2 * 1 = 2 >
    /// bytes_per_tile = 1`, the "this pressure is ours" half of
    /// [`back_off_now`]); every poll-triggering acquire is released right
    /// after. The final shrink (4 -> 2) brings the limit down to exactly the
    /// two held baseline permits, so the call that causes it cannot itself
    /// be admitted until one baseline permit is released -- proven by running
    /// it on a background thread and joining it to confirm it returns.
    #[test]
    fn gate_shrinks_after_two_confirming_polls_and_never_raises_on_recovery() {
        let script = vec![
            probed_facts(1_000, Some(1_000)), // consumed at construction
            probed_facts(1_000, Some(2_000)), // poll 1: hit, 1/2 confirmations
            probed_facts(1_000, Some(2_500)), // poll 2: hit, shrinks 8 -> 4
            probed_facts(1_000, Some(1_000)), // poll 3: recovers, no re-raise
            probed_facts(1_000, Some(2_500)), // poll 4: hit, 1/2 confirmations
            probed_facts(1_000, Some(2_500)), // poll 5: hit, shrinks 4 -> 2
        ];
        let probe = Arc::new(ScriptedProbe::new(script));
        let clock = Arc::new(ManualClock::new());
        let sources = HostSources {
            probe,
            clock: clock.clone(),
        };

        let facts = probed_facts(HOST_FIXED_BYTES + 1_000, Some(1_000));
        let budget = HostBudget::host_derived(&facts);
        let gate = Arc::new(HostBudgetGate::new(budget, 1, 0, 8, sources));
        assert_eq!(gate.initial_limit(), 8);

        // Baseline workload: two permits held for most of the test, at
        // clock=0, so their own polls are skipped by the cadence check
        // (elapsed 0 < PRESSURE_POLL_INTERVAL_MS) rather than consuming the
        // script.
        let held_a = gate.acquire(&NoControl);
        let held_b = gate.acquire(&NoControl);
        assert_eq!(gate.limit(), 8);

        // Poll 1: hit, but only the first of two confirmations.
        clock.advance(PRESSURE_POLL_INTERVAL_MS);
        drop(gate.acquire(&NoControl));
        assert_eq!(gate.limit(), 8, "a single confirming poll must not shrink");

        // Poll 2: hit again, shrinks 8 -> 4.
        clock.advance(PRESSURE_POLL_INTERVAL_MS);
        drop(gate.acquire(&NoControl));
        assert_eq!(gate.limit(), 4);
        assert!(gate.backed_off());

        // Poll 3: recovers (rss back to start); must not raise the limit.
        clock.advance(PRESSURE_POLL_INTERVAL_MS);
        drop(gate.acquire(&NoControl));
        assert_eq!(
            gate.limit(),
            4,
            "a later recovery must leave the shrunk limit where it is"
        );

        // Poll 4: hit, first of two confirmations again.
        clock.advance(PRESSURE_POLL_INTERVAL_MS);
        drop(gate.acquire(&NoControl));
        assert_eq!(gate.limit(), 4);

        // Poll 5: the confirming hit that shrinks 4 -> 2 -- exactly the two
        // permits already held. Run it on a background thread: it must
        // block in `acquire`'s own admission wait until one baseline permit
        // is released.
        clock.advance(PRESSURE_POLL_INTERVAL_MS);
        let gate_bg = gate.clone();
        let handle = std::thread::spawn(move || {
            drop(gate_bg.acquire(&NoControl));
        });

        // Wait for the background thread's own poll to have performed the
        // shrink -- observable via `limit()` -- before releasing a baseline
        // permit, so its `reserved_bytes` reading is guaranteed to have
        // seen both baseline permits still held rather than racing a drop.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while gate.limit() != 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "the background acquirer's poll never observed a shrink to 2"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        drop(held_b);
        handle
            .join()
            .expect("the spawned acquirer must return, not deadlock");

        assert_eq!(gate.limit(), 2);
        assert!(gate.backed_off());

        drop(held_a);
    }

    #[test]
    fn explicit_budget_never_polls_or_shrinks() {
        let probe = Arc::new(ScriptedProbe::new(vec![probed_facts(1_000, Some(999_999))]));
        let clock = Arc::new(ManualClock::new());
        let sources = HostSources {
            probe,
            clock: clock.clone(),
        };
        let gate = HostBudgetGate::new(HostBudget::explicit(100), 1, 0, 8, sources);
        assert_eq!(gate.initial_limit(), 8);
        clock.advance(10 * PRESSURE_POLL_INTERVAL_MS);
        for _ in 0..10 {
            drop(gate.acquire(&NoControl));
        }
        assert_eq!(gate.limit(), 8);
        assert!(!gate.backed_off());
    }

    #[test]
    fn concurrent_acquirers_all_complete_and_respect_the_limit() {
        let gate = Arc::new(HostBudgetGate::new(
            HostBudget::explicit(1),
            1,
            0,
            3,
            HostSources::default(),
        ));
        let mut handles = Vec::new();
        for _ in 0..12 {
            let gate = gate.clone();
            handles.push(std::thread::spawn(move || {
                let _permit = gate.acquire(&NoControl);
                std::thread::sleep(std::time::Duration::from_millis(2));
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert!(gate.peak_in_flight() <= gate.initial_limit());
        assert!(gate.peak_in_flight() >= 1);
    }

    /// A control whose admission cap a test sets and lifts while acquirers
    /// are already waiting.
    struct CapControl(AtomicUsize);

    impl CapControl {
        fn new(cap: usize) -> Self {
            CapControl(AtomicUsize::new(cap))
        }
    }

    impl TileControl for CapControl {
        fn should_stop(&self) -> bool {
            false
        }

        fn admission_cap(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "timed out: {what}");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn a_cap_below_the_limit_bounds_in_flight_and_lifting_it_restores_admission() {
        let gate = Arc::new(HostBudgetGate::new(
            HostBudget::explicit(6),
            1,
            0,
            6,
            HostSources::default(),
        ));
        assert_eq!(gate.initial_limit(), 6);
        let control = Arc::new(CapControl::new(2));
        let admitted = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let (gate, control, admitted, release) = (
                    gate.clone(),
                    control.clone(),
                    admitted.clone(),
                    release.clone(),
                );
                std::thread::spawn(move || {
                    let _permit = gate.acquire(&*control);
                    admitted.fetch_add(1, Ordering::SeqCst);
                    while !release.load(Ordering::SeqCst) {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                })
            })
            .collect();

        wait_until("two acquirers admitted under the cap", || {
            admitted.load(Ordering::SeqCst) >= 2
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(
            admitted.load(Ordering::SeqCst),
            2,
            "a cap of 2 must hold the other four waiting"
        );
        assert_eq!(gate.peak_in_flight(), 2);

        // No notify: the gate must notice the lifted cap on its own.
        control.0.store(usize::MAX, Ordering::SeqCst);
        wait_until("all six admitted once the cap lifts", || {
            admitted.load(Ordering::SeqCst) == 6
        });
        assert_eq!(gate.peak_in_flight(), 6);

        release.store(true, Ordering::SeqCst);
        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn a_cap_never_raises_a_shrunk_limit() {
        let script = vec![
            probed_facts(1_000, Some(1_000)), // consumed at construction
            probed_facts(1_000, Some(2_000)), // poll 1: hit, 1/2 confirmations
            probed_facts(1_000, Some(2_500)), // poll 2: hit, shrinks 8 -> 4
            probed_facts(1_000, Some(2_500)), // poll 3: hit, 1/2 confirmations
            probed_facts(1_000, Some(2_500)), // poll 4: hit, shrinks 4 -> 2
        ];
        let clock = Arc::new(ManualClock::new());
        let sources = HostSources {
            probe: Arc::new(ScriptedProbe::new(script)),
            clock: clock.clone(),
        };
        let budget = HostBudget::host_derived(&probed_facts(HOST_FIXED_BYTES + 1_000, Some(1_000)));
        let gate = Arc::new(HostBudgetGate::new(budget, 1, 0, 8, sources));
        assert_eq!(gate.initial_limit(), 8);
        let control = Arc::new(CapControl::new(4));

        let held_a = gate.acquire(&*control);
        let held_b = gate.acquire(&*control);

        clock.advance(PRESSURE_POLL_INTERVAL_MS);
        drop(gate.acquire(&*control));
        assert_eq!(gate.limit(), 8);
        clock.advance(PRESSURE_POLL_INTERVAL_MS);
        drop(gate.acquire(&*control));
        assert_eq!(gate.limit(), 4, "back-off halves while a cap is set");
        clock.advance(PRESSURE_POLL_INTERVAL_MS);
        drop(gate.acquire(&*control));
        assert_eq!(gate.limit(), 4);

        // Shrinking to the two permits already held: this acquirer waits
        // until one of them is released.
        clock.advance(PRESSURE_POLL_INTERVAL_MS);
        let shrinker = {
            let (gate, control) = (gate.clone(), control.clone());
            std::thread::spawn(move || drop(gate.acquire(&*control)))
        };
        wait_until("the shrink to 2", || gate.limit() == 2);
        drop(held_b);
        shrinker.join().unwrap();
        drop(held_a);

        // The cap of 4 sits above the shrunk limit and must not raise it.
        let live = Arc::new(AtomicUsize::new(0));
        let widest = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (gate, control, live, widest) =
                    (gate.clone(), control.clone(), live.clone(), widest.clone());
                std::thread::spawn(move || {
                    let _permit = gate.acquire(&*control);
                    let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                    widest.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    live.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(gate.limit(), 2);
        assert!(widest.load(Ordering::SeqCst) <= 2);
    }
}
