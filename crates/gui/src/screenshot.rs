// SPDX-License-Identifier: GPL-3.0-or-later

//! Writes a [`slint::Window::take_snapshot`] buffer to a PNG file. Not
//! feature-gated: `i-slint-core` 1.17.1's `Window::take_snapshot` carries
//! no `cfg` -- <https://docs.rs/slint/1.17.1/slint/struct.Window.html>'s
//! "Available on crate feature `raw-window-handle-06` only" against
//! `take_snapshot` belongs to the neighbouring `window_handle` method, not
//! to this one. Do not add that feature on the strength of the rendered
//! docs.

use std::fs;
use std::path::Path;

use slint::{Rgba8Pixel, SharedPixelBuffer};

/// Writes `buffer` to `path` as an 8-bit RGBA PNG, creating `path`'s parent
/// directory first -- the destination directory may not exist yet.
pub fn write_png(buffer: &SharedPixelBuffer<Rgba8Pixel>, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let file = fs::File::create(path)?;
    let mut encoder = png::Encoder::new(file, buffer.width(), buffer.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    writer
        .write_image_data(buffer.as_bytes())
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(())
}
