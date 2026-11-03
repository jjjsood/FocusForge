// SPDX-License-Identifier: GPL-3.0-or-later

//! A per-sample gain and clamp kernel, deliberately arithmetically obvious
//! so a wrong kernel is never confused with a wrong buffer layout.
//!
//! Every kernel in this crate is loaded from a sibling `.wgsl` file via
//! `include_str!`, never a Rust string literal --
//! [`harness_gain.wgsl`](../src/harness_gain.wgsl) is the example.

use std::sync::{Arc, OnceLock};

use focusforge_core::algorithm::{AlgorithmError, TileOutput, TileReader};
use focusforge_core::align::warp::WarpedTile;
use focusforge_core::align::{AlignError, Transform};
use focusforge_core::gpu::{
    AlignBackend, BackendInfo, EccMomentSession, EccSessionRequest, TileBackend,
};
use focusforge_core::tile::{Rect, SampleFormat, Tile};

use crate::budget::{
    DeviceBudget, ProbeOutcome, VramBudget, buffer_limit_shortfalls, device_bytes_per_tile,
    dispatch_limit_shortfall, largest_buffer_bytes, max_tile_side, probe_budget,
    tile_shortfall_message,
};
use crate::device::{GpuError, SharedDevice, block_on, settled_device, shared_device};
use crate::ecc::{self, EccState};
use crate::errors;
use crate::fuse::{self, FusedSample, FusionPipelines};
use crate::pack::{packed_len, unpack_samples};
use crate::pipeline::{
    Pipeline, build_pipeline, run_pass, storage_entry, uniform_entry, upload_params,
};
use crate::transfer::{create_storage_buffer, download_buffer, upload_tile};
use crate::vram::WgpuAllocProbe;
use crate::warp::{self, WarpPipelines};

/// [`TileBackend`] for this crate's kernels: `self_check`, `fuse_tile`,
/// `warp_tile` and `warp_fuse_tile` on one type. Each pipeline is built
/// lazily on first use; bind groups are rebuilt per call because each call
/// uploads fresh buffers.
pub struct GpuBackend {
    device: &'static SharedDevice,
    pipeline: OnceLock<Pipeline>,
    fuse_pipelines: FusionPipelines,
    warp_pipelines: WarpPipelines,
    /// The VRAM working-set budget: built eagerly (no `OnceLock`),
    /// because [`VramBudget::acquire`] needs no geometry at construction --
    /// each call takes the calling tile's own byte cost. See
    /// [`Self::with_max_device_bytes`] for the non-singleton constructor a
    /// test uses to drive a deliberately tiny ceiling.
    budget: Arc<VramBudget>,
    /// The settled budget and its provenance, reported through
    /// [`Self::device_budget`].
    device_budget: DeviceBudget,
    /// The ECC alignment kernels and the templates their sessions share.
    ecc: EccState,
}

impl GpuBackend {
    fn with_budget(device: &'static SharedDevice, device_budget: DeviceBudget) -> Self {
        GpuBackend {
            device,
            pipeline: OnceLock::new(),
            fuse_pipelines: FusionPipelines::new(),
            warp_pipelines: WarpPipelines::new(),
            budget: Arc::new(VramBudget::new(device_budget.bytes())),
            device_budget,
            ecc: EccState::default(),
        }
    }

    /// Probes the device for a working budget
    /// ([`probe_budget`] over a real [`WgpuAllocProbe`]), applies `ceiling`
    /// (never raising, only ever lowering -- [`DeviceBudget::capped_to`]),
    /// and builds the backend. `Err(GpuError::VramExhausted)` when the probe
    /// refused all the way down to `crate::budget::PROBE_MIN_DEVICE_BYTES`:
    /// the device cannot serve even the smallest budget -- only this
    /// startup case falls back to the CPU path; a mid-run exhaustion after
    /// this probe already succeeded does not.
    fn probed(device: &'static SharedDevice, ceiling: Option<u64>) -> Result<GpuBackend, GpuError> {
        let probe = WgpuAllocProbe::new(device.device());
        let device_budget = match probe_budget(|bytes| probe.try_reserve(bytes)) {
            ProbeOutcome::Settled { bytes, .. } => DeviceBudget::probed(bytes),
            ProbeOutcome::Exhausted { floor, detail } => {
                return Err(GpuError::VramExhausted {
                    floor_bytes: floor,
                    detail,
                });
            }
        };
        Ok(GpuBackend::with_budget(
            device,
            device_budget.capped_to(ceiling),
        ))
    }

