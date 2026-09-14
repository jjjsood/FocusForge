// SPDX-License-Identifier: GPL-3.0-or-later

//! Default-gate, GPU-free coverage of the tile scheduler's "a consumer
//! supplies its own pixels" hook (`crates/core/src/tile/scheduler.rs`'s
//! module docs) and of [`BackendFusionConsumer`], driven end to end through
//! a local, test-only [`TileBackend`] -- no `wgpu`, no feature flag.

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use focusforge_core::algorithm::{AlgorithmError, StackAlgorithm, TileCtx, TileOutput, TileReader};
use focusforge_core::align::warp::WarpedTile;
use focusforge_core::align::{AlignError, Transform};
use focusforge_core::fuse::laplacian::Laplacian;
use focusforge_core::fuse::{BackendFusionConsumer, FusionConsumer, FusionGeometry};
use focusforge_core::gpu::{BackendInfo, TileBackend};
use focusforge_core::io::TiffStack;
use focusforge_core::params::{ParamValue, ParamValues, StackContext};
use focusforge_core::tile::preprocess::FramePreprocess;
use focusforge_core::tile::{
    Rect, Tile, TileConsumer, TileError, TileJob, TileScheduler, TileSchedulerOptions,
};

fn scratch_base(label: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("backend_consumer_scratch_{label}"))
}

/// A [`TileReader`] that counts every call, wrapping another reader that is
/// never actually reached by this file's probe tests.
struct CountingReader<'a> {
    inner: &'a dyn TileReader,
    calls: AtomicUsize,
}

impl TileReader for CountingReader<'_> {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.read_tile_into(frame, rect, out)
    }
}

/// A consumer that declares it supplies its own pixels but never reads
/// anything at all -- exists purely to prove the scheduler's own host read
/// loop is skipped, independent of whatever a real pixel-supplying consumer
/// does internally.
struct OwnPixelsProbe;

impl TileConsumer for OwnPixelsProbe {
    fn consume(&self, _job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        Ok(())
    }

    fn supplies_own_pixels(&self) -> bool {
        true
    }
}

/// A non-noop [`FramePreprocess`]: `is_noop` stays the trait's own default
/// (`false`), which is the whole point of this stub.
struct NotNoopPreprocess;

impl FramePreprocess for NotNoopPreprocess {
    fn preprocess(&self, _frame: usize, _tile: &mut Tile) -> Result<(), AlgorithmError> {
        Ok(())
    }
}

/// A minimal, GPU-free [`TileBackend`]: `warp_fuse_tile` composes a direct
/// read (every transform this file ever hands it is identity) with
/// [`Laplacian::fuse`] -- the same composition
/// `focusforge_core::gpu`'s own `#[cfg(test)]` `DummyBackend` uses, but
/// reachable from an integration test, which a `#[cfg(test)]` item never is.
/// `self_check`/`fuse_tile`/`warp_tile` are never called by this file's
/// tests.
struct DummyBackend;

impl TileBackend for DummyBackend {
    fn info(&self) -> BackendInfo {
        BackendInfo {
            name: "test-dummy".into(),
            backend: "test-dummy".into(),
            driver: "test-dummy".into(),
            driver_info: "test-dummy".into(),
        }
    }

    fn self_check(
        &self,
        _reader: &dyn TileReader,
        _frame: usize,
        _rect: Rect,
        _samples_per_pixel: u32,
        _gain: f32,
    ) -> Result<Tile, AlgorithmError> {
        unimplemented!("not exercised by this file's tests")
    }

    fn fuse_tile(
        &self,
        _reader: &dyn TileReader,
        _frame_count: usize,
        _rect: Rect,
        _samples_per_pixel: u32,
        _levels: u32,
    ) -> Result<TileOutput, AlgorithmError> {
        unimplemented!("not exercised by this file's tests")
    }

    fn warp_tile(
        &self,
        _reader: &dyn TileReader,
        _frame_index: usize,
        _frame: (u32, u32),
        _samples_per_pixel: u32,
        _dst: Rect,
        _transform: &Transform,
    ) -> Result<WarpedTile, AlignError> {
        unimplemented!("not exercised by this file's tests")
    }

    fn warp_fuse_tile(
        &self,
        reader: &dyn TileReader,
        _frame: (u32, u32),
        transforms: &[Transform],
        rect: Rect,
        samples_per_pixel: u32,
        levels: u32,
    ) -> Result<TileOutput, AlgorithmError> {
        let mut tiles = Vec::with_capacity(transforms.len());
        for (i, t) in transforms.iter().enumerate() {
            assert_eq!(
                *t,
                Transform::identity(),
                "this file only ever uses identity transforms"
            );
            let mut samples = vec![0u16; (rect.width * rect.height * samples_per_pixel) as usize];
            reader.read_tile_into(i, rect, &mut samples)?;
            tiles.push(Tile {
                rect,
                samples_per_pixel,
                samples,
            });
        }
        let ctx = TileCtx::new(rect, &tiles);
        let mut cfg = ParamValues::new();
        cfg.insert("levels".to_string(), ParamValue::Int(levels as i64));
        Laplacian.fuse(&ctx, &cfg)
    }
}

