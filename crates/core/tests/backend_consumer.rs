// SPDX-License-Identifier: GPL-3.0-or-later

//! Default-gate, GPU-free coverage of the tile scheduler's "a consumer
//! supplies its own pixels" hook (`crates/core/src/tile/scheduler.rs`'s
//! module docs) and of [`BackendFusionConsumer`], driven end to end through
//! a local, test-only [`TileBackend`] -- no `wgpu`, no feature flag.

mod common;

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use focusforge_core::algorithm::{AlgorithmError, StackAlgorithm, TileCtx, TileOutput, TileReader};
use focusforge_core::align::WarpingReader;
use focusforge_core::align::warp::WarpedTile;
use focusforge_core::align::{AlignError, SourceFootprint, Transform};
use focusforge_core::fuse::laplacian::Laplacian;
use focusforge_core::fuse::{BackendFusionConsumer, FusionConsumer, FusionGeometry};
use focusforge_core::gpu::{BackendInfo, TileBackend};
use focusforge_core::io::TiffStack;
use focusforge_core::params::{ParamValue, ParamValues, StackContext};
use focusforge_core::tile::{
    Rect, Tile, TileConsumer, TileJob, TileScheduler, TileSchedulerOptions,
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

/// `TileConsumer::abort` must leave no half-covered output file open: both
/// writers are taken out of their mutexes and dropped. The trait default is a
/// no-op, so a wrapper that does not override `abort` loses that guarantee.
/// Observed through `FusionConsumer::finish`'s
/// `expect("composite writer present")`: after `abort` there is no writer.
#[test]
fn abort_drops_the_sinks_writers_through_the_backend_consumer() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let composite_path =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("backend_consumer_abort_composite.partial");

    let mut values = ParamValues::new();
    values.insert("levels".to_string(), ParamValue::Int(5));
    let sink = FusionConsumer::create(
        &Laplacian,
        values,
        &composite_path,
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
    let consumer = BackendFusionConsumer::new(
        &backend,
        &stack,
        (width, height),
        vec![Transform::identity(); frame_count],
        spp,
        5,
        &sink,
    );
    consumer.abort();

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let finished = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sink.finish()));
    std::panic::set_hook(previous_hook);
    assert!(
        finished.is_err(),
        "abort must have dropped the composite writer, leaving finish nothing to close"
    );

    let _ = std::fs::remove_file(&composite_path);
}

/// A backend that serves exactly two tiles and then reports its device
/// lost: the third `warp_fuse_tile` call, and every one after it, sets the
/// lost flag and fails. Its own arithmetic is the host's -- a warp through
/// [`WarpingReader`] then [`Laplacian::fuse`] -- so a composite mixing
/// backend tiles with fallback tiles equals a pure host run byte for byte.
struct LosingBackend {
    dispatches: AtomicUsize,
    lost: AtomicBool,
    /// How many dispatches succeed before the device is lost.
    healthy: usize,
    /// The read rect of every `warp_fuse_tile` call, in call order.
    rects: Mutex<Vec<Rect>>,
}

impl LosingBackend {
    fn new(healthy: usize) -> Self {
        LosingBackend {
            dispatches: AtomicUsize::new(0),
            lost: AtomicBool::new(false),
            healthy,
            rects: Mutex::new(Vec::new()),
        }
    }
}

const HEALTHY_TILES: usize = 2;

