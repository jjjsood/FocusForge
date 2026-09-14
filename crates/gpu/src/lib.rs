// SPDX-License-Identifier: GPL-3.0-or-later

//! The GPU path: the same kernels `crates/core` runs on the CPU, in WGSL,
//! reached from `crates/core` through
//! [`focusforge_core::gpu::TileBackend`], never the other way around
//! (`crates/core` has no `wgpu` dependency).
//!
//! **wgpu major pin.** `wgpu` is pinned to `29.0.4` because Slint 1.17.1's
//! `unstable-wgpu-29` feature provides its zero-copy `wgpu::Texture` import:
//! the GUI hands Slint the device from \[device\], so the composite never
//! round-trips through the CPU. The major moves only with Slint's;
//! `deny.toml` bans a second `wgpu` major in the lockfile by name.
//!
//! **WGSL loading convention.** Every kernel in this crate is an
//! `include_str!` of a sibling `.wgsl` file passed to
//! `wgpu::ShaderSource::Wgsl`, never a string literal -- see
//! \[harness_gain.wgsl\] via \[harness\].
//!
//! `wgpu` is an optional dependency (`gpu = ["dep:wgpu", "dep:thiserror"]`,
//! `crates/gpu/Cargo.toml`): [`adapter`] and [`pack`] contain no `wgpu` type
//! and stay compiled (and tested) with the feature off, which is what keeps
//! the featureless build free of the `wgpu` compile.

pub mod adapter;
pub mod pack;

#[cfg(feature = "gpu")]
pub mod device;
#[cfg(feature = "gpu")]
pub mod harness;
#[cfg(feature = "gpu")]
pub mod limits;
#[cfg(feature = "gpu")]
pub mod transfer;