fn open_stack() -> TiffStack {
    TiffStack::open(common::mantis_512_dir()).expect("open mantis-512 fixture")
}

#[test]
fn supplies_own_pixels_causes_zero_reads_and_zero_frame_tiles_read() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let stack_ctx = StackContext::new(stack.len(), width, height, spp);
    let counting = CountingReader {
        inner: &stack,
        calls: AtomicUsize::new(0),
    };

    let options = TileSchedulerOptions::new(scratch_base("zero_reads"));
    let scheduler =
        TileScheduler::new(&counting, stack_ctx, spp, &options).expect("scheduler must build");
    let report = scheduler
        .run(&focusforge_core::tile::NoopPreprocess, &OwnPixelsProbe)
        .expect("run must succeed");

    assert_eq!(
        counting.calls.load(Ordering::SeqCst),
        0,
        "a pixel-supplying consumer must cause zero scheduler-side reads"
    );
    assert_eq!(report.frame_tiles_read, 0);
}

#[test]
fn backend_fusion_consumer_matches_fusion_consumer_byte_for_byte() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let stack_ctx = StackContext::new(frame_count, width, height, spp);
    let options = TileSchedulerOptions::new(scratch_base("byte_identical"));

    let mut values = ParamValues::new();
    values.insert("levels".to_string(), ParamValue::Int(5));

    let direct_path =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("backend_consumer_direct_composite.tif");
    let via_backend_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("backend_consumer_via_backend_composite.tif");

    // The direct path: `FusionConsumer` driven by the scheduler's own host
    // read loop.
    {
        let scheduler =
            TileScheduler::new(&stack, stack_ctx, spp, &options).expect("scheduler must build");
        let consumer = FusionConsumer::create(
            &Laplacian,
            values.clone(),
            &direct_path,
            None,
            FusionGeometry {
                width,
                height,
                samples_per_pixel: spp,
                frame_count: frame_count as u32,
                crop: None,
                description: None,
            },
        )
        .expect("create must succeed");
        scheduler
            .run(&focusforge_core::tile::NoopPreprocess, &consumer)
            .expect("run must succeed");
        consumer.finish().expect("finish must succeed");
    }

    // The pixel-supplying path: the identical algorithm and geometry, but
    // through `BackendFusionConsumer` + the local `DummyBackend`, with the
    // scheduler's own read loop skipped entirely.
    {
        let scheduler =
            TileScheduler::new(&stack, stack_ctx, spp, &options).expect("scheduler must build");
        let sink = FusionConsumer::create(
            &Laplacian,
            values.clone(),
            &via_backend_path,
            None,
            FusionGeometry {
                width,
                height,
                samples_per_pixel: spp,
                frame_count: frame_count as u32,
                crop: None,
                description: None,
            },
        )
        .expect("create must succeed");
        let backend = DummyBackend;
        let transforms = vec![Transform::identity(); frame_count];
        let consumer = BackendFusionConsumer::new(
            &backend,
            &stack,
            (width, height),
            transforms,
            spp,
            5,
            &sink,
        );
        scheduler
            .run(&focusforge_core::tile::NoopPreprocess, &consumer)
            .expect("run must succeed");
        sink.finish().expect("finish must succeed");
    }

    let direct_bytes = std::fs::read(&direct_path).expect("read direct composite");
    let via_backend_bytes = std::fs::read(&via_backend_path).expect("read via-backend composite");
    assert_eq!(
        direct_bytes, via_backend_bytes,
        "BackendFusionConsumer's composite must be byte-identical to FusionConsumer's"
    );
}

#[test]
fn a_non_noop_preprocess_is_refused_by_name_for_a_pixel_supplying_consumer() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let stack_ctx = StackContext::new(stack.len(), width, height, spp);
    let options = TileSchedulerOptions::new(scratch_base("refuse_non_noop"));
    let scheduler =
        TileScheduler::new(&stack, stack_ctx, spp, &options).expect("scheduler must build");

    let err = scheduler
        .run(&NotNoopPreprocess, &OwnPixelsProbe)
        .expect_err("a non-noop preprocess must be refused for a pixel-supplying consumer");
    assert!(
        matches!(err, TileError::PixelSupplyingConsumerNeedsNoopPreprocess),
        "expected PixelSupplyingConsumerNeedsNoopPreprocess, got {err:?}"
    );
}
