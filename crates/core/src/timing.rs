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

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use serde::Serialize;

/// The fixed number of detailed trace-span admissions one process may emit.
const RECORD_BUDGET: usize = 262_144;

/// A measured quantity produced at a named allocation, copy, or processing
/// boundary. These counters describe only the sites that report them.
#[derive(Debug, Clone, Copy)]
pub enum TimingCounter {
    /// Decoded `u16` payload written into a caller buffer, not disk I/O.
    ConvertedBytes,
    WarpBytes,
    AllocationBytes,
    AllocationOperations,
    CopyBytes,
    CopyOperations,
    WorkerLimit,
    PeakWorkers,
    /// Capture-time metadata reads for image rows.
    MetadataReads,
    /// Rows passed to a GUI model.
    RowUpdates,
    /// RGBA thumbnail bytes copied into GUI images.
    ThumbnailBytes,
    /// Job timer polls reaching a live window.
    JobPolls,
    /// Decoded segment cache hits.
    CacheHits,
    /// Decoded segment cache misses.
    CacheMisses,
    /// Decoded segment sample bytes produced on cache misses.
    DecodedBytes,
    /// Sample bytes allocated for canvas redraws.
    SampleBytes,
    /// Packed display-source bytes.
    PackedBytes,
    /// Bytes written to GPU buffers from the CPU.
    UploadBytes,
    /// Compute pipelines created by the CPU.
    PipelineCreations,
    /// CPU queue submissions.
    CpuSubmissions,
    /// GPU buffers created by the CPU.
    BufferCreations,
    /// Bytes copied from GPU buffers back to the host.
    ReadbackBytes,
    /// Registrations whose level-0 result left the bounded frame window the
    /// solve read.
    Level0WindowMisses,
    /// Diverged registrations that ran the feature detector for a seeded re-run.
    FeatureSeedAttempts,
    /// Feature-seeded re-runs whose outcome replaced the identity-start one.
    FeatureSeedsAccepted,
}

impl TimingCounter {
    fn name(self) -> &'static str {
        match self {
            Self::ConvertedBytes => "converted_bytes",
            Self::WarpBytes => "warp_bytes",
            Self::AllocationBytes => "allocation_bytes",
            Self::AllocationOperations => "allocation_operations",
            Self::CopyBytes => "copy_bytes",
            Self::CopyOperations => "copy_operations",
            Self::WorkerLimit => "worker_limit",
            Self::PeakWorkers => "peak_workers",
            Self::MetadataReads => "metadata_reads",
            Self::RowUpdates => "row_updates",
            Self::ThumbnailBytes => "thumbnail_bytes",
            Self::JobPolls => "job_polls",
            Self::CacheHits => "cache_hits",
            Self::CacheMisses => "cache_misses",
            Self::DecodedBytes => "decoded_bytes",
            Self::SampleBytes => "sample_bytes",
            Self::PackedBytes => "packed_bytes",
            Self::UploadBytes => "upload_bytes",
            Self::PipelineCreations => "pipeline_creations",
            Self::CpuSubmissions => "cpu_submissions",
            Self::BufferCreations => "buffer_creations",
            Self::ReadbackBytes => "readback_bytes",
            Self::Level0WindowMisses => "level0_window_misses",
            Self::FeatureSeedAttempts => "feature_seed_attempts",
            Self::FeatureSeedsAccepted => "feature_seeds_accepted",
        }
    }
}

/// The bounded accounting summary for one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingSnapshot {
    pub admitted: u64,
    pub exhausted: u64,
    pub converted_bytes: u64,
    pub warp_bytes: u64,
    pub allocation_bytes: u64,
    pub allocation_operations: u64,
    pub copy_bytes: u64,
    pub copy_operations: u64,
    pub worker_limit: u64,
    pub peak_workers: u64,
    pub metadata_reads: u64,
    pub row_updates: u64,
    pub thumbnail_bytes: u64,
    pub job_polls: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub decoded_bytes: u64,
    pub sample_bytes: u64,
    pub packed_bytes: u64,
    pub upload_bytes: u64,
    pub pipeline_creations: u64,
    pub cpu_submissions: u64,
    pub buffer_creations: u64,
    pub readback_bytes: u64,
    pub level0_window_misses: u64,
    pub feature_seed_attempts: u64,
    pub feature_seeds_accepted: u64,
    pub gpu_execution_available: bool,
    pub presentation_available: bool,
    pub saturated: bool,
}

