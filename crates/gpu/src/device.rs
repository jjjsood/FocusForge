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

use crate::adapter::{AdapterPath, classify_adapter, skip_message};
use crate::limits::{LimitShortfall, describe_shortfalls, required_limits};

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

    /// A tile's own largest single buffer would exceed a hard device limit
    /// (`crate::budget::buffer_limit_shortfalls`), refused before dispatch
    /// rather than surfacing as a wgpu validation panic.
    #[error("tile needs a buffer this device cannot allocate: {detail}")]
    TileTooLarge { detail: String },
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

    /// Builds a [`GpuError::TileTooLarge`] naming the shortfall
    /// (`describe_shortfalls`, so the message names the limit, the
    /// requested size and the device's own limit -- `shared.limits()` is
    /// what wgpu actually validates against) plus the adapter's own
    /// headroom (`adapter_limits`) and the place to raise it
    /// (`crate::limits::required_limits`), so a refusal never reads as "some
    /// buffer was too big" with no path forward.
    pub fn tile_too_large(shortfalls: &[LimitShortfall], adapter_limits: &wgpu::Limits) -> Self {
        GpuError::TileTooLarge {
            detail: format!(
                "{}; the adapter itself allows up to max_storage_buffer_binding_size={} \
                 max_buffer_size={} -- raise crate::limits::required_limits() to use more",
                describe_shortfalls(shortfalls),
                adapter_limits.max_storage_buffer_binding_size,
                adapter_limits.max_buffer_size,
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

impl From<AdapterPath<wgpu::Adapter>> for Result<wgpu::Adapter, GpuError> {
    fn from(path: AdapterPath<wgpu::Adapter>) -> Self {
        match path {
            AdapterPath::Present(adapter) => Ok(adapter),
            AdapterPath::Absent { reason } => Err(GpuError::NoAdapter { reason }),
        }
    }
}

/// The device/queue the crate shares, plus the adapter facts a caller needs
/// to report or reason about (`info`, `limits`). Holds `instance` and
/// `adapter` too so neither is dropped out from under the device.
pub struct SharedDevice {
    #[allow(dead_code)]
    instance: Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    info: focusforge_core::gpu::BackendInfo,
    limits: wgpu::Limits,
}

impl SharedDevice {
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    pub fn info(&self) -> &focusforge_core::gpu::BackendInfo {
        &self.info
    }

    /// The limits the device was actually created with -- `required_limits()`
    /// (i.e. `wgpu::Limits::downlevel_defaults()`), never raised here. This
    /// is what wgpu validates every allocation against, which is why the
    /// device-budget's pre-dispatch refusal checks this value rather than
    /// [`Self::adapter_limits`].
    pub fn limits(&self) -> &wgpu::Limits {
        &self.limits
    }

    /// What the adapter could offer if [`crate::limits::required_limits`]
    /// were ever raised -- reported as headroom in the device-budget's
    /// refusal message, never as the threshold itself (that is [`Self::limits`]).
    /// `wgpu::Limits` by value, the same way `adapter.limits()` is already
    /// used at [`init`]'s own shortfall check, above.
    pub fn adapter_limits(&self) -> wgpu::Limits {
        self.adapter.limits()
    }
}

/// Same trick `i-slint-core`'s wgpu_29 backend uses: wgpu's adapter/device
/// request futures are ready on first poll on every native platform, so no
/// async executor dependency is needed here.
fn block_on<F: Future>(future: F) -> F::Output {
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
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        display: None,
    });

    // Documented preference order: discrete before integrated before
    // fallback. `HighPerformance`, not `default()`, states that order
    // explicitly rather than leaving it to the platform's own default; a
    // software adapter is never silently selected
    // (`force_fallback_adapter: false`).
    let adapter_result = block_on(instance.request_adapter(&RequestAdapterOptions {
        power_preference: PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }));
    let adapter: Result<wgpu::Adapter, GpuError> = classify_adapter(adapter_result).into();
    let adapter = adapter?;

    let shortfalls = crate::limits::shortfalls(&required_limits(), &adapter.limits());
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

    let (device, queue) = block_on(adapter.request_device(&DeviceDescriptor {
        label: Some("focusforge-gpu-shared-device"),
        required_features: crate::limits::REQUIRED_FEATURES,
        required_limits: required_limits(),
        experimental_features: ExperimentalFeatures::disabled(),
        memory_hints: MemoryHints::default(),
        trace: Trace::default(),
    }))?;

    let limits = required_limits();
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

/// `shared_device()`, but on failure prints [`skip_message`] and returns
/// `None` instead of propagating the error -- the one place a test learns
/// there is no GPU. Tests call this and `return` early so a skip prints a
/// reason instead of failing or silently passing.
pub fn shared_device_or_skip() -> Option<&'static SharedDevice> {
    match shared_device() {
        Ok(device) => Some(device),
        Err(err) => {
            println!("{}", skip_message(&err.to_string()));
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
