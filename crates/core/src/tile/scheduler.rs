// SPDX-License-Identifier: GPL-3.0-or-later

//! The out-of-core tile scheduler: walks a [`TilePlan`] in parallel with a
//! **scheduler-owned** rayon thread pool (never the global default, which is
//! sized from the machine and from `RAYON_NUM_THREADS`), and hands every tile
//! to a [`TileConsumer`] as an on-demand [`TileCtx`]: frame `f`'s matching
//! tile is read through a [`TileReader`] when the consumer reaches it, passed
//! through the [`FramePreprocess`] hook, and overwritten by
//! the next frame. A tile therefore holds one frame-tile and one coverage
//! mask, never one per frame.
//!
//! # A consumer that supplies its own pixels
//!
//! A [`TileConsumer`] answering [`TileConsumer::supplies_own_pixels`] `true`
//! (a GPU-backed consumer, for instance) is handed an empty context and reads
//! nothing here -- it pulls its own, possibly footprint-grown, source rects
//! instead of the scheduler's own destination read rect. Two consequences
//! follow: [`TileRunReport::frame_tiles_read`] is `0` on that path, because
//! the scheduler genuinely read no frame-tile; and the [`FramePreprocess`]
//! hook is never called for that tile.
//!
//! # Memory budget: an admission gate, not the thread count
//!
//! The rayon pool is sized from `std::thread::available_parallelism()`
//! unconditionally, independent of the memory budget. Concurrency
//! is instead bounded by a [`crate::tile::budget::HostBudgetGate`]: every
//! worker acquires a [`crate::tile::budget::HostPermit`] before it touches a
//! tile and the tile's buffers are gone before the permit drops, so a pool
//! much larger than the admission limit never holds more live tiles than the
//! limit allows -- a full-size pool behind a small gate blocks the excess
//! workers in the gate's own `Condvar`, not by never having been spawned.
//!
//! ```text
//! bytes_per_tile = min(tile_size, extent)^2 * (samples_per_pixel * 2 + 1 + working_bytes_per_pixel)
//!                  + reader scratch
//! in_flight_limit = clamp(budget_bytes / bytes_per_tile, 1, pool_threads)
//! pool_threads = available_parallelism()  -- independent of budget_bytes
//! ```
//!
//! The terms are one `u16` frame tile, one coverage byte per pixel, the
//! method's live heap per pixel
//! ([`TileSchedulerOptions::working_bytes_per_pixel`]) and the reader's
//! per-read allocation. The frame count enters nowhere: 8 and 600 frames
//! admit the same number of tiles.
//!
//! `tile_size` is the outer size: `TilePlan` cuts the interior down by
//! `2 * margin` and then reads it back with `margin` on every side, so a
//! read rect is `tile_size` and never more. Adding a margin term here
//! counts the same margin twice.
//!
//! Worked example: a 512^2 tile of 3 samples costs 7 B/px for the frame tile
//! and mask, 1.75 MiB; a method holding 70 B/px more brings it to 19.25 MiB,
//! so a budget of 1 GiB admits 53 tiles, further capped by the pool.
//!
//! The budget itself is [`crate::tile::budget::HostBudget`]: host-derived by
//! default (`Provenance::Host`, a share of what the host currently reports
//! available), the stated
//! [`DEFAULT_MAX_BYTES_IN_FLIGHT`] constant when the host probe degrades
//! (`Provenance::HostFallback`), or whatever a caller supplies explicitly
//! (`Provenance::Explicit`, via [`TileSchedulerOptions::explicit_budget`]).
//! A host-derived budget below one tile refuses the run outright
//! (`TileError::HostMemoryTooSmall`); an explicit or flag-capped budget
//! floors at 1 tile instead. During a run, the gate
//! watches this process's own RSS and shrinks the limit -- never raises it
//! back -- when observed pressure says the starting estimate was too
//! optimistic; see `crate::tile::budget`'s own module doc for the signal and
//! its cadence.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::algorithm::{TileCtx, TileReader};
use crate::host::HostFacts;
use crate::params::{Provenance, StackContext};
use crate::tile::TileError;
use crate::tile::budget::{HostBudget, HostBudgetGate, HostSources};
use crate::tile::consumer::{TileConsumer, TileJob};
use crate::tile::control::{NoControl, TileControl};
use crate::tile::plan::{DEFAULT_TILE_SIZE, PlannedTile, TilePlan, TileSupport};
use crate::tile::preprocess::FramePreprocess;
use crate::tile::scratch::ScratchDir;
use crate::timing::measurement_scope;