struct TimingState {
    remaining: AtomicUsize,
    admitted: AtomicU64,
    exhausted: AtomicU64,
    converted_bytes: AtomicU64,
    warp_bytes: AtomicU64,
    allocation_bytes: AtomicU64,
    allocation_operations: AtomicU64,
    copy_bytes: AtomicU64,
    copy_operations: AtomicU64,
    worker_limit: AtomicU64,
    peak_workers: AtomicU64,
    metadata_reads: AtomicU64,
    row_updates: AtomicU64,
    thumbnail_bytes: AtomicU64,
    job_polls: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    decoded_bytes: AtomicU64,
    sample_bytes: AtomicU64,
    packed_bytes: AtomicU64,
    upload_bytes: AtomicU64,
    pipeline_creations: AtomicU64,
    cpu_submissions: AtomicU64,
    buffer_creations: AtomicU64,
    readback_bytes: AtomicU64,
    level0_window_misses: AtomicU64,
    feature_seed_attempts: AtomicU64,
    feature_seeds_accepted: AtomicU64,
    saturated: AtomicBool,
    summary_emitted: AtomicBool,
}

impl TimingState {
    fn new(budget: usize) -> Self {
        Self {
            remaining: AtomicUsize::new(budget),
            admitted: AtomicU64::new(0),
            exhausted: AtomicU64::new(0),
            converted_bytes: AtomicU64::new(0),
            warp_bytes: AtomicU64::new(0),
            allocation_bytes: AtomicU64::new(0),
            allocation_operations: AtomicU64::new(0),
            copy_bytes: AtomicU64::new(0),
            copy_operations: AtomicU64::new(0),
            worker_limit: AtomicU64::new(0),
            peak_workers: AtomicU64::new(0),
            metadata_reads: AtomicU64::new(0),
            row_updates: AtomicU64::new(0),
            thumbnail_bytes: AtomicU64::new(0),
            job_polls: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            decoded_bytes: AtomicU64::new(0),
            sample_bytes: AtomicU64::new(0),
            packed_bytes: AtomicU64::new(0),
            upload_bytes: AtomicU64::new(0),
            pipeline_creations: AtomicU64::new(0),
            cpu_submissions: AtomicU64::new(0),
            buffer_creations: AtomicU64::new(0),
            readback_bytes: AtomicU64::new(0),
            level0_window_misses: AtomicU64::new(0),
            feature_seed_attempts: AtomicU64::new(0),
            feature_seeds_accepted: AtomicU64::new(0),
            saturated: AtomicBool::new(false),
            summary_emitted: AtomicBool::new(false),
        }
    }

