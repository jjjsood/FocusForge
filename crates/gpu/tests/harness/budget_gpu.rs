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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use focusforge_core::algorithm::{AlgorithmError, TileReader};
use focusforge_core::align::Transform;
use focusforge_core::gpu::TileBackend;
use focusforge_core::io::TiffStack;
use focusforge_core::params::StackContext;
use focusforge_core::tile::{
    NoopPreprocess, Rect, SampleFormat, TileConsumer, TileJob, TileScheduler,
};
use focusforge_gpu::budget::device_bytes_per_tile;
use focusforge_gpu::device::SharedDevice;
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

/// Runs the whole `fixtures/mantis-512` plan (2x2 tiles at `tile_size = 512,
/// levels = 5`, 160 px margin -- the same geometry `parity_pipeline.rs`'s `run_gpu`
/// establishes) through `backend`, returning every tile's composite samples
/// and Z-Map keyed by its read rect.
fn run_plan(backend: &dyn TileBackend, label: &str) -> BTreeMap<TileKey, TileOutputSamples> {
    let stack = TiffStack::open(common::mantis_512_dir()).expect("open mantis-512 fixture");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let stack_ctx = StackContext::new(frame_count, width, height, spp);

    let options = crate::parity_pipeline::laplacian_options(scratch_base(label));
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

    // The fixture-scale per-tile byte cost (512x512x3, warped).
    let bytes = device_bytes_per_tile(512, 512, 3, LEVELS, true, SampleFormat::U16);
    assert_eq!(tiny_backend.budget().peak_in_flight(), 1);
    assert_eq!(tiny_backend.budget().limit_for(bytes), 1);
    assert_eq!(
        default_result, tiny_result,
        "a tiny device budget must not change a single tile's composite samples or Z-Map"
    );
}

/// A device budget exactly one tile's model bytes asks no fallback; one byte
/// less names the tile's bytes and the CPU path; identity transforms cost less
/// than warped ones.
#[test]
fn a_tile_above_the_device_budget_reports_a_shortfall() {
    if focusforge_gpu::device::shared_device_or_skip().is_none() {
        return;
    }
    let transforms = non_identity_transforms(2);
    let bytes = device_bytes_per_tile(512, 512, 3, LEVELS, true, SampleFormat::U16);

    let at_budget = GpuBackend::with_max_device_bytes(bytes).expect("gpu backend");
    assert_eq!(
        at_budget.tile_budget_shortfall(512, 512, 3, LEVELS, &transforms, SampleFormat::U16),
        None
    );

    let below = GpuBackend::with_max_device_bytes(bytes - 1).expect("gpu backend");
    let message = below
        .tile_budget_shortfall(512, 512, 3, LEVELS, &transforms, SampleFormat::U16)
        .expect("a budget one byte short is a shortfall");
    assert!(message.contains(&format!("needs {bytes} B")), "{message}");
    assert!(message.contains("CPU path"), "{message}");

    // Identity transforms cost less than warped ones.
    let identity = vec![Transform::identity(); 2];
    assert_eq!(
        below.tile_budget_shortfall(512, 512, 3, LEVELS, &identity, SampleFormat::U16),
        None
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
    let predicted = device_bytes_per_tile(width, height, spp, LEVELS, true, SampleFormat::U16);

    let (sample, output) = sample_allocator(shared, || {
        backend
            .warp_fuse_tile(&stack, (width, height), &transforms, rect, spp, LEVELS)
            .expect("warp_fuse_tile must succeed")
    });
    assert_eq!(output.zmap.len(), (width * height) as usize);

    match sample {
        Some(sample) => {
            println!(
                "allocator report (device-wide, concurrent tests share this device -- a \
                 measurement, not a bound): baseline={} peak={} delta={} predicted={}",
                sample.baseline,
                sample.peak,
                sample.peak.saturating_sub(sample.baseline),
                predicted
            );
        }
        None => {
            println!(
                "allocator report unavailable: Device::generate_allocator_report() returned \
                 None on this backend (skip reason: the adapter uses the portability path, \
                 not native Vulkan)"
            );
        }
    }
}

/// Serves frame `i` as frame `i % stack.len()`, so a stack of any length is
/// backed by the same few frames on disk.
struct CyclingReader<'a> {
    stack: &'a TiffStack,
}

impl TileReader for CyclingReader<'_> {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        TileReader::read_tile_into(self.stack, frame % self.stack.len(), rect, out)
    }
}

/// The device allocator's total before a run, its highest value seen while the
/// run executed, and the run's wall time.
pub(crate) struct AllocatorSample {
    pub(crate) baseline: u64,
    pub(crate) peak: u64,
    pub(crate) wall: Duration,
}