/// The default pyramid depth: **5, a default, never a cap**. `levels` stays
/// a live parameter -- a caller (`ParamDesc`, or a direct
/// `TileSchedulerOptions` literal, as every test in this crate already
/// does) can ask for any depth the geometry allows, up to
/// [`super::plan::margin_for_levels`]'s measured table and
/// [`crate::tile::TileError::NoInterior`]'s refusal; nothing here clamps
/// that request.
///
/// Five, not seven, because `crate::fuse::pyramid::MEASURED_MARGIN` --
/// the *measured*, not analytic, tile-overlap margin -- is
/// `[0, 4, 12, 24, 48, 96, 192, 512]` for `levels = 0..=7`. At the
/// [`super::plan::DEFAULT_TILE_SIZE`] of 512 px, and after the interior
/// stride is rounded down to the decimation grid, that leaves an interior
/// of 464 px at 3 levels and 320 px at 5 -- **the shipped default is
/// unaffected by the validity-range floor below**: 512 px at 5 levels
/// needs 448 px (`4 * 96 + 64`) and has 512.
///
/// **6 and 7 levels are both refused outright at 512 px** (`crate::tile::TileError::NoInterior`),
/// naming 832 px and 2112 px as the smallest tile that plans (`max(2 * margin + 2^levels, 4 *
/// margin + 64)`): the margin table is only established in a `4 * margin + 64` buffer per side, and
/// 512 px falls short of it at 6 levels (832 px needed) and at 7 (whose coarsest Gaussian there is
/// a 4x4 image). 5 levels keeps a healthy majority of the tile as usable interior; raising it costs
/// width fast.
pub const DEFAULT_LEVELS: u32 = 5;

/// The default in-flight memory budget: 1 GiB of accounted tile bytes.
pub const DEFAULT_MAX_BYTES_IN_FLIGHT: usize = 1 << 30;

/// Tile geometry and the memory budget, as scheduler constants plus
/// explicit constructor arguments -- **not** `ParamDesc` parameters and not
/// a second schema.
///
/// No `Default` impl: there is no honest default scratch base for a
/// library. A caller (the CLI) supplies one -- repo-relative
/// `.scratch/` for `focusforge-cli`.
///
/// `#[derive(Clone)]` only, with a manual `Debug`: `host` carries trait
/// objects (`HostSources`) that do not implement `Debug`, the same
/// treatment `ParamDesc` gives its own hooks.
#[derive(Clone)]
pub struct TileSchedulerOptions {
    pub tile_size: u32,
    pub levels: u32,
    /// The method's reach beyond the pyramid margin (`TileSupport::reach_px`).
    pub reach_px: u32,
    /// The method's own live heap per pixel of a read rect
    /// (`StackAlgorithm::working_bytes_per_pixel`), counted in each tile's
    /// accounted bytes. 0 for a consumer that holds nothing of its own.
    pub working_bytes_per_pixel: u64,
    pub budget: HostBudget,
    pub host: HostSources,
    pub scratch_base: PathBuf,
}

impl std::fmt::Debug for TileSchedulerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TileSchedulerOptions")
            .field("tile_size", &self.tile_size)
            .field("levels", &self.levels)
            .field("reach_px", &self.reach_px)
            .field("working_bytes_per_pixel", &self.working_bytes_per_pixel)
            .field("budget", &self.budget)
            .field("host", &"...")
            .field("scratch_base", &self.scratch_base)
            .finish()
    }
}

impl TileSchedulerOptions {
    /// The adaptive path: the budget is derived from
    /// [`crate::host::snapshot`] (a `OnceLock`, so the probe runs once per
    /// process) and reports `Provenance::Host`/`Provenance::HostFallback`
    /// accordingly.
    pub fn new(scratch_base: impl Into<PathBuf>) -> Self {
        TileSchedulerOptions {
            tile_size: DEFAULT_TILE_SIZE,
            levels: DEFAULT_LEVELS,
            reach_px: 0,
            working_bytes_per_pixel: 0,
            budget: HostBudget::host_derived(&crate::host::snapshot()),
            host: HostSources::default(),
            scratch_base: scratch_base.into(),
        }
    }