    fn admit(&self) -> bool {
        if self
            .remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            saturating_add(&self.admitted, 1, &self.saturated);
            true
        } else {
            saturating_add(&self.exhausted, 1, &self.saturated);
            false
        }
    }

    fn add(&self, counter: TimingCounter, value: u64) {
        let slot = match counter {
            TimingCounter::ConvertedBytes => &self.converted_bytes,
            TimingCounter::WarpBytes => &self.warp_bytes,
            TimingCounter::AllocationBytes => &self.allocation_bytes,
            TimingCounter::AllocationOperations => &self.allocation_operations,
            TimingCounter::CopyBytes => &self.copy_bytes,
            TimingCounter::CopyOperations => &self.copy_operations,
            TimingCounter::WorkerLimit => &self.worker_limit,
            TimingCounter::PeakWorkers => &self.peak_workers,
            TimingCounter::MetadataReads => &self.metadata_reads,
            TimingCounter::RowUpdates => &self.row_updates,
            TimingCounter::ThumbnailBytes => &self.thumbnail_bytes,
            TimingCounter::JobPolls => &self.job_polls,
            TimingCounter::CacheHits => &self.cache_hits,
            TimingCounter::CacheMisses => &self.cache_misses,
            TimingCounter::DecodedBytes => &self.decoded_bytes,
            TimingCounter::SampleBytes => &self.sample_bytes,
            TimingCounter::PackedBytes => &self.packed_bytes,
            TimingCounter::UploadBytes => &self.upload_bytes,
            TimingCounter::PipelineCreations => &self.pipeline_creations,
            TimingCounter::CpuSubmissions => &self.cpu_submissions,
            TimingCounter::BufferCreations => &self.buffer_creations,
            TimingCounter::ReadbackBytes => &self.readback_bytes,
            TimingCounter::Level0WindowMisses => &self.level0_window_misses,
            TimingCounter::FeatureSeedAttempts => &self.feature_seed_attempts,
            TimingCounter::FeatureSeedsAccepted => &self.feature_seeds_accepted,
        };
        saturating_add(slot, value, &self.saturated);
    }

    fn observe_max(&self, counter: TimingCounter, value: u64) {
        let slot = match counter {
            TimingCounter::WorkerLimit => &self.worker_limit,
            TimingCounter::PeakWorkers => &self.peak_workers,
            _ => {
                self.add(counter, value);
                return;
            }
        };
        slot.fetch_max(value, Ordering::Relaxed);
    }

    fn snapshot(&self) -> TimingSnapshot {
        TimingSnapshot {
            admitted: self.admitted.load(Ordering::Relaxed),
            exhausted: self.exhausted.load(Ordering::Relaxed),
            converted_bytes: self.converted_bytes.load(Ordering::Relaxed),
            warp_bytes: self.warp_bytes.load(Ordering::Relaxed),
            allocation_bytes: self.allocation_bytes.load(Ordering::Relaxed),
            allocation_operations: self.allocation_operations.load(Ordering::Relaxed),
            copy_bytes: self.copy_bytes.load(Ordering::Relaxed),
            copy_operations: self.copy_operations.load(Ordering::Relaxed),
            worker_limit: self.worker_limit.load(Ordering::Relaxed),
            peak_workers: self.peak_workers.load(Ordering::Relaxed),
            metadata_reads: self.metadata_reads.load(Ordering::Relaxed),
            row_updates: self.row_updates.load(Ordering::Relaxed),
            thumbnail_bytes: self.thumbnail_bytes.load(Ordering::Relaxed),
            job_polls: self.job_polls.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            decoded_bytes: self.decoded_bytes.load(Ordering::Relaxed),
            sample_bytes: self.sample_bytes.load(Ordering::Relaxed),
            packed_bytes: self.packed_bytes.load(Ordering::Relaxed),
            upload_bytes: self.upload_bytes.load(Ordering::Relaxed),
            pipeline_creations: self.pipeline_creations.load(Ordering::Relaxed),
            cpu_submissions: self.cpu_submissions.load(Ordering::Relaxed),
            buffer_creations: self.buffer_creations.load(Ordering::Relaxed),
            readback_bytes: self.readback_bytes.load(Ordering::Relaxed),
            level0_window_misses: self.level0_window_misses.load(Ordering::Relaxed),
            feature_seed_attempts: self.feature_seed_attempts.load(Ordering::Relaxed),
            feature_seeds_accepted: self.feature_seeds_accepted.load(Ordering::Relaxed),
            gpu_execution_available: false,
            presentation_available: false,
            saturated: self.saturated.load(Ordering::Relaxed),
        }
    }

    fn begin_summary(&self) -> bool {
        !self.summary_emitted.swap(true, Ordering::AcqRel)
    }
}

fn saturating_add(slot: &AtomicU64, value: u64, saturated: &AtomicBool) {
    let _ = slot.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        if let Some(sum) = current.checked_add(value) {
            Some(sum)
        } else {
            saturated.store(true, Ordering::Relaxed);
            Some(u64::MAX)
        }
    });
}

static TIMING: OnceLock<TimingState> = OnceLock::new();

/// Enables bounded timing records after a diagnostics trace sink is ready.
pub fn enable_trace_accounting() {
    let _ = TIMING.get_or_init(|| TimingState::new(RECORD_BUDGET));
}

fn timing() -> Option<&'static TimingState> {
    TIMING.get()
}

/// Whether bounded timing accounting is active for this process.
pub fn timing_enabled() -> bool {
    timing().is_some()
}

/// A bounded trace span for a measured processing boundary.
#[must_use]
pub struct MeasurementScope(Option<tracing::span::EnteredSpan>);

impl Drop for MeasurementScope {
    fn drop(&mut self) {
        let _ = self.0.take();
    }
}

