// SPDX-License-Identifier: GPL-3.0-or-later

//! One `wgpu::Device` and its `Queue`, created once per process and shared
//! -- no test module and no pipeline stage creates a second one. `init`,
//! below, is the only place in this crate that asks an instance for an
//! adapter or asks that adapter to create a device.

use std::future::Future;
use std::sync::OnceLock;
use std::task::{Context, Poll, Waker};

use wgpu::{
    Backends, DeviceDescriptor, ExperimentalFeatures, Instance, InstanceDescriptor, InstanceFlags,
    MemoryHints, PowerPreference, RequestAdapterOptions, Trace,
};

use crate::limits::{LimitShortfall, describe_shortfalls, device_limits, required_limits};

/// The `for_resource_creation` threshold, as a percent of a heap's own
/// budget: the value gfx-rs/wgpu#7460 itself proposes. Only D3D12 and
/// (optionally, when `VK_EXT_memory_budget` is enabled) Vulkan honour this
/// at all in wgpu 29.0.4.
const MEMORY_BUDGET_THRESHOLD_PERCENT: u8 = 90;

/// Everything that can go wrong bringing up the shared device, named rather
/// than left as an adapter-creation panic.
#[derive(Debug, thiserror::Error)]
pub enum GpuError {
    /// No usable adapter was found down the selected backend(s).
    #[error("no usable GPU adapter: {reason}")]
    NoAdapter { reason: String },

    /// The adapter exists but falls short of [`crate::limits::required_limits`]
    /// on at least one named limit.
    #[error("adapter is short on required limits: {detail}")]
    LimitsShort { detail: String },

    /// The adapter accepted the limits check but device creation itself
    /// failed.
    #[error("failed to create wgpu device: {0}")]
    RequestDevice(#[from] wgpu::RequestDeviceError),

    /// A tile's largest single buffer or its dispatch would exceed a hard
    /// device limit (`crate::budget::buffer_limit_shortfalls`,
    /// `crate::budget::dispatch_limit_shortfall`), refused before any read or
    /// allocation rather than surfacing as a wgpu validation error.
    #[error("tile exceeds a device limit: {detail}")]
    TileTooLarge { detail: String },

    /// A dispatch failed wgpu validation inside
    /// [`crate::errors::with_validation_scope`], so its output buffers were
    /// never written.
    #[error("kernel {kernel} failed validation: {detail}")]
    KernelValidation { kernel: String, detail: String },

    /// A readback (`crate::transfer::download_buffer`/`download_f32`)
    /// failed: `device.poll` failed, the `map_async` callback never fired,
    /// or the mapping it delivered was an error.
    #[error("readback failed: {detail}")]
    Readback { detail: String },

    /// An in-run allocation refusal caught by a `push_error_scope` /
    /// `pop_error_scope(OutOfMemory)` pair around a tile's dispatch. Its
    /// message names `--max-vram` as the flag to lower -- mid-run
    /// exhaustion after a successful startup probe is a run failure, never
    /// a fallback.
    #[error("GPU ran out of device memory mid-run: {detail}; lower --max-vram to reduce demand")]
    DeviceOutOfMemory { detail: String },

    /// [`crate::budget::probe_budget`] backed all the way down to
    /// [`crate::budget::PROBE_MIN_DEVICE_BYTES`] without a single accepted
    /// reservation: the device cannot serve even the smallest budget.
    /// `crate::harness::backend_for` falls back to the CPU path on it,
    /// exactly like [`GpuError::NoAdapter`].
    #[error("device cannot serve even the minimum VRAM budget of {floor_bytes} bytes: {detail}")]
    VramExhausted { floor_bytes: u64, detail: String },
}

impl GpuError {
    /// Builds a [`GpuError::LimitsShort`] whose message names every
    /// shortfall's own limit (`describe_shortfalls`), never only
    /// "some limit was short".
    pub fn limits_short(shortfalls: &[LimitShortfall]) -> Self {
        GpuError::LimitsShort {
            detail: describe_shortfalls(shortfalls),
        }
    }