    /// A caller-stated budget in bytes: never probes, never arms the
    /// runtime back-off, and floors at 1 tile rather than refusing.
    /// What every existing test site uses.
    pub fn explicit_budget(scratch_base: impl Into<PathBuf>, max_bytes_in_flight: u64) -> Self {
        TileSchedulerOptions {
            tile_size: DEFAULT_TILE_SIZE,
            levels: DEFAULT_LEVELS,
            reach_px: 0,
            working_bytes_per_pixel: 0,
            budget: HostBudget::explicit(max_bytes_in_flight),
            host: HostSources::default(),
            scratch_base: scratch_base.into(),
        }
    }

    /// A fabricated [`HostFacts`] in place of a live probe -- the seam a
    /// test uses to substitute a large or a tiny host without touching the
    /// machine actually running the suite.
    pub fn with_host_facts(scratch_base: impl Into<PathBuf>, facts: &HostFacts) -> Self {
        TileSchedulerOptions {
            tile_size: DEFAULT_TILE_SIZE,
            levels: DEFAULT_LEVELS,
            reach_px: 0,
            working_bytes_per_pixel: 0,
            budget: HostBudget::host_derived(facts),
            host: HostSources::default(),
            scratch_base: scratch_base.into(),
        }
    }

    /// Plans for a method's `support`: its reach and depth, and its default
    /// tile size in place of [`DEFAULT_TILE_SIZE`]. A caller that states a
    /// tile size sets `tile_size` afterwards.
    pub fn apply_support(&mut self, support: TileSupport) {
        self.levels = support.decimation_levels;
        self.reach_px = support.reach_px;
        self.tile_size = support.default_tile_size();
    }

    fn support(&self) -> TileSupport {
        TileSupport {
            reach_px: self.reach_px,
            decimation_levels: self.levels,
        }
    }
}

/// What one [`TileScheduler::run`] call measured and produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRunReport {
    pub tiles: usize,
    pub frame_tiles_read: usize,
    pub in_flight_limit: usize,
    pub peak_in_flight: usize,
    /// The budget's byte value, as reported by `HostBudget::bytes()` at
    /// construction.
    pub max_bytes_in_flight: u64,
    /// Where that budget came from.
    pub budget_provenance: Provenance,
    /// The admission limit in force at the end of the run -- equal to
    /// `in_flight_limit` unless the runtime back-off shrunk it.
    pub final_in_flight_limit: usize,
}

/// What one [`TileScheduler::run_tiles_controlled`] call measured, produced,
/// and -- unlike [`TileRunReport`] alone -- whether it stopped before every
/// submitted tile ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlledRun {
    /// `report.tiles` is the count of tiles *submitted* to this call, not
    /// necessarily the count that ran: on a stopped run (see `stopped`) the
    /// two can differ.
    pub report: TileRunReport,
    /// `true` if [`TileControl::should_stop`] ended this call before every
    /// submitted tile ran.
    pub stopped: bool,
    /// The sorted [`PlannedTile::index`] values that finished in this call.
    pub completed: Vec<usize>,
}

/// Internal short-circuit sentinel for
/// [`TileScheduler::run_tiles_controlled`]'s `try_for_each_init`. Never a
/// [`TileError`] variant -- `TileError` is `#[non_exhaustive]` and every one
/// of its variants already means failure; a `Cancelled` variant there
/// would make every existing `match` on `TileError` read a user's pause as a
/// broken run.
///
/// `Failed` carries no payload: `TileError` is not `Clone` (it wraps
/// `std::io::Error` and `rayon::ThreadPoolBuildError`, neither of which is),
/// so the first genuine error this run observes is recorded once in a
/// side-channel `Mutex<Option<TileError>>` instead and consulted
/// unconditionally after the loop -- a duplicate `TileError` payload here
/// would be redundant even if carrying one were possible. rayon 1.12.0 does
/// not specify which of several parallel errors `try_for_each_init` returns,
/// so that side channel, not this enum's payload, is what makes "a stop
/// never masks a real error" true.
enum RunStop {
    /// [`TileControl::should_stop`] returned `true`.
    Requested,
    /// A tile's own work failed; the real [`TileError`] lives in the run's
    /// `first_error` mutex.
    Failed,
}

