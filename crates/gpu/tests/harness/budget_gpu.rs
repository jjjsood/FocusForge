// SPDX-License-Identifier: GPL-3.0-or-later

//! GPU-gated coverage of the VRAM working-set budget (`focusforge_gpu::budget`). A deliberately
//! tiny device budget must serialize `warp_fuse_tile` to one tile in flight
//! at a time while the pipeline still produces the same result
//! (`tiny_budget_serializes_tiles_and_keeps_the_result`), and the device's
//! own allocator report is recorded next to `device_bytes_per_tile`'s
//! prediction, once, as a diagnostic rather than a bound
//! (`allocator_report_matches_the_prediction`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use focusforge_core::algorithm::{AlgorithmError, TileReader};
use focusforge_core::align::Transform;
use focusforge_core::gpu::TileBackend;
use focusforge_core::io::TiffStack;
use focusforge_core::params::StackContext;
use focusforge_core::tile::{
    NoopPreprocess, Rect, TileConsumer, TileJob, TileScheduler, TileSchedulerOptions,
};
use focusforge_gpu::budget::device_bytes_per_tile;
use focusforge_gpu::harness::{GpuBackend, gpu_backend};

use crate::common;

const LEVELS: u32 = 5;

/// The whole-pipeline parity test's mild non-identity transform, so the
/// warp stage sits inside the measured path.
fn non_identity_transforms(frame_count: usize) -> Vec<Transform> {
    let t = Transform::similarity(1.005, 0.0075, 0.4, -0.25);
    (0..frame_count)
        .map(|i| if i == 0 { Transform::identity() } else { t })
        .collect()
}

type TileKey = (u32, u32, u32, u32);
type TileOutputSamples = (Vec<u16>, Vec<u16>);

/// Calls [`TileBackend::warp_fuse_tile`] per job and collects
/// `(samples, zmap)` per read rect; no TIFF sink.
struct CollectingConsumer<'a> {
    backend: &'a dyn TileBackend,
    reader: &'a dyn TileReader,
    frame: (u32, u32),
    transforms: Vec<Transform>,
    samples_per_pixel: u32,
    levels: u32,
    out: Mutex<BTreeMap<TileKey, TileOutputSamples>>,
}

impl TileConsumer for CollectingConsumer<'_> {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        let rect = job.read_rect();
        let output = self.backend.warp_fuse_tile(
            self.reader,
            self.frame,
            &self.transforms,
            rect,
            self.samples_per_pixel,
            self.levels,
        )?;
        let key = (rect.x, rect.y, rect.width, rect.height);
        self.out
            .lock()
            .unwrap()
            .insert(key, (output.tile.samples, output.zmap));
        Ok(())
    }

    fn supplies_own_pixels(&self) -> bool {
        true
    }
}

fn scratch_base(label: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("budget_gpu_scratch_{label}"))
}

/// Runs the whole `fixtures/mantis-512` plan (4 tiles at `tile_size = 512,
/// levels = 5` -- the same geometry `parity_pipeline.rs`'s `run_gpu`
/// establishes) through `backend`, returning every tile's composite samples
/// and Z-Map keyed by its read rect.
fn run_plan(backend: &dyn TileBackend, label: &str) -> BTreeMap<TileKey, TileOutputSamples> {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("open mantis-512 fixture");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let stack_ctx = StackContext::new(frame_count, width, height, spp);

    let mut options = TileSchedulerOptions::new(scratch_base(label));
    options.levels = LEVELS;
    let scheduler =
        TileScheduler::new(&stack, stack_ctx, spp, &options).expect("scheduler must build");

    let consumer = CollectingConsumer {
        backend,
        reader: &stack,
        frame: (width, height),
        transforms: non_identity_transforms(frame_count),
        samples_per_pixel: spp,
        levels: LEVELS,
        out: Mutex::new(BTreeMap::new()),
    };
    scheduler
        .run(&NoopPreprocess, &consumer)
        .expect("gpu run must succeed");
    consumer.out.into_inner().unwrap()
}

