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
#[path = "harness/ecc_parity.rs"]
mod ecc_parity;
#[path = "harness/errors.rs"]
mod errors;
#[path = "harness/full_frame_render_identity.rs"]
mod full_frame_render_identity;
#[path = "harness/parity.rs"]
mod parity;
#[path = "harness/parity_blend.rs"]
mod parity_blend;
#[path = "harness/parity_f32.rs"]
mod parity_f32;
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