/// Bytes one in-flight tile is accounted at, for a `width x height` read rect
/// at `samples_per_pixel` samples, `u16` (2 bytes) each: the frame tile being
/// fused, its coverage mask (1 byte per pixel) and the method's own
/// `working_bytes_per_pixel`. The frame count does not enter: frames are read
/// one at a time into the same buffer.
///
/// The caller passes the **outer** tile size clamped to the frame.
/// [`TilePlan::new`] derives `raw_interior = tile_size - 2 * margin`, then
/// gives each tile a read rect of `interior + margin` on every side -- so
/// the read rect reconstitutes `tile_size` and never exceeds it. A margin
/// term added here would be counting the same margin twice.
///
/// A trailing remainder smaller than the margin is absorbed into the
/// previous stride, and that stride is terminal: the frame edge clamps its
/// far side, so its read rect stays within `tile_size` too
/// (`tile::plan::tests::read_rects_never_exceed_the_tile_size`). Measured
/// over the reference partition (8192x5464, tile 512, 5 levels) the largest
/// read rect is exactly 512x512.
fn bytes_per_tile(
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    working_bytes_per_pixel: u64,
) -> u64 {
    let pixels = width as u64 * height as u64;
    pixels * (samples_per_pixel as u64 * 2 + 1 + working_bytes_per_pixel)
}

/// The pure arithmetic behind the budget: how many tiles can be live at
/// once under `max_bytes_in_flight`, floored at 1 (never 0, so a budget
/// smaller than one tile still makes forward progress) and capped at
/// `available_parallelism` (never more workers than the host has, and never
/// scaling with it either -- the cap is a ceiling, not a target).
///
/// `pub(crate)`, not private: `crate::tile::budget::HostBudgetGate` calls
/// this directly to compute its own initial limit.
pub(crate) fn in_flight_for_budget(
    bytes_per_tile: u64,
    max_bytes_in_flight: u64,
    available_parallelism: usize,
) -> usize {
    let available_parallelism = available_parallelism.max(1);
    if bytes_per_tile == 0 {
        return available_parallelism;
    }
    let raw = (max_bytes_in_flight / bytes_per_tile).max(1);
    (raw.min(available_parallelism as u64)) as usize
}

/// The out-of-core tile scheduler. Typed on `&dyn TileReader`, never on a
/// concrete reader: this is what lets a test drive it from an in-memory
/// reader and lets a warping reader slot in without touching this
/// module.
pub struct TileScheduler<'a> {
    reader: &'a dyn TileReader,
    frame_count: usize,
    samples_per_pixel: u32,
    plan: TilePlan,
    scratch_base: PathBuf,
    in_flight: usize,
    pool_threads: usize,
    bytes_per_tile: u64,
    budget: HostBudget,
    gate: HostBudgetGate,
}

impl<'a> TileScheduler<'a> {
    /// `samples_per_pixel` is a separate argument, not a `StackContext`
    /// field: `StackContext` is `#[non_exhaustive]` with a three-argument
    /// `new`, and `TileReader` exposes no geometry of its own. A value that
    /// disagrees with the reader surfaces on the first tile as that
    /// reader's own error, wrapped in [`TileError::Consumer`].
    pub fn new(
        reader: &'a dyn TileReader,
        stack: StackContext,
        samples_per_pixel: u32,
        options: &TileSchedulerOptions,
    ) -> Result<Self, TileError> {
        let plan = TilePlan::new(
            stack.width,
            stack.height,
            options.tile_size,
            options.support(),
        )?;
        let pool_threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        // A read rect never exceeds the frame, whatever the tile size.
        let read_width = options.tile_size.min(stack.width);
        let read_height = options.tile_size.min(stack.height);
        let bytes = bytes_per_tile(
            read_width,
            read_height,
            samples_per_pixel,
            options.working_bytes_per_pixel,
        ) + reader.read_scratch_bytes(read_width, read_height);

        if options.budget.refuses_below_one_tile() && bytes > 0 && options.budget.bytes() < bytes {
            return Err(TileError::HostMemoryTooSmall {
                budget_bytes: options.budget.bytes(),
                bytes_per_tile: bytes,
                tile_size: options.tile_size,
            });
        }

        let in_flight = in_flight_for_budget(bytes, options.budget.bytes(), pool_threads);
        let gate = HostBudgetGate::new(options.budget, bytes, pool_threads, options.host.clone());

        Ok(TileScheduler {
            reader,
            frame_count: stack.frame_count,
            samples_per_pixel,
            plan,
            scratch_base: options.scratch_base.clone(),
            in_flight,
            pool_threads,
            bytes_per_tile: bytes,
            budget: options.budget,
            gate,
        })
    }

