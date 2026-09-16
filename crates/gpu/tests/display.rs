// SPDX-License-Identifier: GPL-3.0-or-later

//! The two device-free assertions about the display surface: the format and
//! usage `slint::Image::try_from(wgpu::Texture)` checks. No device is
//! created here; every device test lives in `--test harness`.

#![cfg(feature = "gpu")]

use focusforge_gpu::display::{DISPLAY_TEXTURE_FORMAT, DISPLAY_TEXTURE_USAGE};

#[test]
fn display_texture_format_is_rgba8unorm() {
    assert_eq!(DISPLAY_TEXTURE_FORMAT, wgpu::TextureFormat::Rgba8Unorm);
}

#[test]
fn display_texture_usage_satisfies_slint() {
    // The pair `Image::try_from` actually checks.
    assert!(DISPLAY_TEXTURE_USAGE.contains(wgpu::TextureUsages::TEXTURE_BINDING));
    assert!(DISPLAY_TEXTURE_USAGE.contains(wgpu::TextureUsages::RENDER_ATTACHMENT));
    // This crate's own additions: the kernel's write, and the parity test's
    // readback.
    assert!(DISPLAY_TEXTURE_USAGE.contains(wgpu::TextureUsages::STORAGE_BINDING));
    assert!(DISPLAY_TEXTURE_USAGE.contains(wgpu::TextureUsages::COPY_SRC));
}