    /// A second *backend* over the same `&'static SharedDevice` -- never a
    /// second device -- for a test that needs an
    /// explicit budget the process-wide [`gpu_backend`] singleton cannot
    /// provide. Never probes: the budget is reported `Explicit`, exactly
    /// like a flag-capped `HostBudget` one crate over.
    pub fn with_max_device_bytes(max_device_bytes: u64) -> Result<GpuBackend, &'static GpuError> {
        let device = shared_device()?;
        Ok(GpuBackend::with_budget(
            device,
            DeviceBudget::explicit(max_device_bytes),
        ))
    }

    /// The budget wrapping [`Self::warp_fuse_tile`]'s dispatch -- the
    /// observable a test reads `peak_in_flight()` / `limit_for()` off.
    pub fn budget(&self) -> &VramBudget {
        &self.budget
    }

    /// The settled device-byte budget and where it came from.
    pub fn device_budget(&self) -> DeviceBudget {
        self.device_budget
    }

    /// Refuses a `width x height` read rect at `samples_per_pixel` whose
    /// largest buffer or widest 1-D dispatch (one invocation per sample) the
    /// device's limits cannot take -- what wgpu validates a dispatch against,
    /// not the adapter's own headroom, which the message only names.
    fn check_tile(
        &self,
        width: u32,
        height: u32,
        samples_per_pixel: u32,
        warped: bool,
    ) -> Result<(), GpuError> {
        let limits = self.device.limits();
        let largest = largest_buffer_bytes(width, height, samples_per_pixel, warped);
        let mut shortfalls = buffer_limit_shortfalls(
            largest,
            limits.max_storage_buffer_binding_size,
            limits.max_buffer_size,
        );
        let invocations = u64::from(width) * u64::from(height) * u64::from(samples_per_pixel);
        shortfalls.extend(dispatch_limit_shortfall(
            invocations,
            limits.max_compute_workgroups_per_dimension,
        ));
        if shortfalls.is_empty() {
            Ok(())
        } else {
            Err(GpuError::tile_too_large(
                &shortfalls,
                &self.device.adapter_limits(),
            ))
        }
    }

    fn pipeline(&self) -> &Pipeline {
        self.pipeline.get_or_init(|| {
            build_pipeline(
                self.device.device(),
                include_str!("harness_gain.wgsl"),
                "focusforge-gpu-harness-gain",
                "main",
                &[
                    storage_entry(0, true),
                    storage_entry(1, false),
                    uniform_entry(2),
                ],
            )
        })
    }
}

impl TileBackend for GpuBackend {
    fn info(&self) -> BackendInfo {
        self.device.info().clone()
    }

    fn max_tile_size(&self, samples_per_pixel: u32) -> u32 {
        let limits = self.device.limits();
        max_tile_side(
            samples_per_pixel,
            limits.max_compute_workgroups_per_dimension,
            limits.max_storage_buffer_binding_size,
            limits.max_buffer_size,
        )
    }

    fn tile_budget_shortfall(
        &self,
        width: u32,
        height: u32,
        samples_per_pixel: u32,
        levels: u32,
        transforms: &[Transform],
        sample_format: SampleFormat,
    ) -> Option<String> {
        let (bytes, _) = tile_bytes(
            width,
            height,
            samples_per_pixel,
            levels,
            transforms,
            sample_format,
        );
        tile_shortfall_message(width, height, bytes, self.device_budget.bytes())
    }

