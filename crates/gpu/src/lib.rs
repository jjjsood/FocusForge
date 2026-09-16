// SPDX-License-Identifier: GPL-3.0-or-later

// Doc links to `pub(crate)` items resolve: this crate's docs are always
// built with `--document-private-items`.
#![allow(rustdoc::private_intra_doc_links)]

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
//! \[harness_gain.wgsl\] via \[harness\], and \[warp_bicubic.wgsl\] via
//! \[warp\].
//!
//! `wgpu` is an optional dependency (`gpu = ["dep:wgpu", "dep:thiserror"]`,
//! `crates/gpu/Cargo.toml`): [`adapter`], [`pack`], [`warp_coords`] and
//! [`budget`] contain no `wgpu` type and stay compiled (and tested) with the
//! feature off, which is what keeps the featureless build free of the `wgpu`
//! compile. [`limits`] is a mixed module: `LimitShortfall` and
//! `describe_shortfalls` name no `wgpu` type either and stay unconditional
//! too, so [`budget`]'s pre-dispatch refusal can reuse them and be
//! unit-tested in this same default gate; only the three items that do name
//! `wgpu::Limits` are individually `#[cfg(feature = "gpu")]` inside that
//! module.

pub mod adapter;
pub mod budget;
pub mod limits;
pub mod pack;
pub mod warp_coords;

#[cfg(feature = "gpu")]
pub mod device;
/// The display kernel: a 16-to-8-bit reduction with ordered dither and no
/// tone curve, the display texture Slint imports, and the one readback
/// path that exists only for the parity test.
#[cfg(feature = "gpu")]
pub mod display;
#[cfg(feature = "gpu")]
pub mod fuse;
#[cfg(feature = "gpu")]
pub mod harness;
#[cfg(feature = "gpu")]
pub(crate) mod pipeline;
#[cfg(feature = "gpu")]
pub mod stack_tile;
#[cfg(feature = "gpu")]
pub mod transfer;
#[cfg(feature = "gpu")]
pub mod vram;
#[cfg(feature = "gpu")]
pub mod warp;
