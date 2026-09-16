// SPDX-License-Identifier: GPL-3.0-or-later

//! Wall-clock stage timing, carried out of `core` as data exactly as
//! [`crate::tile::scheduler::TileRunReport`] already is. `core` never
//! prints a number from this module -- only `crates/cli` renders it, in
//! `stack --json`/`align --json` and their human-text twins.
//!
//! # What a number here measures
//!
//! Every figure is **wall clock, in seconds**, from one process's own
//! [`std::time::Instant`] -- never CPU time; CPU-time attribution stays in
//! `/usr/bin/time -v` output outside this schema. The five [`Stage`]s a
//! [`StageTimer`] tracks are **contiguous and disjoint by construction**:
//! each `mark` closes the span since the previous `mark` (or since `new()`)
//! and opens the next, so the non-`None` figures a finished
//! [`StageTimings`] carries always sum, exactly, to the run's own total
//! wall clock (`StageTimer::total_seconds`) -- there is no unattributed
//! remainder to name.
//!
//! A stage that is itself parallel (`fuse`, over the tile scheduler's own
//! rayon pool, `crate::tile::scheduler`) reports the wall-clock span of
//! that fan-out, not the CPU time spent inside it -- read it beside
//! `in_flight_limit`/`peak_in_flight`, already in the same report, never as
//! a claim about how much work happened. A per-tile read/fuse/write split
//! does not exist as a clean seam -- those three
//! interleave inside one parallel tile loop by design
//! (`crate::tile::scheduler::TileScheduler::run`) -- so the five true
//! sequential seams (setup / align / prepare / fuse / finalize, with
//! `prepare` folded into `fuse`) are what this module measures, no finer.
//!
//! # `None` versus `Some(0.0)`
//!
//! A stage never entered on a given run (`fuse` on a plain `align` run,
//! `warp_write` absent `--write-warped`, `align` under `--no-align`)
//! serializes as an explicit JSON `null`, distinguishable from a stage that
//! was entered and measured at (as close to) no time at all
//! (`Some(0.0)`). `StageTimings` derives `Serialize` with **no**
//! `#[serde(skip_serializing_if)]` for exactly this reason: every key is
//! always present on the wire, `null` or a number, never absent.

use std::time::Instant;

use serde::Serialize;

/// One run's named phases, deliberately its own vocabulary -- not
/// [`crate::job::StageCache`]'s `StageKey` (a cache-key type, not a timing
/// one) and not [`crate::job::JobStage`] (`#[non_exhaustive]`, the job
/// engine's own control/ledger vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Everything before alignment: opening the stack, resolving
    /// parameters, the fingerprint and stage-cache lookups.
    Setup,
    /// Producing this run's [`crate::align::StackAlignment`] -- computed,
    /// reused from `--resume`'s ledger peek, reused from the stage cache,
    /// or never entered at all under `--no-align`.
    Align,
    /// Scheduler build, staging, ledger open and
    /// `FusionConsumer::create`/`resume`, plus `job::drive`'s own fan-out
    /// over the tile scheduler's rayon pool.
    Fuse,
    /// `focusforge-cli align --write-warped`'s per-frame warped-TIFF dump.
    /// Never entered by `stack`, and never entered by a plain `align`
    /// without the flag.
    WarpWrite,
    /// Sealing the project, measuring review metrics, discarding the
    /// resume ledger, assembling the report.
    Finalize,
}

/// Accumulates wall-clock seconds per [`Stage`] from one monotonic clock,
/// the same `Instant`-epoch shape [`crate::job::control::JobControl`]
/// already uses.
///
/// `mark(stage)` closes the span since the previous `mark` (or since
/// `new()`) and credits it to `stage` -- marking the same stage twice
/// **sums** the two spans rather than overwriting the first. Exactly one
/// clock read at construction and exactly one per `mark` call:
/// [`StageTimer::clock_reads`] is public so that bound is an assertion a
/// test can make, not a claim left undemonstrated -- the count tracks
/// marks, never the tiles or frames a stage happens to process.
pub struct StageTimer {
    started: Instant,
    last: Instant,
    clock_reads: u32,
    setup: f64,
    align: f64,
    fuse: f64,
    warp_write: f64,
    finalize: f64,
    setup_entered: bool,
    align_entered: bool,
    fuse_entered: bool,
    warp_write_entered: bool,
    finalize_entered: bool,
}

