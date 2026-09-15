// SPDX-License-Identifier: GPL-3.0-or-later

//! One test binary, one device: separate integration test files are
//! separate processes, each with its own device, which would prove nothing
//! about the single-device invariant this establishes. The pinned filters
//! below name every test that needs it --
//! `device::adapter_selection_is_reported`,
//! `transfer::tile_roundtrip_is_bit_identical`,
//! `parity::harness_kernel_matches_cpu_twin`, `parity_fusion::*`,
//! `parity_warp::*`, `parity_pipeline::*`, `parity_tonemap::*` -- only
//! resolve as module paths inside one binary.
//!
//! **Runtime skip guard, `FF_GPU_PARITY`:**
//! `crates/gui` depends on `focusforge-gpu` with the `gpu` feature, which
//! unifies that feature on for plain `cargo test --workspace` and would
//! otherwise pull this whole binary into the default gate. Every test here
//! routes through `focusforge_gpu::device::shared_device_or_skip()`, which
//! prints a skip and returns `None` unless `FF_GPU_PARITY=1` is set --
//! enables rather than requires, the inverse polarity of
//! `FF_REQUIRE_SYNTH_ALIGN` (`crates/cli/tests/synthetic_ground_truth.rs`),
//! because a GPU being present is not itself a reason to spend minutes of
//! the default gate on parity. Run explicitly:
//! `FF_GPU_PARITY=1 cargo test -p focusforge-gpu --features gpu --test harness`.

#![cfg(feature = "gpu")]

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