    fn self_check(
        &self,
        reader: &dyn TileReader,
        frame: usize,
        rect: Rect,
        samples_per_pixel: u32,
        gain: f32,
    ) -> Result<Tile, AlgorithmError> {
        self.check_tile(rect.width, rect.height, samples_per_pixel, false)?;
        let sample_count = (rect.width * rect.height * samples_per_pixel) as usize;
        let mut samples = vec![0u16; sample_count];
        reader.read_tile_into(frame, rect, &mut samples)?;
        let tile = Tile {
            rect,
            samples_per_pixel,
            samples,
        };

        let device = self.device.device();
        let queue = self.device.queue();

        let word_count = (packed_len(sample_count) / 4) as u32;
        let byte_len = u64::from(word_count) * 4;

        let src_buffer = upload_tile(device, queue, &tile);
        let dst_buffer = create_storage_buffer(device, byte_len, "focusforge-gpu-harness-dst");

        let mut params = [0u8; 16];
        params[0..4].copy_from_slice(&word_count.to_le_bytes());
        params[4..8].copy_from_slice(&(sample_count as u32).to_le_bytes());
        params[8..12].copy_from_slice(&gain.to_le_bytes());
        let params_buffer = upload_params(device, queue, &params);

        let pipeline = self.pipeline();
        let entries = [
            wgpu::BindGroupEntry {
                binding: 0,
                resource: src_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: dst_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: params_buffer.as_entire_binding(),
            },
        ];
        run_pass(
            device,
            queue,
            pipeline,
            &entries,
            word_count,
            "focusforge-gpu-harness-pass",
        );

        let bytes = download_buffer(device, queue, &dst_buffer, byte_len)?;
        let samples = unpack_samples(&bytes, sample_count);

        Ok(Tile {
            rect,
            samples_per_pixel,
            samples,
        })
    }

    fn fuse_tile(
        &self,
        reader: &dyn TileReader,
        frame_count: usize,
        rect: Rect,
        samples_per_pixel: u32,
        levels: u32,
    ) -> Result<TileOutput, AlgorithmError> {
        self.check_tile(rect.width, rect.height, samples_per_pixel, false)?;
        fuse::fuse_tile_gpu(
            self.device,
            reader,
            &self.fuse_pipelines,
            frame_count,
            rect,
            samples_per_pixel,
            levels,
            &[],
        )
    }

    fn warp_tile(
        &self,
        reader: &dyn TileReader,
        frame_index: usize,
        frame: (u32, u32),
        samples_per_pixel: u32,
        dst: Rect,
        transform: &Transform,
    ) -> Result<WarpedTile, AlignError> {
        self.check_tile(dst.width, dst.height, samples_per_pixel, true)?;
        warp::warp_tile_gpu(
            self.device,
            &self.warp_pipelines,
            reader,
            frame_index,
            frame,
            samples_per_pixel,
            dst,
            transform,
        )
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
        self.warp_fuse::<u16>(reader, frame, transforms, rect, samples_per_pixel, levels)
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
        self.warp_fuse::<f32>(reader, frame, transforms, rect, samples_per_pixel, levels)
    }

    fn device_lost(&self) -> bool {
        self.device.lost()
    }
}

impl GpuBackend {
    /// The body of [`TileBackend::warp_fuse_tile`] and
    /// [`TileBackend::warp_fuse_tile_f32`]: the budget permit and error
    /// scopes around one tile, read back as `O`.
    fn warp_fuse<O: FusedSample>(
        &self,
        reader: &dyn TileReader,
        frame: (u32, u32),
        transforms: &[Transform],
        rect: Rect,
        samples_per_pixel: u32,
        levels: u32,
    ) -> Result<TileOutput<O>, AlgorithmError> {
        let (bytes, warped) = tile_bytes(
            rect.width,
            rect.height,
            samples_per_pixel,
            levels,
            transforms,
            reader.sample_format(),
        );
        self.check_tile(rect.width, rect.height, samples_per_pixel, warped)?;

        let _permit = self.budget.acquire(bytes);

        // Both scopes are pushed and popped on this one rayon worker: wgpu
        // routes an error to the innermost matching scope of the *raising*
        // thread. One pair per tile, never per pyramid level -- a validation
        // error names its kernel from the dispatch label instead.
        let device = self.device.device();
        let result = errors::with_validation_scope(device, || {
            let scope = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
            let result = crate::stack_tile::warp_fuse_tile_gpu::<O>(
                self.device,
                &self.warp_pipelines,
                &self.fuse_pipelines,
                reader,
                frame,
                transforms,
                rect,
                samples_per_pixel,
                levels,
            );
            let oom = block_on(scope.pop());
            match oom {
                // `result`'s `Ok` value is discarded even if the dispatch
                // otherwise "succeeded": after an OOM, `create_buffer` has
                // handed back error buffers whose use raises *validation*
                // errors, which route to a different filter. A mid-run
                // exhaustion fails the run; it never falls back to the host.
                Some(err) => Err(GpuError::DeviceOutOfMemory {
                    detail: err.to_string(),
                }
                .into()),
                None => result,
            }
        })
        .map_err(AlgorithmError::from)
        .and_then(std::convert::identity);

        #[cfg(feature = "gpu-tests")]
        forced_device_loss(self.device);

        result
    }
}

