// SPDX-License-Identifier: GPL-3.0-or-later

//! [`BackendFusionConsumer`]: the [`TileConsumer`] that drives a
//! [`TileBackend`] through the whole warp-then-fuse pipeline for one tile,
//! joining the scheduler's "a consumer that supplies its own pixels" hook
//! (`crates/core/src/tile/scheduler.rs`) to [`FusionConsumer`]'s write tail.
//! Lives in `crates/core`, not `crates/gpu`: it names no `wgpu` type (`&dyn
//! TileBackend` is a `crates/core`-owned trait object), which is what lets
//! this hook be exercised end to end with a stub backend in the default,
//! GPU-free gate.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::algorithm::{AlgorithmError, TileCtx, TileReader};
use crate::align::{Transform, WarpingReader};
use crate::fuse::FusionConsumer;
use crate::gpu::TileBackend;
use crate::tile::{NoopPreprocess, SampleFormat, TileConsumer, TileJob};

/// Joins a [`TileBackend`] to a [`FusionConsumer`] sink: `consume` calls
/// [`TileBackend::warp_fuse_tile`] over the **raw** `reader` (never a
/// [`WarpingReader`] -- the backend pulls its own footprint-grown source
/// rects) and hands the result to [`FusionConsumer::write_output`].
///
/// A tile whose backend reports its device lost is finished on the host
/// instead, through [`WarpingReader`] and the sink's own
/// [`TileConsumer::consume`], and counts in [`Self::fallback_tiles`].
pub struct BackendFusionConsumer<'a> {
    backend: &'a dyn TileBackend,
    reader: &'a dyn TileReader,
    frame: (u32, u32),
    transforms: Vec<Transform>,
    samples_per_pixel: u32,
    levels: u32,
    sink: &'a FusionConsumer,
    fallback_tiles: AtomicUsize,
}

impl<'a> BackendFusionConsumer<'a> {
    pub fn new(
        backend: &'a dyn TileBackend,
        reader: &'a dyn TileReader,
        frame: (u32, u32),
        transforms: Vec<Transform>,
        samples_per_pixel: u32,
        levels: u32,
        sink: &'a FusionConsumer,
    ) -> Self {
        BackendFusionConsumer {
            backend,
            reader,
            frame,
            transforms,
            samples_per_pixel,
            levels,
            sink,
            fallback_tiles: AtomicUsize::new(0),
        }
    }

    /// How many tiles this consumer finished on the host after the device
    /// was lost. `0` for a run whose device stayed healthy -- which is also
    /// how many frame tiles the run read through this consumer, times the
    /// frame count.
    pub fn fallback_tiles(&self) -> usize {
        self.fallback_tiles.load(Ordering::Relaxed)
    }

    /// Lets the sink fuse this tile from the host, its frames read one at a
    /// time through the warping reader. [`TileConsumer::consume`], not
    /// [`FusionConsumer::write_output`]: the sink owns the algorithm and the
    /// resolved parameter values, and reaching them through `consume` is
    /// what makes a tile finished here equal to the same tile on a run that
    /// never had a backend at all.
    ///
    /// The context reads through [`TileReader::read_tile_coverage_into`], so
    /// the sink sees the coverage a warp leaves.
    fn consume_on_host(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        let warping = WarpingReader::new(
            self.reader,
            self.frame,
            self.samples_per_pixel,
            self.transforms.clone(),
        );
        self.fallback_tiles.fetch_add(1, Ordering::Relaxed);
        let ctx = TileCtx::on_demand(
            job.read_rect(),
            self.transforms.len(),
            self.samples_per_pixel,
            &warping,
            &NoopPreprocess,
        );
        self.sink
            .consume(&TileJob::new(job.tile(), ctx, job.scratch()))
    }
}

impl BackendFusionConsumer<'_> {
    /// The device is re-checked on every error, whatever its shape: the first
    /// call after a loss can fail anywhere between allocating a buffer and
    /// reading the result back. An error raised while the device is still
    /// healthy is a genuine failure and propagates.
    fn backend_failed(&self, job: &TileJob<'_>, err: AlgorithmError) -> Result<(), AlgorithmError> {
        if self.backend.device_lost() {
            self.consume_on_host(job)
        } else {
            Err(err)
        }
    }
}

impl TileConsumer for BackendFusionConsumer<'_> {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        if self.backend.device_lost() {
            return self.consume_on_host(job);
        }
        match self.sink.format() {
            SampleFormat::U16 => match self.backend.warp_fuse_tile(
                self.reader,
                self.frame,
                &self.transforms,
                job.read_rect(),
                self.samples_per_pixel,
                self.levels,
            ) {
                Ok(output) => self.sink.write_output(job, &output),
                Err(err) => self.backend_failed(job, err),
            },
            SampleFormat::F32 => match self.backend.warp_fuse_tile_f32(
                self.reader,
                self.frame,
                &self.transforms,
                job.read_rect(),
                self.samples_per_pixel,
                self.levels,
            ) {
                Ok(output) => self.sink.write_output_f32(job, &output),
                Err(err) => self.backend_failed(job, err),
            },
        }
    }

    fn supplies_own_pixels(&self) -> bool {
        true
    }

    /// Forwarded to the sink, which owns both writers: this consumer holds
    /// no output file of its own, so the whole of the cleanup after a
    /// genuine error is [`FusionConsumer`]'s own `abort`. Without this
    /// override the trait's no-op default would apply and a half-covered
    /// file would stay open on the backend path.
    fn abort(&self) {
        self.sink.abort();
    }
}
