// SPDX-License-Identifier: GPL-3.0-or-later

//! The out-of-core tile scheduler: walks a [`TilePlan`] in parallel with a
//! **scheduler-owned** rayon thread pool (never the global default, which is
//! sized from the machine and from `RAYON_NUM_THREADS`), reading every
//! frame's matching tile through a [`TileReader`], running the (currently
//! empty) [`FramePreprocess`] hook per frame-tile, and handing the set to a
//! [`TileConsumer`].
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
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::algorithm::{TileCtx, TileReader};
use crate::params::StackContext;
use crate::tile::consumer::{TileConsumer, TileJob};
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

        let frame_count = self.frame_count;
        let spp = self.samples_per_pixel;
        let reader = self.reader;

        let result: Result<(), TileError> = pool.install(|| {
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
                |buf: &mut Vec<Tile>, planned: &PlannedTile| -> Result<(), TileError> {
                    let cur = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(cur, Ordering::SeqCst);

                    let outcome = (|| -> Result<(), TileError> {
                        let read_rect = planned.read();
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
                    outcome
                },
            )
        });

        result?;

        Ok(TileRunReport {
            tiles: tiles.len(),
            frame_tiles_read: frame_tiles_read.load(Ordering::Relaxed),
            in_flight_limit: self.in_flight,
            peak_in_flight: peak.load(Ordering::SeqCst),
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
