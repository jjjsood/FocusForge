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
    NoopPreprocess, Rect, SampleFormat, Tile, TileConsumer, TileJob, TileScheduler,
    TileSchedulerOptions,
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

/// A minimal, GPU-free [`TileBackend`]: `warp_fuse_tile` composes a
/// [`WarpingReader`] over the reader (so an `f32` reader fuses as `f32`) with
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
        frame: (u32, u32),
        transforms: &[Transform],
        rect: Rect,
        samples_per_pixel: u32,
        levels: u32,
    ) -> Result<TileOutput, AlgorithmError> {
        laplacian_over(
            reader,
            frame,
            transforms,
            rect,
            samples_per_pixel,
            levels,
            |ctx, cfg| Laplacian.fuse(ctx, cfg),
        )
    }

    fn warp_fuse_tile_f32(
        &self,
        reader: &dyn TileReader,
        frame: (u32, u32),
        transforms: &[Transform],
        rect: Rect,
        samples_per_pixel: u32,
        levels: u32,
    ) -> Result<TileOutput<f32>, AlgorithmError> {
        laplacian_over(
            reader,
            frame,
            transforms,
            rect,
            samples_per_pixel,
            levels,
            |ctx, cfg| Laplacian.fuse_f32(ctx, cfg),
        )
    }
}

