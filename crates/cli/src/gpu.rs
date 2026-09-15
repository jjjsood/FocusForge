// SPDX-License-Identifier: GPL-3.0-or-later

//! Resolves whether one `stack`/`align` run takes the GPU path or the CPU
//! one, and prints that decision -- never a panic, never a non-zero exit: a
//! missing adapter is the common outcome on a machine with no GPU, not a
//! failure of this command. `#[cfg(feature = "gpu")]`: with the feature
//! off, `crate::main`'s own `resolve_backend` never even calls into this
//! module, and none of it is compiled.

use focusforge_core::gpu::TileBackend;
use focusforge_gpu::adapter::{PathChoice, choose_path};
use focusforge_gpu::harness::gpu_backend_capped;

use crate::stack::DeviceBudgetReport;

/// `no_gpu` is `--no-gpu`'s value: `true` forces the CPU path
/// unconditionally, without even attempting -- let alone reporting a
/// missing -- adapter. `max_vram` is `--max-vram`'s value, a ceiling-only
/// cap on the probed device budget (`gpu_backend_capped`). Prints the
/// decision to **stderr** (the stream `crate::fail` and `align`'s own lax
/// warning already use), and returns the backend plus its settled budget
/// report on the GPU path, `(None, None)` on the CPU path.
pub fn resolve_backend(
    no_gpu: bool,
    max_vram: Option<u64>,
) -> (Option<&'static dyn TileBackend>, Option<DeviceBudgetReport>) {
    let (path, result, message) = choose_path(no_gpu, gpu_backend_capped(max_vram));
    eprintln!("{message}");
    match path {
        PathChoice::Gpu => match result.ok() {
            Some(backend) => {
                let device_budget = backend.device_budget();
                (
                    Some(backend as &dyn TileBackend),
                    Some(DeviceBudgetReport {
                        max_device_bytes: device_budget.bytes(),
                        provenance: device_budget.provenance(),
                    }),
                )
            }
            None => (None, None),
        },
        PathChoice::CpuFallback => (None, None),
    }
}