/// Runs `run` on this thread while a background thread polls the allocator
/// report, so a mid-run peak is not missed between the last dispatch and the
/// tail readback. The sample is `None` when the backend has no allocator report.
pub(crate) fn sample_allocator<T>(
    shared: &'static SharedDevice,
    run: impl FnOnce() -> T,
) -> (Option<AllocatorSample>, T) {
    let baseline = shared.device().generate_allocator_report();
    let running = Arc::new(AtomicBool::new(true));
    let peak_seen = Arc::new(AtomicU64::new(0));
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

    let started = Instant::now();
    let result = run();
    let wall = started.elapsed();

    running.store(false, Ordering::SeqCst);
    sampler.join().expect("sampler thread must not panic");
    let after = shared.device().generate_allocator_report();

    let sample = baseline
        .zip(after)
        .map(|(baseline, after)| AllocatorSample {
            baseline: baseline.total_allocated_bytes,
            peak: peak_seen
                .load(Ordering::SeqCst)
                .max(after.total_allocated_bytes),
            wall,
        });
    (sample, result)
}

/// One 512x512 warped tile fused from 8, 32 and 128 frames (frame `i` read as
/// frame `i % 8`) stays within the model, which takes no frame count; the
/// three peaks differ by under 4 MiB (one 1.57 MB allocator step that lands at
/// a different frame count per run). A device that kept per-frame buffers
/// would grow over 100 MiB across the 120 extra frames. Figures are
/// device-wide: run it alone (`--test-threads=1`).
#[test]
fn device_peak_is_flat_in_frame_count() {
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    let backend = gpu_backend().expect("gpu backend");
    let stack = TiffStack::open(common::mantis_512_dir()).expect("open mantis-512 fixture");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let reader = CyclingReader { stack: &stack };
    let rect = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };

    let predicted = device_bytes_per_tile(width, height, spp, LEVELS, true, SampleFormat::U16);
    let mut deltas = Vec::new();
    for frame_count in [8, 32, 128] {
        let transforms = non_identity_transforms(frame_count);
        let (sample, output) = sample_allocator(shared, || {
            backend
                .warp_fuse_tile(&reader, (width, height), &transforms, rect, spp, LEVELS)
                .expect("warp_fuse_tile must succeed")
        });
        assert_eq!(output.zmap.len(), (width * height) as usize);
        let Some(sample) = sample else {
            println!(
                "allocator report unavailable: Device::generate_allocator_report() returned \
                 None on this backend; the device peak cannot be measured"
            );
            return;
        };
        let delta = sample.peak.saturating_sub(sample.baseline);
        println!(
            "frames={frame_count} baseline={} peak={} delta={delta} predicted={predicted} \
             tile_wall_s={:.3}",
            sample.baseline,
            sample.peak,
            sample.wall.as_secs_f64()
        );
        deltas.push(delta);
    }

    let (min, max) = deltas
        .iter()
        .fold((u64::MAX, 0), |(min, max), &d| (min.min(d), max.max(d)));
    assert!(
        max - min < 4 << 20,
        "the device peak over baseline grew with the frame count: {deltas:?}"
    );
    assert!(
        max <= predicted,
        "a tile peaks at {max} B over baseline, above the model's {predicted} B"
    );
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

    let stage_ns = |snapshot: &focusforge_core::timing::TimingSnapshot| {
        [
            snapshot.gpu_tile_read_ns,
            snapshot.gpu_tile_warp_ns,
            snapshot.gpu_tile_upload_ns,
            snapshot.gpu_tile_pyramid_ns,
            snapshot.gpu_tile_select_ns,
            snapshot.gpu_tile_reconstruct_ns,
            snapshot.gpu_tile_readback_ns,
            snapshot.gpu_tile_wait_ns,
        ]
    };
    let mut deltas = Vec::new();
    for _ in 0..2 {
        let before = focusforge_core::timing::timing_snapshot().expect("accounting is enabled");
        backend
            .warp_fuse_tile(&stack, (width, height), &transforms, rect, spp, LEVELS)
            .expect("warp_fuse_tile must succeed");
        let after = focusforge_core::timing::timing_snapshot().expect("accounting is enabled");
        for (stage, (then, now)) in stage_ns(&before).iter().zip(stage_ns(&after)).enumerate() {
            assert!(now > *then, "stage counter {stage} did not grow: {after:?}");
        }
        deltas.push((
            after.cpu_submissions - before.cpu_submissions,
            after.buffer_creations - before.buffer_creations,
            after.upload_bytes - before.upload_bytes,
            after.readback_bytes - before.readback_bytes,
        ));
    }

    assert_eq!(deltas[0], deltas[1], "the same tile must cost the same");
    let (submits, creations, uploads, readbacks) = deltas[0];
    assert!(submits > 0 && creations > 0 && uploads > 0);
    let pixels = u64::from(rect.width) * u64::from(rect.height);
    let warped_frames = transforms
        .iter()
        .filter(|t| **t != Transform::identity())
        .count() as u64;
    assert!(
        uploads >= 24 * pixels * warped_frames,
        "{uploads} uploaded bytes do not cover the warp coordinates of {warped_frames} frames"
    );
    let composite = focusforge_gpu::pack::packed_len(pixels as usize * spp as usize) as u64;
    assert_eq!(
        readbacks,
        composite + 8 * pixels,
        "a tile reads back its packed composite and its Z-Map"
    );
}