/// Opens one measured processing boundary when bounded accounting is active.
pub fn measurement_scope(kind: &'static str) -> MeasurementScope {
    let Some(state) = timing() else {
        return MeasurementScope(None);
    };
    if !state.admit() {
        return MeasurementScope(None);
    }
    MeasurementScope(Some(tracing::debug_span!("timing", kind).entered()))
}

/// Opens one bounded measured boundary with scalar correlation fields.
pub fn measurement_scope_with_fields(
    kind: &'static str,
    first: u64,
    last: u64,
    count: u64,
    coalesced: u64,
    incomplete: bool,
    publication: u64,
) -> MeasurementScope {
    let Some(state) = timing() else {
        return MeasurementScope(None);
    };
    if !state.admit() {
        return MeasurementScope(None);
    }
    MeasurementScope(Some(
        tracing::debug_span!(
            "timing",
            kind,
            first,
            last,
            count,
            coalesced,
            incomplete,
            publication
        )
        .entered(),
    ))
}

/// Adds an actual count at a named processing boundary.
pub fn record_timing(counter: TimingCounter, value: u64) {
    let Some(state) = timing() else {
        return;
    };
    state.add(counter, value);
    if state.admit() {
        let _record =
            tracing::debug_span!("timing_count", counter = counter.name(), value).entered();
    }
}

/// Records the solver iterations one pyramid level spent and how the level
/// ended, as a trace span nested under the frame being solved.
pub fn record_level_iterations(level: u32, iterations: u32, status: &str) {
    let Some(state) = timing() else {
        return;
    };
    if state.admit() {
        let _record = tracing::debug_span!("timing_level", level, iterations, status).entered();
    }
}

/// Records the largest observed worker value for a processing run.
pub fn observe_timing_max(counter: TimingCounter, value: u64) {
    let Some(state) = timing() else {
        return;
    };
    state.observe_max(counter, value);
    if state.admit() {
        let _record =
            tracing::debug_span!("timing_maximum", counter = counter.name(), value).entered();
    }
}

/// Returns the bounded accounting state when tracing enabled it.
pub fn timing_snapshot() -> Option<TimingSnapshot> {
    timing().map(TimingState::snapshot)
}

/// Marks a locally aggregated timing counter as saturated.
pub fn mark_timing_saturated() {
    if let Some(state) = timing() {
        state.saturated.store(true, Ordering::Relaxed);
    }
}

/// Emits the one fixed accounting packet before a diagnostics trace closes.
pub fn emit_timing_summary() {
    let Some(state) = timing() else {
        return;
    };
    if !state.begin_summary() {
        return;
    }
    let snapshot = state.snapshot();
    let _summary = tracing::debug_span!(
        "timing_summary",
        admission_budget = RECORD_BUDGET,
        admitted_spans = snapshot.admitted,
        exhausted_admissions = snapshot.exhausted,
        converted_bytes = snapshot.converted_bytes,
        warp_bytes = snapshot.warp_bytes,
        allocation_bytes = snapshot.allocation_bytes,
        allocation_operations = snapshot.allocation_operations,
        copy_bytes = snapshot.copy_bytes,
        copy_operations = snapshot.copy_operations,
        worker_limit = snapshot.worker_limit,
        peak_workers = snapshot.peak_workers,
        metadata_reads = snapshot.metadata_reads,
        row_updates = snapshot.row_updates,
        thumbnail_bytes = snapshot.thumbnail_bytes,
        job_polls = snapshot.job_polls,
        cache_hits = snapshot.cache_hits,
        cache_misses = snapshot.cache_misses,
        decoded_bytes = snapshot.decoded_bytes,
        sample_bytes = snapshot.sample_bytes,
        packed_bytes = snapshot.packed_bytes,
        upload_bytes = snapshot.upload_bytes,
        pipeline_creations = snapshot.pipeline_creations,
        cpu_submissions = snapshot.cpu_submissions,
        buffer_creations = snapshot.buffer_creations,
        readback_bytes = snapshot.readback_bytes,
        level0_window_misses = snapshot.level0_window_misses,
        feature_seed_attempts = snapshot.feature_seed_attempts,
        feature_seeds_accepted = snapshot.feature_seeds_accepted,
        gpu_execution_available = snapshot.gpu_execution_available,
        presentation_available = snapshot.presentation_available,
        saturated = snapshot.saturated,
    )
    .entered();
}

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

