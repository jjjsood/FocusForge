// SPDX-License-Identifier: GPL-3.0-or-later

//! One test binary, one device: separate integration test files are
//! separate processes, each with its own device, which would prove nothing
//! about the single-device invariant this establishes. The pinned filters
//! below name every test that needs it --
//! `device::adapter_selection_is_reported`,
//! `transfer::tile_roundtrip_is_bit_identical`,
//! `parity::harness_kernel_matches_cpu_twin` -- only resolve as module paths
//! inside one binary.

#![cfg(feature = "gpu")]

mod common;

#[path = "harness/device.rs"]
mod device;
#[path = "harness/parity.rs"]
mod parity;
#[path = "harness/transfer.rs"]
mod transfer;
