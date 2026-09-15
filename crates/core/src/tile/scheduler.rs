// SPDX-License-Identifier: GPL-3.0-or-later

//! The out-of-core tile scheduler: walks a [`TilePlan`] in parallel with a
//! **scheduler-owned** rayon thread pool (never the global default, which is
//! sized from the machine and from `RAYON_NUM_THREADS`), reading every
//! frame's matching tile through a [`TileReader`], running the (currently
//! empty) [`FramePreprocess`] hook per frame-tile, and handing the set to a
//! [`TileConsumer`].
//!
//! # A consumer that supplies its own pixels
//!
//! A [`TileConsumer`] answering [`TileConsumer::supplies_own_pixels`] `true`
//! (a GPU-backed consumer, for instance) skips this host read loop entirely -- it pulls
//! its own, possibly footprint-grown, source rects instead of the
//! scheduler's own destination read rect. Two consequences follow, both
//! deliberate rather than papered over: [`TileRunReport::frame_tiles_read`]
//! is `0` on that path, because the scheduler genuinely read no frame-tile;
//! and the (currently empty) [`FramePreprocess`] hook is never called for
//! that tile, since there is no frame-tile to run it over --
//! [`TileScheduler::run_tiles`] refuses outright (rather than silently
//! skipping) when a non-noop `FramePreprocess` meets such a consumer.
//!
//! # Memory budget
//!
//! The rayon pool is sized from `std::thread::available_parallelism()`
//! unconditionally, never from the memory budget. Concurrency
//! is bounded by a [`crate::tile::budget::HostBudgetGate`]: every
//! worker acquires a [`crate::tile::budget::HostPermit`] before it reads a
//! frame-tile and releases the tile-set buffer before the permit drops, so
//! a pool much larger than the admission limit never holds more live
//! frame-tile sets than the limit allows -- a full-size pool behind a
//! small gate blocks the excess workers in the gate's own `Condvar`, not by
//! never having been spawned. Coupling the pool size to the byte budget
//! would shrink concurrency as frame count grows, leaving most of a large
//! machine's cores idle even though it has the memory to keep them busy.
//!
//! ```text
//! bytes_per_tile_set = frames * tile_size^2 * samples_per_pixel * 2 B
//! in_flight = clamp(max_bytes_in_flight / bytes_per_tile_set, 1, available_parallelism)
//! ```
//!
//! `tile_size` is the outer size: `TilePlan` cuts the interior down by
//! `2 * margin` and then reads it back with `margin` on every side, so a
//! read rect is `tile_size` and never more. Adding a margin term here
//! counts the same margin twice and over-reserves by 1.89x against the
//! real partition.
//!
//! Worked example: 61 frames * 512^2 * 3 samples * 2 B/sample is
//! ~91.5 MiB per tile set, so a 1 GiB budget bounds `in_flight` to a small,
//! fixed number of simultaneously-live frame-tile sets regardless of
//! whether the host reports 4 cores or 128 -- `ThreadPool::install` blocks
//! the calling thread without making it a worker, so live tile sets are
//! bounded by `num_threads` exactly (rayon 1.12.0 --
//! <https://docs.rs/rayon/1.12.0/rayon/struct.ThreadPoolBuilder.html>).

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::algorithm::{TileCtx, TileReader};
use crate::params::StackContext;
use crate::tile::consumer::{TileConsumer, TileJob};
use crate::tile::control::{NoControl, TileControl};
use crate::tile::plan::{DEFAULT_TILE_SIZE, PlannedTile, TilePlan};
use crate::tile::preprocess::FramePreprocess;
use crate::tile::scratch::ScratchDir;
use crate::tile::{Tile, TileError};

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

/// The default in-flight memory budget: 1 GiB of frame-tile sets, before
/// per-worker pyramid allocations on top.
pub const DEFAULT_MAX_BYTES_IN_FLIGHT: usize = 1 << 30;

/// Tile geometry and the memory budget, as scheduler constants plus
/// explicit constructor arguments -- **not** `ParamDesc` parameters and not
/// a second schema.
///
/// No `Default` impl: there is no honest default scratch base for a
/// library. A caller (the CLI) supplies one -- repo-relative
/// `.scratch/` for `focusforge-cli`.
#[derive(Debug, Clone)]
pub struct TileSchedulerOptions {
    pub tile_size: u32,
    pub levels: u32,
    pub max_bytes_in_flight: usize,
    pub scratch_base: PathBuf,
}

