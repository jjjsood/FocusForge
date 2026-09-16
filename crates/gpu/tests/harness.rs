// SPDX-License-Identifier: GPL-3.0-or-later

//! One test binary, one device: every GPU test is a module of this binary,
//! so all of them share the single process-wide device. Gated behind
//! `gpu-tests` so a plain `cargo test` never brings up a device: run with
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
#[path = "harness/parity_tonemap.rs"]
mod parity_tonemap;
#[path = "harness/parity_warp.rs"]
mod parity_warp;
#[path = "harness/transfer.rs"]
mod transfer;
