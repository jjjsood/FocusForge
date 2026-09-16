// SPDX-License-Identifier: GPL-3.0-or-later

//! [`BackendFusionConsumer`]: the [`TileConsumer`] that drives a
//! [`TileBackend`] through the whole warp-then-fuse pipeline for one tile,
//! joining the scheduler's "a consumer that supplies its own pixels" hook
//! (`crates/core/src/tile/scheduler.rs`) to [`FusionConsumer`]'s write tail.
//! Lives in `crates/core`, not `crates/gpu`: it names no `wgpu` type (`&dyn
//! TileBackend` is a `crates/core`-owned trait object), which is what lets
//! this hook be exercised end to end with a stub backend in the default,
//! GPU-free gate.

use crate::algorithm::{AlgorithmError, TileReader};
use crate::align::Transform;
use crate::fuse::FusionConsumer;
use crate::gpu::TileBackend;
use crate::tile::{TileConsumer, TileJob};

/// Joins a [`TileBackend`] to a [`FusionConsumer`] sink: `consume` calls
/// [`TileBackend::warp_fuse_tile`] over the **raw** `reader` (never a
/// [`crate::align::WarpingReader`] -- the backend pulls its own
/// footprint-grown source rects) and hands the result to
/// [`FusionConsumer::write_output`].
pub struct BackendFusionConsumer<'a> {
    backend: &'a dyn TileBackend,
    reader: &'a dyn TileReader,
    frame: (u32, u32),
    transforms: Vec<Transform>,
    samples_per_pixel: u32,
    levels: u32,
    sink: &'a FusionConsumer,
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
        }
    }
}

impl TileConsumer for BackendFusionConsumer<'_> {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        let output = self.backend.warp_fuse_tile(
            self.reader,
            self.frame,
            &self.transforms,
            job.read_rect(),
            self.samples_per_pixel,
            self.levels,
        )?;
        self.sink.write_output(job, &output)
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
