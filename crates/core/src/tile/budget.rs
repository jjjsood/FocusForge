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
use crate::tile::scheduler::{DEFAULT_MAX_BYTES_IN_FLIGHT, in_flight_for_budget};

/// The share of `HostFacts::usable_available()` a host-derived budget
/// claims. Frame tiles are not the process's only live bytes -- at
/// fixture scale a 2 MiB budget measures under a 5 MiB peak live heap once
/// the fusion consumer's per-worker pyramids are counted, roughly 2.5x, so
/// claiming 40% of what the host currently reports available keeps the
/// modelled peak at or under it.
pub const HOST_BUDGET_SHARE_PERCENT: u64 = 40;

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
/// arms the runtime back-off -- an explicit or flag-capped budget never
/// probes at all, keeping every existing test deterministic).
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

impl HostBudget {
    /// Derives a budget from host facts: [`HOST_BUDGET_SHARE_PERCENT`] of
    /// what the host currently reports available when the facts were
    /// actually probed, or the stated [`DEFAULT_MAX_BYTES_IN_FLIGHT`]
    /// constant -- never deleted, the degraded answer -- when the probe was
    /// unavailable.
    pub fn host_derived(facts: &HostFacts) -> Self {
        match facts.origin() {
            FactsOrigin::Probed => HostBudget {
                bytes: facts.usable_available() * HOST_BUDGET_SHARE_PERCENT / 100,
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

    /// A caller-supplied budget: never probes, always floors at 1 tile
    /// rather than refusing.
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
        let cache_bytes = self.bytes / 4;
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
    /// budget (including one produced by [`Self::capped_to`]) never probes,
    /// keeping every test that passes a fixed budget deterministic.
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
    pub fn new(budget: HostBudget, bytes_per_tile: u64, cap: usize, sources: HostSources) -> Self {
        let initial_limit = in_flight_for_budget(bytes_per_tile, budget.bytes(), cap);
        let arms_back_off = budget.arms_back_off();
        // Only a host-derived, uncapped budget probes at all -- an
        // `explicit`/flag-capped budget never reads `sources.probe`, so a
        // test that supplies no probe worth calling stays deterministic.
        let (rss_at_start, available_at_start) = if arms_back_off {
            let facts = sources.probe.facts();
            (facts.process_rss(), facts.usable_available())
        } else {
            (None, 0)
        };
        HostBudgetGate {
            state: Mutex::new(GateState {
                in_flight: 0,
                limit: initial_limit,
            }),
            condvar: Condvar::new(),
            initial_limit,
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
    fn host_derived_claims_the_stated_share_of_usable_available() {
        let facts = probed_facts(10_000_000, Some(1_000));
        let budget = HostBudget::host_derived(&facts);
        assert_eq!(budget.bytes(), 10_000_000 * HOST_BUDGET_SHARE_PERCENT / 100);
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
        let facts = probed_facts(10_000_000, Some(1_000));
        let budget = HostBudget::host_derived(&facts).capped_to(Some(1_000));
        assert_eq!(budget.bytes(), 1_000);
        assert_eq!(budget.provenance(), Provenance::Explicit);
    }

    #[test]
    fn capped_to_leaves_a_higher_ceiling_unchanged() {
        let facts = probed_facts(10_000_000, Some(1_000));
        let original = HostBudget::host_derived(&facts);
        let capped = original.capped_to(Some(u64::MAX));
        assert_eq!(capped, original);
        assert_eq!(capped.provenance(), Provenance::Host);
    }

    #[test]
    fn capped_to_none_leaves_the_budget_unchanged() {
        let facts = probed_facts(10_000_000, Some(1_000));
        let original = HostBudget::host_derived(&facts);
        assert_eq!(original.capped_to(None), original);
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

        let facts = probed_facts(1_000, Some(1_000));
        let budget = HostBudget::host_derived(&facts);
        let gate = Arc::new(HostBudgetGate::new(budget, 1, 8, sources));
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
        let gate = HostBudgetGate::new(HostBudget::explicit(100), 1, 8, sources);
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
        let budget = HostBudget::host_derived(&probed_facts(1_000, Some(1_000)));
        let gate = Arc::new(HostBudgetGate::new(budget, 1, 8, sources));
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