    pub fn plan(&self) -> &TilePlan {
        &self.plan
    }

    /// The number of tiles the scheduler allows to be in flight at once at
    /// the start of a run, derived from the memory budget at
    /// construction. The runtime back-off may shrink the *actual* admission
    /// limit during a run -- see `TileRunReport::final_in_flight_limit`.
    pub fn in_flight_limit(&self) -> usize {
        self.in_flight
    }

    /// The bytes one in-flight tile is accounted at: one frame tile, its
    /// coverage mask, the method's working bytes and the reader's per-read
    /// scratch.
    pub fn bytes_per_tile(&self) -> u64 {
        self.bytes_per_tile
    }

    /// The rayon pool's own thread count -- `available_parallelism()`,
    /// independent of the memory budget.
    pub fn pool_threads(&self) -> usize {
        self.pool_threads
    }

    /// The memory budget this scheduler was built with.
    pub fn budget(&self) -> HostBudget {
        self.budget
    }

    /// The admission gate's current permit ceiling. Starts equal to
    /// [`Self::in_flight_limit`]; a caller that never runs anything through
    /// this scheduler's own runtime back-off will always see the two equal.
    pub fn final_in_flight_limit(&self) -> usize {
        self.gate.limit()
    }

    /// Run every tile of this scheduler's plan.
    pub fn run(
        &self,
        preprocess: &dyn FramePreprocess,
        consumer: &dyn TileConsumer,
    ) -> Result<TileRunReport, TileError> {
        self.run_tiles(self.plan.tiles(), preprocess, consumer)
    }

    /// Run exactly `tiles` (a subset of, or all of, `self.plan().tiles()`).
    /// The subset entry point the ignored reference-stack test uses to
    /// schedule a bounded number of tiles without materializing a frame,
    /// and a resume path can reuse to restart a partially-completed
    /// run.
    pub fn run_tiles(
        &self,
        tiles: &[PlannedTile],
        preprocess: &dyn FramePreprocess,
        consumer: &dyn TileConsumer,
    ) -> Result<TileRunReport, TileError> {
        self.run_tiles_controlled(tiles, preprocess, consumer, &NoControl)
            .map(|controlled| controlled.report)
    }