/// `fuse` over a [`TileCtx`] of `reader` warped by `transforms`, with the
/// Laplacian depth `levels`.
fn laplacian_over<O>(
    reader: &dyn TileReader,
    frame: (u32, u32),
    transforms: &[Transform],
    rect: Rect,
    samples_per_pixel: u32,
    levels: u32,
    fuse: impl FnOnce(&TileCtx<'_>, &ParamValues) -> Result<TileOutput<O>, AlgorithmError>,
) -> Result<TileOutput<O>, AlgorithmError> {
    let warping = WarpingReader::new(reader, frame, samples_per_pixel, transforms.to_vec());
    let ctx = TileCtx::on_demand(
        rect,
        transforms.len(),
        samples_per_pixel,
        &warping,
        &NoopPreprocess,
    );
    let mut cfg = ParamValues::new();
    cfg.insert("levels".to_string(), ParamValue::Int(levels as i64));
    fuse(&ctx, &cfg)
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

impl LosingBackend {
    /// Records `rect` and loses the device once `healthy` dispatches are used.
    fn dispatch(&self, rect: Rect) -> Result<(), AlgorithmError> {
        self.rects.lock().unwrap().push(rect);
        if self.dispatches.fetch_add(1, Ordering::SeqCst) >= self.healthy {
            self.lost.store(true, Ordering::SeqCst);
            return Err(AlgorithmError::Failed {
                algorithm: "laplacian".to_string(),
                detail: "device lost".to_string(),
            });
        }
        Ok(())
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
        self.dispatch(rect)?;
        laplacian_over(
            reader,
            frame,
            transforms,
            rect,
            samples_per_pixel,
            levels,
            |ctx, cfg| Laplacian.fuse(ctx, cfg),
        )
    }

    fn warp_fuse_tile_f32(
        &self,
        reader: &dyn TileReader,
        frame: (u32, u32),
        transforms: &[Transform],
        rect: Rect,
        samples_per_pixel: u32,
        levels: u32,
    ) -> Result<TileOutput<f32>, AlgorithmError> {
        self.dispatch(rect)?;
        laplacian_over(
            reader,
            frame,
            transforms,
            rect,
            samples_per_pixel,
            levels,
            |ctx, cfg| Laplacian.fuse_f32(ctx, cfg),
        )
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

/// Single-channel frames in memory, textured so each frame is sharp in its
/// own band of columns and highlights reach above 65535. Served as `format`:
/// `F32` keeps the highlights, `U16` clips them at the read.
struct Frames {
    width: u32,
    height: u32,
    frames: Vec<Vec<f32>>,
    format: SampleFormat,
}

impl Frames {
    fn new(width: u32, height: u32, frame_count: usize, format: SampleFormat) -> Self {
        let frames = (0..frame_count)
            .map(|f| {
                let centre = width as f32 * (f as f32 + 0.5) / frame_count as f32;
                (0..height)
                    .flat_map(|y| (0..width).map(move |x| (x, y)))
                    .map(|(x, y)| {
                        let sharp = 1.0 / (1.0 + ((x as f32 - centre) / 40.0).powi(2));
                        36_000.0
                            + 34_000.0
                                * sharp
                                * (0.9 * x as f32 + f as f32).sin()
                                * (0.8 * y as f32).cos()
                    })
                    .collect()
            })
            .collect();
        Frames {
            width,
            height,
            frames,
            format,
        }
    }

    fn copy<T: Copy>(&self, frame: usize, rect: Rect, out: &mut [T], map: impl Fn(f32) -> T) {
        for row in 0..rect.height as usize {
            let from = (rect.y as usize + row) * self.width as usize + rect.x as usize;
            for col in 0..rect.width as usize {
                out[row * rect.width as usize + col] = map(self.frames[frame][from + col]);
            }
        }
    }
}

impl TileReader for Frames {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        self.copy(frame, rect, out, |v| v.clamp(0.0, 65_535.0).round() as u16);
        Ok(())
    }

    fn sample_format(&self) -> SampleFormat {
        self.format
    }

    fn read_tile_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        self.copy(frame, rect, out, |v| v);
        Ok(())
    }
}

const F32_LEVELS: u32 = 2;

/// Fuses `reader` into a TIFF at `path`, on the host or through `backend`.
/// Returns the tiles planned and the tiles the consumer finished on the host.
fn fuse_frames(
    reader: &Frames,
    label: &str,
    backend: Option<&dyn TileBackend>,
) -> (PathBuf, usize, usize) {
    let (width, height) = (reader.width, reader.height);
    let frame_count = reader.frames.len();
    let stack_ctx = StackContext::new(frame_count, width, height, 1);
    let mut values = ParamValues::new();
    values.insert("levels".to_string(), ParamValue::Int(F32_LEVELS as i64));
    let mut options = TileSchedulerOptions::new(scratch_base(label));
    options.tile_size = 112;
    options.levels = F32_LEVELS;
    let path =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("backend_consumer_{label}.tif"));
    let sink = FusionConsumer::create_as(
        &Laplacian,
        values,
        &path,
        None,
        FusionGeometry {
            width,
            height,
            samples_per_pixel: 1,
            frame_count: frame_count as u32,
            crop: None,
            description: None,
        },
        reader.sample_format(),
    )
    .expect("create must succeed");
    let scheduler =
        TileScheduler::new(reader, stack_ctx, 1, &options).expect("scheduler must build");
    let planned = scheduler.plan().len();
    let fallback = match backend {
        None => {
            scheduler
                .run(&NoopPreprocess, &sink)
                .expect("run must succeed");
            0
        }
        Some(backend) => {
            let consumer = BackendFusionConsumer::new(
                backend,
                reader,
                (width, height),
                vec![Transform::identity(); frame_count],
                1,
                F32_LEVELS,
                &sink,
            );
            scheduler
                .run(&NoopPreprocess, &consumer)
                .expect("a lost device must not fail the run");
            consumer.fallback_tiles()
        }
    };
    sink.finish().expect("finish must succeed");
    (path, planned, fallback)
}

#[test]
fn dummy_backend_f32_matches_host_fusion() {
    let reader = Frames::new(260, 190, 3, SampleFormat::F32);
    let (host, planned, _) = fuse_frames(&reader, "f32_host", None);
    assert!(planned >= 3, "tile seams must be crossed: {planned} tiles");
    let (via_backend, _, fallback) = fuse_frames(&reader, "f32_backend", Some(&DummyBackend));
    assert_eq!(fallback, 0);
    let staged = TiffStack::open_staged(&host).expect("the composite reopens");
    assert_eq!(staged.sample_format(), SampleFormat::F32);
    let host_bytes = std::fs::read(&host).expect("read host composite");
    assert_eq!(
        host_bytes,
        std::fs::read(&via_backend).expect("read backend composite"),
        "the backend path and the host path fuse the same f32 frames to the same file"
    );

    // The same frames clipped at the read fuse differently: the f32 path is live.
    let clipped = Frames::new(260, 190, 3, SampleFormat::U16);
    let (clipped_path, _, _) = fuse_frames(&clipped, "f32_clipped", None);
    assert_ne!(
        host_bytes,
        std::fs::read(&clipped_path).expect("read clipped composite")
    );
}

#[test]
fn dummy_backend_f32_device_loss_finishes_on_the_host() {
    let reader = Frames::new(260, 190, 3, SampleFormat::F32);
    let (host, planned, _) = fuse_frames(&reader, "f32_loss_host", None);
    let backend = LosingBackend::new(1);
    let (lossy, _, fallback) = fuse_frames(&reader, "f32_loss_backend", Some(&backend));
    assert_eq!(
        fallback,
        planned - 1,
        "every tile after the first finishes on the host"
    );
    assert_eq!(
        std::fs::read(&host).expect("read host composite"),
        std::fs::read(&lossy).expect("read fallback composite"),
        "a run finished on the host after a device loss equals a pure host run"
    );
}
