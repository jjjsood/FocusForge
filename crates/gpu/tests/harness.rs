// SPDX-License-Identifier: GPL-3.0-or-later

//! One test binary, one device: separate integration test files are
//! separate processes, each with its own device, which would prove nothing
//! about the single-device invariant this establishes. The pinned filters
//! below name every test that needs it --
//! `device::adapter_selection_is_reported`,
//! `transfer::tile_roundtrip_is_bit_identical`,
//! `parity::harness_kernel_matches_cpu_twin`, `parity_fusion::*`,
//! `parity_warp::*`, `parity_pipeline::*` -- only resolve as module paths
//! inside one binary. Gated behind `gpu-tests` (not the default `gpu`) so
//! a plain `cargo test` never brings up a device: run deliberately with
//! `cargo test --features gpu-tests -- --test-threads=1`.

#![cfg(feature = "gpu-tests")]

mod common;

#[path = "harness/budget_gpu.rs"]
mod budget_gpu;
#[path = "harness/device.rs"]
mod device;
#[path = "harness/parity.rs"]
mod parity;
#[path = "harness/parity_fusion.rs"]
mod parity_fusion;
#[path = "harness/parity_pipeline.rs"]
mod parity_pipeline;
#[path = "harness/parity_warp.rs"]
mod parity_warp;
#[path = "harness/transfer.rs"]
mod transfer;