#[test]
fn tiny_budget_serializes_tiles_and_keeps_the_result() {
    if focusforge_gpu::device::shared_device_or_skip().is_none() {
        return;
    }
    let default_backend = gpu_backend().expect("gpu backend");
    let default_result = run_plan(default_backend, "default");
    // Diagnostic only: the test binary runs its tests in parallel over one
    // device, so the default backend's own peak/limit are printed, never
    // asserted.
    println!(
        "default backend: peak_in_flight={}",
        default_backend.budget().peak_in_flight()
    );

    let tiny_backend = GpuBackend::with_max_device_bytes(1).expect("tiny gpu backend");
    let tiny_result = run_plan(&tiny_backend, "tiny");

    // The fixture-scale per-tile byte cost (8 frames, 512x512x3, warped).
    let bytes = device_bytes_per_tile(8, 512, 512, 3, LEVELS, true);
    assert_eq!(tiny_backend.budget().peak_in_flight(), 1);
    assert_eq!(tiny_backend.budget().limit_for(bytes), 1);
    assert_eq!(
        default_result, tiny_result,
        "a tiny device budget must not change a single tile's composite samples or Z-Map"
    );
}

/// The real [`focusforge_gpu::vram::WgpuAllocProbe`], through the process
/// singleton (`gpu_backend`): the settled budget is positive and its
/// provenance is `Host` (a probe settled) or `HostFallback`, never
/// `Explicit`, since no `--max-vram` ceiling applies here.
#[test]
fn wgpu_alloc_probe_settles_and_reports_provenance() {
    if focusforge_gpu::device::shared_device_or_skip().is_none() {
        return;
    }
    let backend = gpu_backend().expect("gpu backend");
    let device_budget = backend.device_budget();
    println!(
        "wgpu_alloc_probe_settles_and_reports_provenance: bytes={} provenance={:?}",
        device_budget.bytes(),
        device_budget.provenance()
    );
    assert!(
        device_budget.bytes() > 0,
        "a settled device budget must be a positive byte count"
    );
    assert!(
        matches!(
            device_budget.provenance(),
            focusforge_core::params::Provenance::Host
                | focusforge_core::params::Provenance::HostFallback
        ),
        "the singleton backend (no --max-vram applied) must report Host or HostFallback \
         provenance, got {:?}",
        device_budget.provenance()
    );
}

#[test]
fn allocator_report_matches_the_prediction() {
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let backend = gpu_backend().expect("gpu backend");

    let stack = TiffStack::open(common::mantis_512_dir()).expect("open mantis-512 fixture");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let transforms = non_identity_transforms(frame_count);
    let rect = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let predicted = device_bytes_per_tile(frame_count, width, height, spp, LEVELS, true);

    let baseline = shared.device().generate_allocator_report();

    // A background thread samples the allocator report while the one
    // dispatch runs synchronously on this thread, so a mid-tile peak isn't
    // missed between the last dispatch and the tail readback.
    let running = std::sync::Arc::new(AtomicBool::new(true));
    let peak_seen = std::sync::Arc::new(AtomicU64::new(0));
    let sampler = {
        let running = running.clone();
        let peak_seen = peak_seen.clone();
        std::thread::spawn(move || {
            while running.load(Ordering::SeqCst) {
                if let Some(report) = shared.device().generate_allocator_report() {
                    peak_seen.fetch_max(report.total_allocated_bytes, Ordering::SeqCst);
                }
                std::thread::yield_now();
            }
        })
    };

    let output = backend
        .warp_fuse_tile(&stack, (width, height), &transforms, rect, spp, LEVELS)
        .expect("warp_fuse_tile must succeed");
    assert_eq!(output.zmap.len(), (width * height) as usize);

    running.store(false, Ordering::SeqCst);
    sampler.join().expect("sampler thread must not panic");
    let after = shared.device().generate_allocator_report();

    match (baseline, after) {
        (Some(baseline), Some(after)) => {
            let peak = peak_seen
                .load(Ordering::SeqCst)
                .max(after.total_allocated_bytes);
            let delta = peak.saturating_sub(baseline.total_allocated_bytes);
            println!(
                "allocator report (device-wide, concurrent tests share this device -- a \
                 measurement, not a bound): baseline={} peak={} delta={} predicted={}",
                baseline.total_allocated_bytes, peak, delta, predicted
            );
        }
        _ => {
            println!(
                "allocator report unavailable: Device::generate_allocator_report() returned \
                 None on this backend (skip reason: the adapter uses the portability path, \
                 not native Vulkan)"
            );
        }
    }
}