impl Stage {
    /// This stage's lower-case name, the spelling its key carries in
    /// [`StageTimings`] and its span carries in the log.
    fn name(self) -> &'static str {
        match self {
            Stage::Setup => "setup",
            Stage::Align => "align",
            Stage::Fuse => "fuse",
            Stage::WarpWrite => "warp_write",
            Stage::Finalize => "finalize",
        }
    }
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
    stage_span: Option<tracing::span::EnteredSpan>,
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
            stage_span: None,
        }
    }

    /// Closes whatever stage span is open and enters one for the stage
    /// about to run, so every event raised until the next [`Self::enter`]
    /// or [`Self::mark`] names the stage it happened in. A timer that holds
    /// an entered span is not `Send`: it stays on the thread that built it,
    /// and an early return closes the span by dropping the timer.
    pub fn enter(&mut self, stage: Stage) {
        // Taken before the new span is created: a span created while the
        // previous one is still entered would become its child rather than
        // its sibling.
        self.stage_span = None;
        self.stage_span = Some(tracing::debug_span!("stage", stage = stage.name()).entered());
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
        self.stage_span = None;
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

#[cfg(test)]
mod accounting_tests {
    use super::{RECORD_BUDGET, TimingCounter, TimingState};

    #[test]
    fn recording_budget_admits_observed_capture_demand() {
        const OBSERVED_CAPTURE_DEMAND: usize = 220_018;
        let state = TimingState::new(RECORD_BUDGET);

        for _ in 0..OBSERVED_CAPTURE_DEMAND {
            assert!(state.admit());
        }

        let snapshot = state.snapshot();
        assert_eq!(snapshot.admitted, OBSERVED_CAPTURE_DEMAND as u64);
        assert_eq!(snapshot.exhausted, 0);
        assert!(!snapshot.saturated);
    }

    #[test]
    fn finite_admission_reports_exhaustion_without_wrapping_counters() {
        let state = TimingState::new(2);

        assert!(state.admit());
        state.add(TimingCounter::ConvertedBytes, u64::MAX);
        state.add(TimingCounter::AllocationOperations, 1);
        assert!(state.admit());
        state.add(TimingCounter::ConvertedBytes, 1);
        assert!(!state.admit());

        let snapshot = state.snapshot();
        assert_eq!(snapshot.admitted, 2);
        assert_eq!(snapshot.exhausted, 1);
        assert_eq!(snapshot.converted_bytes, u64::MAX);
        assert_eq!(snapshot.allocation_operations, 1);
        assert!(snapshot.saturated);
        assert!(state.begin_summary());
        assert!(!state.begin_summary());
    }

    #[test]
    fn the_feature_seed_counters_accumulate_and_snapshot() {
        let state = TimingState::new(2);

        state.add(TimingCounter::FeatureSeedAttempts, 3);
        state.add(TimingCounter::FeatureSeedAttempts, 1);
        state.add(TimingCounter::FeatureSeedsAccepted, 2);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.feature_seed_attempts, 4);
        assert_eq!(snapshot.feature_seeds_accepted, 2);
        assert_eq!(
            TimingCounter::FeatureSeedAttempts.name(),
            "feature_seed_attempts"
        );
        assert_eq!(
            TimingCounter::FeatureSeedsAccepted.name(),
            "feature_seeds_accepted"
        );
    }

    #[test]
    fn buffer_creations_and_readback_bytes_accumulate_and_saturate() {
        let state = TimingState::new(2);

        state.add(TimingCounter::BufferCreations, 2);
        state.add(TimingCounter::BufferCreations, 3);
        state.add(TimingCounter::ReadbackBytes, 1 << 20);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.buffer_creations, 5);
        assert_eq!(snapshot.readback_bytes, 1 << 20);
        assert!(!snapshot.saturated);

        state.add(TimingCounter::ReadbackBytes, u64::MAX);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.readback_bytes, u64::MAX);

        state.add(TimingCounter::Level0WindowMisses, 2);
        assert_eq!(state.snapshot().level0_window_misses, 2);
        assert_eq!(snapshot.buffer_creations, 5);
        assert!(snapshot.saturated);
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