/// The device bytes of one `width x height` tile, and whether any frame is
/// warped. The identity test is the one `stack_tile`'s per-frame check makes,
/// folded into one flag for the whole tile's byte-cost model (`crate::budget`).
fn tile_bytes(
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    levels: u32,
    transforms: &[Transform],
    sample_format: SampleFormat,
) -> (u64, bool) {
    let warped = transforms.iter().any(|t| *t != Transform::identity());
    (
        device_bytes_per_tile(
            width,
            height,
            samples_per_pixel,
            levels,
            warped,
            sample_format,
        ),
        warped,
    )
}

impl AlignBackend for GpuBackend {
    fn open_ecc(
        &self,
        request: &EccSessionRequest<'_>,
    ) -> Result<Option<Box<dyn EccMomentSession>>, AlgorithmError> {
        ecc::open(self.device, &self.budget, &self.ecc, request)
    }
}

#[cfg(feature = "gpu-tests")]
impl GpuBackend {
    /// The ECC sampler read back: every sample of `request`'s region through
    /// `warp`, `None` where its taps leave the window. For the parity tests.
    pub fn ecc_sample_values(
        &self,
        request: &EccSessionRequest<'_>,
        warp: &Transform,
    ) -> Result<Vec<Option<f32>>, AlgorithmError> {
        ecc::sample_values(self.device, &self.ecc, request, warp)
    }
}

/// Destroys the shared device once `FF_FORCE_DEVICE_LOST` GPU operations have
/// run -- fused tiles and ECC iterations alike; an absent or unparsable value
/// does nothing. The destroying call returns once the loss is observable.
#[cfg(feature = "gpu-tests")]
pub(crate) fn forced_device_loss(device: &SharedDevice) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static GPU_OPERATIONS: AtomicUsize = AtomicUsize::new(0);

    let Some(after) = std::env::var("FF_FORCE_DEVICE_LOST")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return;
    };
    if GPU_OPERATIONS.fetch_add(1, Ordering::SeqCst) + 1 == after {
        device.force_loss();
    }
}

/// Stores the `Result` so a failed probe (`GpuError::VramExhausted`) never
/// poisons the lock: every later caller sees the same `Err`.
static GPU_BACKEND: OnceLock<Result<GpuBackend, GpuError>> = OnceLock::new();

/// The process-wide GPU backend, probed once atop [`shared_device`] and
/// capped to `max_vram`. The first caller's `max_vram` wins: later calls
/// reuse the settled result without re-probing.
pub fn gpu_backend_capped(max_vram: Option<u64>) -> Result<&'static GpuBackend, &'static GpuError> {
    let device = shared_device()?;
    GPU_BACKEND
        .get_or_init(|| GpuBackend::probed(device, max_vram))
        .as_ref()
}

/// [`gpu_backend_capped`] with no ceiling.
pub fn gpu_backend() -> Result<&'static GpuBackend, &'static GpuError> {
    gpu_backend_capped(None)
}

/// The backend this process has settled on, `None` while no caller has asked
/// for one. A failed device is reported as its stored `Err` even though
/// [`gpu_backend_capped`] returned before setting the backend. Reads only:
/// asking never opens an adapter.
pub fn settled_backend() -> Option<Result<&'static GpuBackend, &'static GpuError>> {
    match settled_device() {
        Some(Err(err)) => Some(Err(err)),
        _ => GPU_BACKEND.get().map(Result::as_ref),
    }
}