impl TileSchedulerOptions {
    pub fn new(scratch_base: impl Into<PathBuf>) -> Self {
        TileSchedulerOptions {
            tile_size: DEFAULT_TILE_SIZE,
            levels: DEFAULT_LEVELS,
            max_bytes_in_flight: DEFAULT_MAX_BYTES_IN_FLIGHT,
            scratch_base: scratch_base.into(),
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
}

/// What one [`TileScheduler::run_tiles_controlled`] call measured, produced,
/// and -- unlike [`TileRunReport`] alone -- whether it stopped before every
/// submitted tile ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlledRun {
    /// `report.tiles` is the count of tiles *submitted* to this call, not
    /// necessarily the count that ran: on a stopped run (see `stopped`) it
    /// is no longer that number.
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

/// `bytes_per_tile_set` for `frame_count` frames of `tile_size x tile_size`
/// read rects at `samples_per_pixel` samples, `u16` (2 bytes) each.
///
/// `tile_size` is the **outer** size, and that is the whole subtlety here.
/// [`TilePlan::new`] derives `raw_interior = tile_size - 2 * margin`, then
/// gives each tile a read rect of `interior + margin` on every side -- so
/// the read rect reconstitutes `tile_size` and never exceeds it. A margin
/// term added here would be counting the same margin twice.
///
/// Measured over the reference partition (8192x5464, tile 512, 5 levels:
/// 442 tiles, interior stride 320) the largest read rect is exactly
/// 512x512. Ragged strides absorbed into a previous stride are always
/// terminal, so they are clamped at the frame edge and come out smaller,
/// not larger.
fn bytes_per_tile_set(frame_count: usize, tile_size: u32, samples_per_pixel: u32) -> u64 {
    frame_count as u64 * tile_size as u64 * tile_size as u64 * samples_per_pixel as u64 * 2
}

/// The pure arithmetic behind the budget: how many tile sets can be live at
/// once under `max_bytes_in_flight`, floored at 1 (never 0, so a budget
/// smaller than one tile set still makes forward progress) and capped at
/// `available_parallelism` (never more workers than the host has, and never
/// scaling with it either -- the cap is a ceiling, not a target).
fn in_flight_for_budget(
    bytes_per_tile_set: u64,
    max_bytes_in_flight: usize,
    available_parallelism: usize,
) -> usize {
    let available_parallelism = available_parallelism.max(1);
    if bytes_per_tile_set == 0 {
        return available_parallelism;
    }
    let raw = (max_bytes_in_flight as u64 / bytes_per_tile_set).max(1);
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
}

impl<'a> TileScheduler<'a> {
    /// `samples_per_pixel` is a separate argument, not a `StackContext`
    /// field: `StackContext` is `#[non_exhaustive]` with a three-argument
    /// `new`, and `TileReader` exposes no geometry of its own. A value that
    /// disagrees with the reader surfaces on the first tile as that
    /// reader's own error, wrapped in [`TileError::Read`].
    pub fn new(
        reader: &'a dyn TileReader,
        stack: StackContext,
        samples_per_pixel: u32,
        options: &TileSchedulerOptions,
    ) -> Result<Self, TileError> {
        let plan = TilePlan::new(stack.width, stack.height, options.tile_size, options.levels)?;
        let available_parallelism = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let bytes = bytes_per_tile_set(stack.frame_count, options.tile_size, samples_per_pixel);
        let in_flight =
            in_flight_for_budget(bytes, options.max_bytes_in_flight, available_parallelism);

        Ok(TileScheduler {
            reader,
            frame_count: stack.frame_count,
            samples_per_pixel,
            plan,
            scratch_base: options.scratch_base.clone(),
            in_flight,
        })
    }

    pub fn plan(&self) -> &TilePlan {
        &self.plan
    }

    /// The number of frame-tile sets the scheduler allows to be live at
    /// once, derived from the memory budget at construction.
    pub fn in_flight_limit(&self) -> usize {
        self.in_flight
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
    /// `core::job::Job::run` drives directly. The body that used to be
    /// `run_tiles`, plus a control check as the very first statement of the
    /// per-tile closure (before `live.fetch_add`, so a skipped unit never
    /// counts as in flight and never perturbs `peak_in_flight`), and
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
    /// `true`, every worker tests that flag at the top of the unit it next
    /// picks up, so each of this run's `self.in_flight_limit()` workers can
    /// finish at most the one unit it already holds when the request is
    /// observed. **At most `in_flight_limit` further units complete** after
    /// a stop is requested -- exactly at most 1 when `max_bytes_in_flight =
    /// 1` pins the pool to a single worker via the floor in
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
        // Deliberately here, not in the `run_tiles` wrapper above:
        // `crate::job::Job::run` drives this method directly and never goes
        // through `run_tiles`, so a guard living in the wrapper would leave
        // the job-engine path silently accepting the combination this
        // refuses.
        let supplies_own_pixels = consumer.supplies_own_pixels();
        if supplies_own_pixels && !preprocess.is_noop() {
            return Err(TileError::PixelSupplyingConsumerNeedsNoopPreprocess);
        }

        let scratch = ScratchDir::create_in(&self.scratch_base)?;
        let scratch_path = scratch.path().to_path_buf();

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.in_flight)
            .thread_name(|i| format!("focusforge-tile-{i}"))
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

        let install_result: Result<(), RunStop> = pool.install(|| {
            tiles.par_iter().try_for_each_init(
                || {
                    (0..frame_count)
                        .map(|_| Tile {
                            rect: crate::tile::Rect {
                                x: 0,
                                y: 0,
                                width: 0,
                                height: 0,
                            },
                            samples_per_pixel: spp,
                            samples: Vec::new(),
                        })
                        .collect::<Vec<_>>()
                },
                |buf: &mut Vec<Tile>, planned: &PlannedTile| -> Result<(), RunStop> {
                    // The control check is the *first* statement of the
                    // per-tile closure, above `live.fetch_add` below, so a
                    // skipped unit never counts as in flight.
                    if control.should_stop() {
                        return Err(RunStop::Requested);
                    }

                    let cur = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(cur, Ordering::SeqCst);

                    let outcome = (|| -> Result<(), TileError> {
                        let read_rect = planned.read();

                        if supplies_own_pixels {
                            let job = TileJob::without_frame_tiles(planned, &scratch_path);
                            return consumer
                                .consume(&job)
                                .map_err(|source| TileError::Consumer {
                                    rect: planned.interior(),
                                    source,
                                });
                        }

                        let len =
                            read_rect.width as usize * read_rect.height as usize * spp as usize;

                        for (frame, tile) in buf.iter_mut().enumerate() {
                            tile.rect = read_rect;
                            tile.samples.resize(len, 0);
                            reader
                                .read_tile_into(frame, read_rect, &mut tile.samples)
                                .map_err(|source| TileError::Read {
                                    frame,
                                    rect: read_rect,
                                    source,
                                })?;
                            frame_tiles_read.fetch_add(1, Ordering::Relaxed);

                            // Preprocess errors carry the same {frame, rect}
                            // shape as a read failure: both happen while
                            // preparing one frame's tile before the
                            // consumer ever sees it, and NoopPreprocess (the
                            // hook's only implementation) never
                            // fails.
                            preprocess.preprocess(frame, tile).map_err(|source| {
                                TileError::Read {
                                    frame,
                                    rect: read_rect,
                                    source,
                                }
                            })?;
                        }

                        let ctx = TileCtx::new(read_rect, buf);
                        let job = TileJob::new(planned, ctx, &scratch_path);
                        consumer
                            .consume(&job)
                            .map_err(|source| TileError::Consumer {
                                rect: planned.interior(),
                                source,
                            })?;
                        Ok(())
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
                },
            )
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
        // 61 frames, 512x512 read rects, 3 samples/pixel, u16.
        let bytes = bytes_per_tile_set(61, 512, 3);
        assert_eq!(bytes, 95_944_704);
        // A 1 GiB budget divided by ~91.5 MiB per tile set floors to 11,
        // and 11 live tile sets are 0.98 GiB -- just under the budget,
        // which is what a budget is supposed to do.
        let in_flight = in_flight_for_budget(bytes, 1 << 30, 32);
        assert_eq!(in_flight, 11);
    }

    #[test]
    fn budget_below_one_tile_set_floors_at_one_never_zero() {
        let bytes = bytes_per_tile_set(61, 512, 3);
        let in_flight = in_flight_for_budget(bytes, 1, 32);
        assert_eq!(in_flight, 1);
    }

    #[test]
    fn in_flight_never_exceeds_available_parallelism() {
        // A tiny tile set (so the budget alone would allow hundreds live)
        // must still be capped by the host's reported parallelism.
        let bytes = bytes_per_tile_set(1, 4, 1);
        let in_flight = in_flight_for_budget(bytes, usize::MAX, 4);
        assert_eq!(in_flight, 4);
    }
}