impl TileBackend for LosingBackend {
    fn info(&self) -> BackendInfo {
        BackendInfo {
            name: "test-losing".into(),
            backend: "test-losing".into(),
            driver: "test-losing".into(),
            driver_info: "test-losing".into(),
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

    fn device_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    fn warp_fuse_tile(
        &self,
        reader: &dyn TileReader,
        frame: (u32, u32),
        transforms: &[Transform],
        rect: Rect,
        samples_per_pixel: u32,
        levels: u32,
    ) -> Result<TileOutput, AlgorithmError> {
        self.rects.lock().unwrap().push(rect);
        if self.dispatches.fetch_add(1, Ordering::SeqCst) >= self.healthy {
            self.lost.store(true, Ordering::SeqCst);
            return Err(AlgorithmError::Failed {
                algorithm: "laplacian".to_string(),
                detail: "device lost".to_string(),
            });
        }
        let warping = WarpingReader::new(reader, frame, samples_per_pixel, transforms.to_vec());
        let mut tiles = Vec::with_capacity(transforms.len());
        let mut coverage = Vec::with_capacity(transforms.len());
        for i in 0..transforms.len() {
            let mut samples =
                vec![0u16; rect.width as usize * rect.height as usize * samples_per_pixel as usize];
            coverage.push(warping.read_tile_coverage_into(i, rect, &mut samples)?);
            tiles.push(Tile {
                rect,
                samples_per_pixel,
                samples,
            });
        }
        let ctx = TileCtx::new(rect, &tiles).with_coverage(&coverage);
        let mut cfg = ParamValues::new();
        cfg.insert("levels".to_string(), ParamValue::Int(levels as i64));
        Laplacian.fuse(&ctx, &cfg)
    }
}

/// A mild rotation-plus-scale for every frame but the reference, so the
/// fallback goes through the warp path rather than `WarpingReader`'s
/// identity short-circuit.
fn non_identity_transforms(frame_count: usize) -> Vec<Transform> {
    let t = Transform::similarity(1.005, 0.0075, 0.4, -0.25);
    (0..frame_count)
        .map(|i| if i == 0 { Transform::identity() } else { t })
        .collect()
}

/// A device lost part-way through a run finishes that run on the host, with
/// a composite equal byte for byte to one produced entirely on the host,
/// and reports how many tiles it read frames for.
#[test]
fn device_loss_finishes_the_run_on_the_host() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let stack_ctx = StackContext::new(frame_count, width, height, spp);
    let transforms = non_identity_transforms(frame_count);

    // Three pyramid levels, not the five the tests above use: five would
    // need a tile larger than the fixture is wide, and a tile size small
    // enough to plan several tiles over a 512 px fixture is what puts both
    // the backend path and the host path in one run.
    const LEVELS: u32 = 3;
    let mut values = ParamValues::new();
    values.insert("levels".to_string(), ParamValue::Int(LEVELS as i64));

    let mut options = TileSchedulerOptions::new(scratch_base("device_loss"));
    options.tile_size = 200;
    options.levels = LEVELS;

    let geometry = FusionGeometry {
        width,
        height,
        samples_per_pixel: spp,
        frame_count: frame_count as u32,
        crop: None,
        description: None,
    };

    let host_path =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("backend_consumer_device_loss_host.tif");
    let fallback_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("backend_consumer_device_loss_fallback.tif");

    // The whole run on the host, through a warping reader carrying the same
    // transforms -- the oracle.
    let planned = {
        let warping = WarpingReader::new(&stack, (width, height), spp, transforms.clone());
        let scheduler =
            TileScheduler::new(&warping, stack_ctx, spp, &options).expect("scheduler must build");
        let planned = scheduler.plan().len();
        let consumer = FusionConsumer::create(
            &Laplacian,
            values.clone(),
            &host_path,
            None,
            geometry.clone(),
        )
        .expect("create must succeed");
        scheduler
            .run(&focusforge_core::tile::NoopPreprocess, &consumer)
            .expect("run must succeed");
        consumer.finish().expect("finish must succeed");
        planned
    };
    assert!(
        planned > HEALTHY_TILES,
        "the plan must hold more tiles than the backend serves, got {planned}"
    );

    let fallback_tiles = {
        let scheduler =
            TileScheduler::new(&stack, stack_ctx, spp, &options).expect("scheduler must build");
        let sink =
            FusionConsumer::create(&Laplacian, values.clone(), &fallback_path, None, geometry)
                .expect("create must succeed");
        let backend = LosingBackend::new(HEALTHY_TILES);
        let consumer = BackendFusionConsumer::new(
            &backend,
            &stack,
            (width, height),
            transforms.clone(),
            spp,
            LEVELS,
            &sink,
        );
        scheduler
            .run(&focusforge_core::tile::NoopPreprocess, &consumer)
            .expect("a lost device must not fail the run");
        let fallback_tiles = consumer.fallback_tiles();
        drop(consumer);
        sink.finish().expect("finish must succeed");
        fallback_tiles
    };

    assert_eq!(
        fallback_tiles,
        planned - HEALTHY_TILES,
        "every tile past the two the backend served must have been read on the host"
    );
    assert_eq!(
        std::fs::read(&host_path).expect("read host composite"),
        std::fs::read(&fallback_path).expect("read fallback composite"),
        "a run finished on the host after a device loss must equal a pure host run"
    );
}

/// A tile whose warped footprint reaches past a frame edge still goes to the
/// backend: none is read on the host while the device is healthy.
#[test]
fn tiles_with_a_clamped_footprint_reach_the_backend() {
    let stack = open_stack();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let stack_ctx = StackContext::new(frame_count, width, height, spp);
    let mut transforms = vec![Transform::identity(); frame_count];
    transforms[3] = Transform::translation(-12.5, 0.0);

    const LEVELS: u32 = 3;
    let mut values = ParamValues::new();
    values.insert("levels".to_string(), ParamValue::Int(LEVELS as i64));
    let mut options = TileSchedulerOptions::new(scratch_base("clamped_footprint"));
    options.tile_size = 200;
    options.levels = LEVELS;
    let geometry = FusionGeometry {
        width,
        height,
        samples_per_pixel: spp,
        frame_count: frame_count as u32,
        crop: None,
        description: None,
    };
    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let host_path = tmp.join("backend_consumer_clamped_host.tif");
    let routed_path = tmp.join("backend_consumer_clamped_routed.tif");

    let planned = {
        let warping = WarpingReader::new(&stack, (width, height), spp, transforms.clone());
        let scheduler =
            TileScheduler::new(&warping, stack_ctx, spp, &options).expect("scheduler must build");
        let planned = scheduler.plan().len();
        let consumer = FusionConsumer::create(
            &Laplacian,
            values.clone(),
            &host_path,
            None,
            geometry.clone(),
        )
        .expect("create must succeed");
        scheduler
            .run(&focusforge_core::tile::NoopPreprocess, &consumer)
            .expect("run must succeed");
        consumer.finish().expect("finish must succeed");
        planned
    };

    let scheduler =
        TileScheduler::new(&stack, stack_ctx, spp, &options).expect("scheduler must build");
    let sink = FusionConsumer::create(&Laplacian, values, &routed_path, None, geometry)
        .expect("create must succeed");
    let backend = LosingBackend::new(usize::MAX);
    let consumer = BackendFusionConsumer::new(
        &backend,
        &stack,
        (width, height),
        transforms.clone(),
        spp,
        LEVELS,
        &sink,
    );
    scheduler
        .run(&focusforge_core::tile::NoopPreprocess, &consumer)
        .expect("run must succeed");
    assert_eq!(
        consumer.fallback_tiles(),
        0,
        "a healthy device leaves no tile to the host"
    );
    drop(consumer);
    sink.finish().expect("finish must succeed");

    let rects = backend.rects.lock().unwrap();
    assert_eq!(
        rects.len(),
        planned,
        "every planned tile reaches the backend"
    );
    assert!(
        rects.iter().any(|rect| matches!(
            transforms[3].source_footprint(*rect, (width, height)),
            Ok(SourceFootprint::Covers { clamped: true, .. } | SourceFootprint::Outside)
        )),
        "the plan must include a tile whose footprint reaches past the source frame"
    );
    assert_eq!(
        std::fs::read(&host_path).unwrap(),
        std::fs::read(&routed_path).unwrap(),
        "a coverage-aware backend must reproduce the pure host run"
    );
}