    /// Builds a [`GpuError::TileTooLarge`] naming each shortfall against the
    /// device's limits (what wgpu validates), plus the adapter's headroom and
    /// `crate::limits::required_limits` as the place to raise it.
    pub fn tile_too_large(shortfalls: &[LimitShortfall], adapter_limits: &wgpu::Limits) -> Self {
        GpuError::TileTooLarge {
            detail: format!(
                "{}; the adapter itself allows up to max_storage_buffer_binding_size={} \
                 max_buffer_size={} max_compute_workgroups_per_dimension={} -- raise \
                 crate::limits::required_limits() to use more",
                describe_shortfalls(shortfalls),
                adapter_limits.max_storage_buffer_binding_size,
                adapter_limits.max_buffer_size,
                adapter_limits.max_compute_workgroups_per_dimension,
            ),
        }
    }
}

impl From<GpuError> for focusforge_core::algorithm::AlgorithmError {
    fn from(err: GpuError) -> Self {
        focusforge_core::algorithm::AlgorithmError::Failed {
            algorithm: "gpu".to_string(),
            detail: err.to_string(),
        }
    }
}

/// For `crate::warp`'s readback call sites, which return `AlignError`.
impl From<GpuError> for focusforge_core::align::AlignError {
    fn from(err: GpuError) -> Self {
        focusforge_core::align::AlignError::Backend {
            detail: err.to_string(),
        }
    }
}

/// The device/queue the crate shares, plus the adapter facts a caller needs
/// to report or reason about (`info`, `limits`). Holds `instance` and
/// `adapter` too so neither is dropped out from under the device, and so
/// the headless compute path can report the selected adapter and limits.
pub struct SharedDevice {
    instance: Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    info: focusforge_core::gpu::BackendInfo,
    limits: wgpu::Limits,
}

impl SharedDevice {
    /// The `wgpu::Instance` the device was created from. `wgpu::Instance` is
    /// `Clone` in the pinned wgpu 29.0.4, so callers can retain their own
    /// handle without moving this crate's copy.
    pub fn instance(&self) -> &wgpu::Instance {
        &self.instance
    }