/// Whether this process's VRAM cap is fixed: the backend was probed, the lazy
/// align backend took its `max_vram` at its first use, or the device failed.
/// A cap given after that has no effect. Reads only.
pub fn vram_cap_settled() -> bool {
    GPU_BACKEND.get().is_some()
        || LAZY_ALIGN_BACKEND.get().is_some()
        || matches!(settled_device(), Some(Err(_)))
}

/// Resolves whether a stack run over `algorithm_id` takes the GPU path, and
/// a one-line, human-readable account of the choice either way. This crate
/// ships a WGSL port of exactly one stacking algorithm, so every other id
/// stays on the CPU path without attempting an adapter probe. `no_gpu`
/// forces the CPU path regardless of `algorithm_id`; `max_vram` is a
/// ceiling-only cap on the probed device budget.
pub fn backend_for(
    algorithm_id: &str,
    no_gpu: bool,
    max_vram: Option<u64>,
) -> (Option<&'static GpuBackend>, String) {
    use focusforge_core::algorithm::StackAlgorithm;
    if algorithm_id != focusforge_core::fuse::laplacian::Laplacian.id() {
        // No adapter attempted, and nothing to report: an algorithm with no
        // GPU kernel stays on the CPU path silently, exactly as if this
        // function had never been called.
        return (None, String::new());
    }
    if no_gpu {
        return (None, "--cpu given: forcing the CPU path".to_string());
    }
    match gpu_backend_capped(max_vram) {
        Ok(backend) => (
            Some(backend),
            format!("adapter selected: {}; using the GPU path", backend.info()),
        ),
        Err(err) => (None, cpu_fallback_line(err)),
    }
}

/// The align stage's accelerator, opened on the first ECC level routed to it:
/// the adapter, device and budget come up inside the first
/// [`AlignBackend::open_ecc`] call, so a run whose levels all fall below the
/// offload threshold never touches the GPU. A failed bring-up declines that
/// level and every later one, which then run on the CPU.
pub struct LazyAlignBackend {
    max_vram: Option<u64>,
    announce: fn(&str),
    selected: OnceLock<Option<&'static GpuBackend>>,
    adapter: OnceLock<String>,
}

impl LazyAlignBackend {
    /// The adapter's display once a level has opened it; empty before, and
    /// when the bring-up failed.
    pub fn adapter(&self) -> &OnceLock<String> {
        &self.adapter
    }

    /// Whether the device a level already opened is lost. `false` before any
    /// level has opened it: asking never brings the adapter up.
    pub fn lost(&self) -> bool {
        matches!(self.selected.get(), Some(Some(backend)) if backend.device_lost())
    }

    /// Brings the backend up on first call and reports the outcome to
    /// `announce` once, as one line.
    fn resolve(&self) -> Option<&'static GpuBackend> {
        *self.selected.get_or_init(|| {
            let (backend, line) = match gpu_backend_capped(self.max_vram) {
                Ok(backend) => (
                    Some(backend),
                    format!(
                        "alignment offloads large levels to {}; device budget {} bytes",
                        backend.info(),
                        backend.device_budget().bytes()
                    ),
                ),
                Err(err) => (None, cpu_fallback_line(err)),
            };
            if let Some(backend) = backend {
                let _ = self.adapter.set(backend.info().to_string());
            }
            (self.announce)(&line);
            backend
        })
    }
}

impl AlignBackend for LazyAlignBackend {
    fn open_ecc(
        &self,
        request: &EccSessionRequest<'_>,
    ) -> Result<Option<Box<dyn EccMomentSession>>, AlgorithmError> {
        match self.resolve() {
            Some(backend) => backend.open_ecc(request),
            None => Ok(None),
        }
    }
}

static LAZY_ALIGN_BACKEND: OnceLock<LazyAlignBackend> = OnceLock::new();