    /// The controlled entry point [`run`](Self::run) and
    /// [`run_tiles`](Self::run_tiles) delegate to with [`NoControl`], and
    /// `core::job::Job::run` drives directly: the per-tile loop, with a
    /// control check as the very first statement of the per-tile closure
    /// (before `live.fetch_add`, so a skipped unit never counts as in
    /// flight and never perturbs `peak_in_flight`), and
    /// per-index completion bookkeeping so a caller can resume from exactly
    /// the tiles this call did not finish. This is what makes
    /// "byte-identical with and without the engine" fall out of the shape
    /// rather than needing a second code path.
    ///
    /// A consumer answering [`TileConsumer::supplies_own_pixels`] `true`
    /// short-circuits the host read loop: it is handed a
    /// [`TileJob::without_frame_tiles`] and the run's `frame_tiles_read`
    /// stays at 0. Completion bookkeeping is unaffected -- such a tile is
    /// marked complete and reported through [`TileControl::unit_completed`]
    /// exactly like a normally-read one, so pause/resume behaves the same on
    /// either path.
    ///
    /// # The pause bound
    ///
    /// Once a [`TileControl::should_stop`] implementation starts returning
    /// `true`, every worker checks it immediately after acquiring its
    /// admission permit and before starting work, so at most
    /// `self.in_flight_limit()` permits already held when the request is
    /// observed can each finish the one unit admitted under it. **At most
    /// `in_flight_limit` further units complete** after
    /// a stop is requested -- exactly at most 1 when `max_bytes_in_flight =
    /// 1` pins the admission gate to a single permit via the floor in
    /// `in_flight_for_budget`. This bound rests on our own check, not on
    /// rayon's short-circuit, which is documented as best-effort rather
    /// than a guaranteed immediate stop (rayon 1.12.0 --
    /// <https://docs.rs/rayon/1.12.0/rayon/iter/trait.ParallelIterator.html#method.try_for_each_init>).
    pub fn run_tiles_controlled(
        &self,
        tiles: &[PlannedTile],
        preprocess: &dyn FramePreprocess,
        consumer: &dyn TileConsumer,
        control: &dyn TileControl,
    ) -> Result<ControlledRun, TileError> {
        let supplies_own_pixels = consumer.supplies_own_pixels();

        let scratch = ScratchDir::create_in(&self.scratch_base)?;
        let scratch_path = scratch.path().to_path_buf();

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.pool_threads)
            .thread_name(|i| format!("focusforge-tile-{i}"))
            .panic_handler(crate::pool::log_worker_panic)
            .build()
            .map_err(|source| TileError::ThreadPool { source })?;

        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let frame_tiles_read = AtomicUsize::new(0);
        // Completed units are recorded by index, not merely counted: rayon
        // finishes tiles out of order, so "the first N" is never the set
        // that actually ran. Sized to the
        // *whole plan*, not just this call's `tiles`, so a resume's index
        // set stays meaningful against `self.plan()`.
        let completed: Vec<AtomicBool> = (0..self.plan.len())
            .map(|_| AtomicBool::new(false))
            .collect();
        // A stop must never mask a real error: the first genuine failure this run observes, independent of
        // whichever closure's return value `try_for_each_init` itself ends
        // up propagating.
        let first_error: Mutex<Option<TileError>> = Mutex::new(None);

        let frame_count = self.frame_count;
        let spp = self.samples_per_pixel;
        let reader = self.reader;
        let gate = &self.gate;

        // Span context does not cross onto a pool thread by itself: the
        // caller's span is captured here and each tile span is parented to
        // it explicitly, so an event raised inside a tile's closure still
        // names the run it belongs to.
        let parent = tracing::Span::current();

        let install_result: Result<(), RunStop> = pool.install(|| {
            tiles
                .par_iter()
                .try_for_each(|planned: &PlannedTile| -> Result<(), RunStop> {
                    // The budget admission permit is acquired first, above
                    // the control check below: the rayon pool's own thread
                    // count is independent of the budget (`pool_threads` is
                    // `available_parallelism()` unconditionally), so a stop
                    // check that ran *before* admission would let every pool
                    // thread past it before any of them blocks on the gate --
                    // silently widening the pause bound documented on
                    // `run_tiles_controlled` from `in_flight_limit` to
                    // `pool_threads`. Acquiring first keeps admission itself
                    // the bound: at most one further unit per outstanding
                    // permit completes after a stop is requested.
                    let _permit = gate.acquire(control);

                    // The control check is the first statement after
                    // admission, above `live.fetch_add` below, so a skipped
                    // unit never counts as in flight.
                    if control.should_stop() {
                        return Err(RunStop::Requested);
                    }

                    // One span per tile, never per pixel: the finest
                    // granularity anything in this loop may cost.
                    let tile_span = tracing::debug_span!(
                        parent: &parent,
                        "tile",
                        index = planned.index(),
                        rect = ?planned.interior()
                    );
                    let _tile = tile_span.enter();
                    let _tile_total = measurement_scope("fusion_tile");

                    let cur = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(cur, Ordering::SeqCst);

                    let outcome = (|| -> Result<(), TileError> {
                        let read_rect = planned.read();

                        if supplies_own_pixels {
                            let job = TileJob::without_frame_tiles(planned, &scratch_path);
                            let _consume = measurement_scope("fusion_consume");
                            return consumer
                                .consume(&job)
                                .map_err(|source| TileError::Consumer {
                                    rect: planned.interior(),
                                    source,
                                });
                        }

                        let ctx =
                            TileCtx::on_demand(read_rect, frame_count, spp, reader, preprocess);
                        let job = TileJob::new(planned, ctx, &scratch_path);
                        let _consume = measurement_scope("fusion_consume");
                        let consumed = consumer.consume(&job);
                        frame_tiles_read.fetch_add(job.ctx().frames_read(), Ordering::Relaxed);
                        consumed.map_err(|source| TileError::Consumer {
                            rect: planned.interior(),
                            source,
                        })
                    })();

                    live.fetch_sub(1, Ordering::SeqCst);

                    match outcome {
                        Ok(()) => {
                            completed[planned.index()].store(true, Ordering::SeqCst);
                            // Per-unit, on the worker that ran it: this is
                            // what makes a polled `JobProgress` advance
                            // during the run instead of only when the
                            // segment ends.
                            let read_rect = planned.read();
                            control
                                .unit_completed(read_rect.width as u64 * read_rect.height as u64);
                            tracing::debug!("tile complete");
                            Ok(())
                        }
                        Err(e) => {
                            let mut guard = first_error.lock().unwrap();
                            if guard.is_none() {
                                *guard = Some(e);
                            }
                            Err(RunStop::Failed)
                        }
                    }
                })
        });