    /// The `wgpu::Adapter` the device was requested from. `wgpu::Adapter` is
    /// `Clone` in the pinned wgpu 29.0.4, for the same reason.
    pub fn adapter(&self) -> &wgpu::Adapter {
        &self.adapter
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    pub fn info(&self) -> &focusforge_core::gpu::BackendInfo {
        &self.info
    }

    /// The limits the device was created with (`crate::limits::device_limits`):
    /// `required_limits()` with only the texture dimensions widened for
    /// the selected adapter. wgpu validates allocations against these, so
    /// the pre-dispatch refusal does too, not [`Self::adapter_limits`].
    pub fn limits(&self) -> &wgpu::Limits {
        &self.limits
    }

    /// What the adapter could offer if [`crate::limits::required_limits`]
    /// were raised: headroom for the refusal message, never the threshold
    /// itself (that is [`Self::limits`]).
    pub fn adapter_limits(&self) -> wgpu::Limits {
        self.adapter.limits()
    }

    /// Whether this device has been lost. wgpu queues the device-lost
    /// callback and only runs it from a later poll, so this polls first for
    /// the freshest answer before reading the flag the callback set -- the
    /// compute-only flag, since this is the headless device, never the
    /// render device's.
    pub fn lost(&self) -> bool {
        let _ = self.device.poll(wgpu::PollType::Poll);
        crate::errors::compute_device_lost()
    }

    /// Destroys the device and polls until the loss flag is set, so the loss
    /// is observable on return. Panics if the flag is still unset after 1000
    /// polls, 1 ms apart.
    #[cfg(feature = "gpu-tests")]
    pub fn force_loss(&self) {
        const POLLS: u32 = 1000;
        self.device.destroy();
        for _ in 0..POLLS {
            let _ = self.device.poll(wgpu::PollType::Poll);
            if crate::errors::compute_device_lost() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("forced device loss was not observed after {POLLS} polls");
    }
}

/// wgpu's adapter/device request and error-scope futures are ready on first
/// poll on every native platform (the same approach `i-slint-core`'s wgpu_29
/// backend takes), so no async executor is needed.
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn init() -> Result<SharedDevice, GpuError> {
    let instance = Instance::new(InstanceDescriptor {
        backends: Backends::from_env().unwrap_or_default(),
        flags: InstanceFlags::from_build_config().with_env(),
        backend_options: wgpu::BackendOptions::from_env_or_default(),
        // `for_device_loss` stays `None` on purpose: arming it risks
        // `download_buffer`'s poll path hitting a device-loss panic instead
        // of the recoverable `OutOfMemory` error scope this crate builds.
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds {
            for_resource_creation: Some(MEMORY_BUDGET_THRESHOLD_PERCENT),
            for_device_loss: None,
        },
        display: None,
    });

    // Documented preference order: discrete before integrated before
    // fallback. `HighPerformance`, not `default()`, states that order
    // explicitly rather than leaving it to the platform's own default; a
    // software adapter is never silently selected
    // (`force_fallback_adapter: false`).
    let adapter = block_on(instance.request_adapter(&RequestAdapterOptions {
        power_preference: PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .map_err(|err| GpuError::NoAdapter {
        reason: err.to_string(),
    })?;

    let adapter_limits = adapter.limits();
    let shortfalls = crate::limits::shortfalls(&required_limits(), &adapter_limits);
    if !shortfalls.is_empty() {
        return Err(GpuError::limits_short(&shortfalls));
    }

    let adapter_info = adapter.get_info();
    let info = focusforge_core::gpu::BackendInfo {
        name: adapter_info.name.clone(),
        backend: format!("{:?}", adapter_info.backend),
        driver: adapter_info.driver.clone(),
        driver_info: adapter_info.driver_info.clone(),
    };

    let limits = device_limits(&adapter_limits);

    let (device, queue) = block_on(adapter.request_device(&DeviceDescriptor {
        label: Some("focusforge-gpu-shared-device"),
        required_features: crate::limits::REQUIRED_FEATURES,
        required_limits: limits.clone(),
        experimental_features: ExperimentalFeatures::disabled(),
        memory_hints: MemoryHints::default(),
        trace: Trace::default(),
    }))?;

    crate::errors::install_compute_handlers(&device);

    // Two records, not one: the adapter's identity is what a report is read
    // against, the limits are what a refused allocation is read against, and
    // a limits set is long. Both are raised inside the process-wide device's
    // initialiser, so a batch of any number of stacks logs each exactly once.
    tracing::debug!(
        adapter = %adapter_info.name,
        backend = %info.backend,
        driver = %adapter_info.driver,
        driver_info = %adapter_info.driver_info,
        device_type = ?adapter_info.device_type,
        "graphics adapter selected"
    );
    tracing::debug!(
        adapter_limits = ?adapter_limits,
        device_limits = ?limits,
        "graphics adapter and device limits"
    );

    Ok(SharedDevice {
        instance,
        adapter,
        device,
        queue,
        info,
        limits,
    })
}

static SHARED: OnceLock<Result<SharedDevice, GpuError>> = OnceLock::new();

/// The process-wide device, initialised on first call. `OnceLock<Result<..>>`,
/// not a `Mutex` and not a panicking initialiser: `init` returns its error
/// instead of unwinding, so the lock never poisons and every later caller
/// observes the same stored `Err` rather than a lock-poisoning message.
pub fn shared_device() -> Result<&'static SharedDevice, &'static GpuError> {
    SHARED.get_or_init(init).as_ref()
}

/// The process-wide device if a caller already initialised it; never
/// initialises it.
pub(crate) fn settled_device() -> Option<Result<&'static SharedDevice, &'static GpuError>> {
    SHARED.get().map(Result::as_ref)
}

/// `shared_device()`, but on failure prints `SKIP: no usable GPU adapter --
/// <error>` and returns `None` instead of propagating the error -- the one
/// place a test learns there is no GPU. Tests call this and `return` early so
/// a skip prints a reason instead of failing or silently passing.
pub fn shared_device_or_skip() -> Option<&'static SharedDevice> {
    match shared_device() {
        Ok(device) => Some(device),
        Err(err) => {
            println!("SKIP: no usable GPU adapter -- {err}");
            None
        }
    }
}

/// One line naming the selected adapter, its backend and its driver --
/// reported, not left implicit.
pub fn adapter_report(info: &focusforge_core::gpu::BackendInfo) -> String {
    format!(
        "adapter selected: name={} backend={} driver={} driver_info={}",
        info.name, info.backend, info.driver, info.driver_info
    )
}