/// [`backend_for`] for the align stage: only the pyramidal ECC aligner has a
/// GPU path, so any other `algorithm_id` stays on the CPU with an empty reason
/// and `no_gpu` with its reason, neither probing an adapter. Otherwise the
/// [`LazyAlignBackend`] shares the [`gpu_backend_capped`] singleton and passes
/// its selection line to `announce` on first use. The line never starts with
/// [`backend_for`]'s `adapter selected: `. The first call's `max_vram` and
/// `announce` win.
pub fn align_backend_for(
    algorithm_id: &str,
    no_gpu: bool,
    max_vram: Option<u64>,
    announce: fn(&str),
) -> (Option<&'static LazyAlignBackend>, String) {
    use focusforge_core::algorithm::AlignAlgorithm;
    if algorithm_id != focusforge_core::align::PyramidalEcc.id() {
        return (None, String::new());
    }
    if no_gpu {
        return (None, "--cpu given: aligning on the CPU".to_string());
    }
    let lazy = LAZY_ALIGN_BACKEND.get_or_init(|| LazyAlignBackend {
        max_vram,
        announce,
        selected: OnceLock::new(),
        adapter: OnceLock::new(),
    });
    (Some(lazy), String::new())
}

/// Reason-led, because [`GpuError::VramExhausted`] (an adapter that cannot
/// serve the minimum budget) falls back here exactly like a missing adapter.
fn cpu_fallback_line(reason: &GpuError) -> String {
    format!("{reason}; continuing on the CPU path")
}

#[cfg(test)]
mod tests {
    use super::{
        LazyAlignBackend, align_backend_for, backend_for, cpu_fallback_line, settled_backend,
        vram_cap_settled,
    };
    use crate::device::GpuError;
    use focusforge_core::algorithm::{AlignAlgorithm, StackAlgorithm};
    use focusforge_core::align::PyramidalEcc;
    use focusforge_core::fuse::laplacian::Laplacian;

    #[test]
    fn cpu_fallback_line_leads_with_the_reason() {
        let exhausted = GpuError::VramExhausted {
            floor_bytes: 268_435_456,
            detail: "allocation refused".to_string(),
        };
        assert_eq!(
            cpu_fallback_line(&exhausted),
            "device cannot serve even the minimum VRAM budget of 268435456 bytes: \
             allocation refused; continuing on the CPU path"
        );
        let no_adapter = GpuError::NoAdapter {
            reason: "no backend available".to_string(),
        };
        assert_eq!(
            cpu_fallback_line(&no_adapter),
            "no usable GPU adapter: no backend available; continuing on the CPU path"
        );
    }

    #[test]
    fn backend_for_cpu_selection() {
        for max_vram in [None, Some(0)] {
            let (backend, message) = backend_for(Laplacian.id(), true, max_vram);
            assert!(backend.is_none());
            assert_eq!(message, "--cpu given: forcing the CPU path");

            for no_gpu in [false, true] {
                let (backend, message) = backend_for("unsupported", no_gpu, max_vram);
                assert!(backend.is_none());
                assert!(message.is_empty());
            }
        }
    }

    #[test]
    fn a_lazy_align_backend_is_not_lost_before_bring_up() {
        let lazy = LazyAlignBackend {
            max_vram: None,
            announce: |_| panic!("asking whether the device is lost must not bring it up"),
            selected: std::sync::OnceLock::new(),
            adapter: std::sync::OnceLock::new(),
        };
        assert!(!lazy.lost());
        assert!(lazy.selected.get().is_none(), "no adapter was opened");
        assert!(lazy.adapter().get().is_none());
    }

    #[test]
    fn align_backend_for_cpu_selection() {
        for max_vram in [None, Some(0)] {
            let (backend, message) = align_backend_for(PyramidalEcc.id(), true, max_vram, |_| {});
            assert!(backend.is_none());
            assert_eq!(message, "--cpu given: aligning on the CPU");
            assert!(!message.contains("adapter selected: "));

            for no_gpu in [false, true] {
                let (backend, message) = align_backend_for("unsupported", no_gpu, max_vram, |_| {});
                assert!(backend.is_none());
                assert!(message.is_empty());
            }
        }
    }

    #[test]
    fn reading_the_settled_state_never_brings_the_adapter_up() {
        assert!(settled_backend().is_none());
        assert!(!vram_cap_settled());

        let (lazy, _) = align_backend_for(PyramidalEcc.id(), false, None, |_| {});
        assert!(lazy.is_some());
        assert!(vram_cap_settled(), "the lazy align backend fixed its cap");
        assert!(settled_backend().is_none(), "no backend was probed");
    }
}