        // A recorded genuine error always wins over a mere stop request.
        if let Some(err) = first_error.into_inner().unwrap() {
            return Err(err);
        }

        let stopped = match install_result {
            Ok(()) => false,
            Err(RunStop::Requested) => true,
            Err(RunStop::Failed) => {
                unreachable!(
                    "RunStop::Failed is only ever returned after recording the real error, \
                     and that recorded error already returned above"
                )
            }
        };

        let completed_indices: Vec<usize> = completed
            .iter()
            .enumerate()
            .filter_map(|(index, done)| done.load(Ordering::SeqCst).then_some(index))
            .collect();

        Ok(ControlledRun {
            report: TileRunReport {
                tiles: tiles.len(),
                frame_tiles_read: frame_tiles_read.load(Ordering::Relaxed),
                in_flight_limit: self.in_flight,
                peak_in_flight: peak.load(Ordering::SeqCst),
                max_bytes_in_flight: self.budget.bytes(),
                budget_provenance: self.budget.provenance(),
                final_in_flight_limit: self.gate.limit(),
            },
            stopped,
            completed: completed_indices,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::TiffStack;

    fn _assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn tiff_stack_and_tile_reader_are_send_and_sync() {
        _assert_send_sync::<TiffStack>();
        _assert_send_sync::<&dyn TileReader>();
    }

    #[test]
    fn budget_arithmetic_at_the_reference_scale() {
        // 512x512 read rects, 3 samples/pixel, u16, plus one mask byte per
        // pixel: 7 B/px for the frame tile, whatever the frame count.
        let bytes = bytes_per_tile(512, 512, 3, 0);
        assert_eq!(bytes, 1_835_008);
        // A method holding 70 B/px more floors a 1 GiB budget to 53 tiles.
        let in_flight = in_flight_for_budget(bytes_per_tile(512, 512, 3, 70), 1u64 << 30, 64);
        assert_eq!(in_flight, 53);
        assert_eq!(in_flight_for_budget(bytes, 1u64 << 30, 32), 32);
    }

    #[test]
    fn budget_below_one_tile_floors_at_one_never_zero() {
        let bytes = bytes_per_tile(512, 512, 3, 0);
        let in_flight = in_flight_for_budget(bytes, 1, 32);
        assert_eq!(in_flight, 1);
    }

    #[test]
    fn in_flight_never_exceeds_available_parallelism() {
        // A tiny tile (so the budget alone would allow hundreds live)
        // must still be capped by the host's reported parallelism.
        let bytes = bytes_per_tile(4, 4, 1, 0);
        let in_flight = in_flight_for_budget(bytes, u64::MAX, 4);
        assert_eq!(in_flight, 4);
    }
}
