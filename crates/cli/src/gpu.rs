// SPDX-License-Identifier: GPL-3.0-or-later

//! Resolves whether one `stack`/`align` run takes the GPU path or the CPU
//! one, and prints that decision -- never a panic, never a non-zero exit: a
//! missing adapter is the common outcome on a machine with no GPU, not a
//! failure of this command. `#[cfg(feature = "gpu")]`: with the feature
//! off, `crate::main`'s own `resolve_backend` never even calls into this
//! module, and none of it is compiled.

use focusforge_core::gpu::{AlignBackend, TileBackend};
use focusforge_gpu::harness::{GpuBackend, align_backend_for, backend_for};

use crate::stack::{AlignBackendChoice, BackendChoice, DeviceBudgetReport, DevicePeak};

/// The device budget's own realized high-water tile count for this
/// `GpuBackend`, scoped to one group: `reset_peak` restarts the interval,
/// and `peak_in_flight` is read fresh on every call, never cached, since it
/// is only meaningful after `scheduler.run` has returned.
impl DevicePeak for GpuBackend {
    fn reset_peak(&self) {
        self.budget().reset_peak();
    }

    fn peak_in_flight(&self) -> usize {
        self.budget().peak_in_flight()
    }
}

/// `no_gpu` (`--cpu`'s value) forces the CPU path unconditionally;
/// `max_vram` (`--max-vram`'s value) caps the probed device budget. Prints
/// a required fallback reason to stderr.
/// Returns the backend plus its settled budget report and device-peak
/// handle on the GPU path, `BackendChoice::cpu()` on the CPU path.
pub fn resolve_backend(
    algorithm_id: &str,
    no_gpu: bool,
    max_vram: Option<u64>,
    quiet: bool,
) -> BackendChoice {
    let (backend, message) = backend_for(algorithm_id, no_gpu, max_vram);
    if !message.is_empty() && (!quiet || message.contains("continuing on the CPU")) {
        eprintln!("{message}");
    }
    match backend {
        Some(backend) => {
            let device_budget = backend.device_budget();
            BackendChoice {
                backend: Some(backend as &dyn TileBackend),
                device_budget: Some(DeviceBudgetReport {
                    max_device_bytes: device_budget.bytes(),
                    provenance: device_budget.provenance(),
                }),
                device_peak: Some(backend as &dyn DevicePeak),
            }
        }
        None => BackendChoice::cpu(),
    }
}

/// [`resolve_backend`] for the align stage: prints the one-line reason to
/// stderr when the CPU is chosen up front, and
/// otherwise returns an accelerator that opens the adapter on the first level
/// it serves and prints its selection line then.
pub fn resolve_align_backend(
    algorithm_id: &str,
    no_gpu: bool,
    max_vram: Option<u64>,
    quiet: bool,
) -> AlignBackendChoice {
    let (backend, message) = align_backend_for(
        algorithm_id,
        no_gpu,
        max_vram,
        if quiet {
            announce_align_backend_quiet
        } else {
            announce_align_backend
        },
    );
    if !message.is_empty() && (!quiet || message.contains("continuing on the CPU")) {
        eprintln!("{message}");
    }
    match backend {
        Some(backend) => AlignBackendChoice {
            backend: Some(backend as &dyn AlignBackend),
            adapter: Some(backend.adapter()),
        },
        None => AlignBackendChoice::cpu(),
    }
}

fn announce_align_backend(line: &str) {
    eprintln!("{line}");
}

fn announce_align_backend_quiet(line: &str) {
    if should_print_align_announcement(line) {
        eprintln!("{line}");
    }
}

fn should_print_align_announcement(line: &str) -> bool {
    !line.starts_with("alignment offloads")
}

#[cfg(test)]
mod tests {
    use super::should_print_align_announcement;

    #[test]
    fn quiet_suppresses_the_successful_lazy_adapter_announcement() {
        assert!(!should_print_align_announcement(
            "alignment offloads large levels to test adapter; device budget 1 bytes"
        ));
    }

    #[test]
    fn quiet_retains_a_lazy_adapter_fallback_reason() {
        assert!(should_print_align_announcement(
            "GPU adapter unavailable; continuing on the CPU"
        ));
    }
}