/// The submits and buffer creations one more frame adds to a warped 256x256
/// tile do not depend on the pyramid depth: a frame costs its warp, its
/// base-plane convert and one batch for the pyramid and select passes, however
/// many levels the batch records. Counts are taken at 4 and 8 frames so the
/// per-tile fixed cost cancels.
#[test]
fn submits_per_frame_do_not_grow_with_pyramid_depth() {
    let Some(_shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    focusforge_core::timing::enable_trace_accounting();
    let backend = gpu_backend().expect("gpu backend");
    let stack = TiffStack::open(common::mantis_512_dir()).expect("open mantis-512 fixture");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let reader = CyclingReader { stack: &stack };
    let rect = Rect {
        x: 0,
        y: 0,
        width: 256,
        height: 256,
    };

    let mut increments = Vec::new();
    for levels in [3u32, 5] {
        let cost = |frame_count: usize| {
            let transforms = non_identity_transforms(frame_count);
            let before = focusforge_core::timing::timing_snapshot().expect("accounting is enabled");
            backend
                .warp_fuse_tile(&reader, (width, height), &transforms, rect, spp, levels)
                .expect("warp_fuse_tile must succeed");
            let after = focusforge_core::timing::timing_snapshot().expect("accounting is enabled");
            (
                after.cpu_submissions - before.cpu_submissions,
                after.buffer_creations - before.buffer_creations,
            )
        };
        let (submits_4, creations_4) = cost(4);
        let (submits_8, creations_8) = cost(8);
        let (extra_submits, extra_creations) = (submits_8 - submits_4, creations_8 - creations_4);
        assert_eq!(
            extra_submits % 4,
            0,
            "levels {levels}: {extra_submits} submits"
        );
        assert_eq!(
            extra_creations % 4,
            0,
            "levels {levels}: {extra_creations} creations"
        );
        let (per_frame_submits, per_frame_creations) = (extra_submits / 4, extra_creations / 4);
        assert!(
            per_frame_submits <= 3,
            "levels {levels}: {per_frame_submits} submits per frame; warp, convert and one batch allow 3"
        );
        // Warp 4, convert 2, one select uniform per level, one coverage mask per
        // level plus one for the residual.
        assert!(
            per_frame_creations <= 7 + 2 * u64::from(levels),
            "levels {levels}: {per_frame_creations} buffer creations per frame"
        );
        increments.push(per_frame_submits);
    }
    assert_eq!(
        increments[0], increments[1],
        "per-frame submits must not depend on the pyramid depth"
    );
}

/// A coverage mask with no uncovered pixel selects nothing out, so it costs no
/// eligibility upload: the tile moves the same bytes and creates the same
/// buffers as one fused without masks, and its output is identical.
#[test]
fn an_all_true_mask_costs_no_upload() {
    let Some(shared) = focusforge_gpu::device::shared_device_or_skip() else {
        return;
    };
    focusforge_core::timing::enable_trace_accounting();
    let stack = TiffStack::open(common::mantis_512_dir()).expect("open mantis-512 fixture");
    let spp = stack.header(0).samples_per_pixel;
    let frame_count = stack.len();
    let rect = Rect {
        x: 64,
        y: 32,
        width: 256,
        height: 192,
    };
    let pipelines = focusforge_gpu::fuse::FusionPipelines::new();
    let full = vec![Some(vec![true; (rect.width * rect.height) as usize]); frame_count];
    let run = |coverage: &[Option<Vec<bool>>]| {
        let before = focusforge_core::timing::timing_snapshot().expect("accounting is enabled");
        let output = focusforge_gpu::fuse::fuse_tile_gpu(
            shared,
            &stack,
            &pipelines,
            frame_count,
            rect,
            spp,
            LEVELS,
            coverage,
        )
        .expect("fuse_tile_gpu must succeed");
        let after = focusforge_core::timing::timing_snapshot().expect("accounting is enabled");
        let cost = (
            after.upload_bytes - before.upload_bytes,
            after.buffer_creations - before.buffer_creations,
        );
        (output, cost)
    };

    let (unmasked, unmasked_cost) = run(&[]);
    let (masked, masked_cost) = run(&full);

    assert_eq!(masked.tile.samples, unmasked.tile.samples);
    assert_eq!(masked.zmap, unmasked.zmap);
    assert_eq!(
        masked_cost, unmasked_cost,
        "(upload_bytes, buffer_creations) of an all-true mask must equal the unmasked tile's"
    );
}