impl StageTimer {
    /// Starts the timer, reading the clock exactly once.
    pub fn new() -> Self {
        Self::from_start(Instant::now())
    }

    /// Starts the timer from an `Instant` the caller already holds --
    /// `crates/cli`'s own run-start clock read, in every driver -- so
    /// building a timer never costs a second clock read of its own, and a
    /// run's `wall_seconds` (computed from that same `Instant` on every
    /// path, including `failed`/`cancelled`) and its stage breakdown are
    /// exactly the same clock.
    pub fn from_start(started: Instant) -> Self {
        StageTimer {
            started,
            last: started,
            clock_reads: 1,
            setup: 0.0,
            align: 0.0,
            fuse: 0.0,
            warp_write: 0.0,
            finalize: 0.0,
            setup_entered: false,
            align_entered: false,
            fuse_entered: false,
            warp_write_entered: false,
            finalize_entered: false,
        }
    }

    /// Closes the span since the last mark (or since [`Self::new`]) and
    /// credits it to `stage`. Reads the clock exactly once.
    pub fn mark(&mut self, stage: Stage) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.clock_reads += 1;
        match stage {
            Stage::Setup => {
                self.setup += elapsed;
                self.setup_entered = true;
            }
            Stage::Align => {
                self.align += elapsed;
                self.align_entered = true;
            }
            Stage::Fuse => {
                self.fuse += elapsed;
                self.fuse_entered = true;
            }
            Stage::WarpWrite => {
                self.warp_write += elapsed;
                self.warp_write_entered = true;
            }
            Stage::Finalize => {
                self.finalize += elapsed;
                self.finalize_entered = true;
            }
        }
    }

    /// How many times this timer has read the clock: one at [`Self::new`]
    /// plus one per [`Self::mark`] call. A function of stage count alone,
    /// never of tiles or frames.
    pub fn clock_reads(&self) -> u32 {
        self.clock_reads
    }

    /// The wall clock elapsed from [`Self::new`] to the most recent
    /// [`Self::mark`] (or to `new()` itself, if never marked). This is what
    /// a caller assigns to its own `wall_seconds` field, so the accounting
    /// identity -- non-`None` stages sum to `wall_seconds` -- is exact
    /// rather than tolerated.
    pub fn total_seconds(&self) -> f64 {
        self.last
            .saturating_duration_since(self.started)
            .as_secs_f64()
    }

    /// Consumes the timer, producing the frozen, serializable breakdown. A
    /// stage never marked on this run comes out `None` -- distinguishable
    /// on the wire from a stage measured at zero seconds.
    pub fn finish(self) -> StageTimings {
        StageTimings {
            setup: self.setup_entered.then_some(self.setup),
            align: self.align_entered.then_some(self.align),
            fuse: self.fuse_entered.then_some(self.fuse),
            warp_write: self.warp_write_entered.then_some(self.warp_write),
            finalize: self.finalize_entered.then_some(self.finalize),
        }
    }
}

impl Default for StageTimer {
    fn default() -> Self {
        Self::new()
    }
}

/// One run's stage breakdown, wall-clock seconds -- see the module
/// documentation for the unit, the disjoint-contiguous contract and what
/// `None` means. Additive-only on every schema that carries it
/// (`StackReport`, `AlignReport`): a future stage is a new field, never a
/// reinterpretation of one of these five.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct StageTimings {
    pub setup: Option<f64>,
    pub align: Option<f64>,
    pub fuse: Option<f64>,
    pub warp_write: Option<f64>,
    pub finalize: Option<f64>,
}