/// A reader no refused tile may ever reach.
struct UnreadableReader;

impl TileReader for UnreadableReader {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        _out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        panic!("a refused tile must not read frame {frame} rect {rect:?}");
    }
}

#[test]
fn a_rect_past_the_dispatch_limit_is_refused_by_every_entry_point() {
    if focusforge_gpu::device::shared_device_or_skip().is_none() {
        return;
    }
    let backend = gpu_backend().expect("gpu backend");
    let spp = 3;
    let side = backend.max_tile_size(spp) + 1;
    let rect = Rect {
        x: 0,
        y: 0,
        width: side,
        height: side,
    };
    let transforms = non_identity_transforms(2);
    let reads_before = focusforge_gpu::transfer::readback_count();

    let refusals = [
        (
            "self_check",
            backend
                .self_check(&UnreadableReader, 0, rect, spp, 1.0)
                .err()
                .map(|err| err.to_string()),
        ),
        (
            "fuse_tile",
            backend
                .fuse_tile(&UnreadableReader, 2, rect, spp, LEVELS)
                .err()
                .map(|err| err.to_string()),
        ),
        (
            "warp_tile",
            backend
                .warp_tile(
                    &UnreadableReader,
                    1,
                    (side, side),
                    spp,
                    rect,
                    &transforms[1],
                )
                .err()
                .map(|err| err.to_string()),
        ),
        (
            "warp_fuse_tile",
            backend
                .warp_fuse_tile(
                    &UnreadableReader,
                    (side, side),
                    &transforms,
                    rect,
                    spp,
                    LEVELS,
                )
                .err()
                .map(|err| err.to_string()),
        ),
    ];
    for (entry, refusal) in refusals {
        let message = refusal.unwrap_or_else(|| panic!("{entry} accepted a {side}x{side} rect"));
        assert!(
            message.contains("max_compute_workgroups_per_dimension"),
            "{entry} must name the dispatch limit, got: {message}"
        );
    }
    assert_eq!(focusforge_gpu::transfer::readback_count(), reads_before);
}

#[test]
fn identical_tiles_cost_identical_transfers_and_allocations() {
    let Some(_shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    focusforge_core::timing::enable_trace_accounting();
    let backend = gpu_backend().expect("gpu backend");
    let stack = TiffStack::open(common::mantis_512_dir()).expect("open mantis-512 fixture");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let transforms = non_identity_transforms(stack.len());
    let rect = Rect {
        x: 0,
        y: 0,
        width: 256,
        height: 256,
    };

    let mut deltas = Vec::new();
    for _ in 0..2 {
        let before = focusforge_core::timing::timing_snapshot().expect("accounting is enabled");
        backend
            .warp_fuse_tile(&stack, (width, height), &transforms, rect, spp, LEVELS)
            .expect("warp_fuse_tile must succeed");
        let after = focusforge_core::timing::timing_snapshot().expect("accounting is enabled");
        deltas.push((
            after.buffer_creations - before.buffer_creations,
            after.upload_bytes - before.upload_bytes,
            after.readback_bytes - before.readback_bytes,
        ));
    }

    assert_eq!(deltas[0], deltas[1], "the same tile must cost the same");
    let (creations, uploads, readbacks) = deltas[0];
    assert!(creations > 0 && uploads > 0);
    let pixels = u64::from(rect.width) * u64::from(rect.height);
    let composite = focusforge_gpu::pack::packed_len(pixels as usize * spp as usize) as u64;
    assert_eq!(
        readbacks,
        composite + 8 * pixels,
        "a tile reads back its packed composite and its Z-Map"
    );
}
