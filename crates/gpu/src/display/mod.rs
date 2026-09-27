// SPDX-License-Identifier: GPL-3.0-or-later

//! The display kernel: a 16-to-8-bit reduction with ordered dither and no
//! tone curve (`tonemap.wgsl`, the WGSL twin of
//! `focusforge_core::display::tonemap_cpu`), the display texture Slint
//! imports, and the one readback path that exists only for the parity test.
//! The only kernel in this crate that exists because of a display
//! constraint (Slint's `Rgba8Unorm`/`Rgba8UnormSrgb` texture-importer
//! contract) rather than a performance one.

pub mod readback;
pub use readback::read_display_texture;

use crate::pack::pack_samples;
use crate::pipeline::{
    Pipeline, build_pipeline, run_pass_2d, storage_entry, storage_texture_entry, uniform_entry,
};
use crate::transfer::create_storage_buffer;

const WGSL_SRC: &str = include_str!("tonemap.wgsl");

/// The one format Slint's texture importer accepts that this crate uses.
/// `Rgba8UnormSrgb` would linearise on sample, a second correction on
/// already display-referred material.
pub const DISPLAY_TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// The usage set the display texture is created with: `STORAGE_BINDING` for
/// the kernel's write, `COPY_SRC` for [`read_display_texture`], and
/// `TEXTURE_BINDING | RENDER_ATTACHMENT`, the pair
/// `slint::Image::try_from(wgpu::Texture)` checks.
pub const DISPLAY_TEXTURE_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::from_bits_truncate(
    wgpu::TextureUsages::STORAGE_BINDING.bits()
        | wgpu::TextureUsages::TEXTURE_BINDING.bits()
        | wgpu::TextureUsages::RENDER_ATTACHMENT.bits()
        | wgpu::TextureUsages::COPY_SRC.bits(),
);

/// The display surface: allocated once, re-used across frames, and
/// re-allocated only when the image it shows changes size. One per
/// pane -- `crates/gui/src/gpu_canvas.rs` owns two.
pub struct DisplayTexture {
    texture: wgpu::Texture,
    width: u32,
    height: u32,
}

impl DisplayTexture {
    /// Creates a new `Rgba8Unorm` texture of exactly `width x height`, with
    /// both usages `Image::try_from` checks plus this crate's own two
    /// additions.
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("focusforge-gpu-display-texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DISPLAY_TEXTURE_FORMAT,
            usage: DISPLAY_TEXTURE_USAGE,
            view_formats: &[],
        });
        DisplayTexture {
            texture,
            width,
            height,
        }
    }

    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

/// The kernel's uniform: 16 bytes, far under
/// `max_uniform_buffer_binding_size = 16 KiB`. Explicit `to_le_bytes`,
/// never a `bytemuck` transmute -- the workspace denies `unsafe_code`,
/// the same reason `crate::pack` gives.
struct Params {
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    index_scale: u32,
}

impl Params {
    fn to_bytes(&self) -> [u8; 16] {
        let mut bytes = [0u8; 16];
        bytes[0..4].copy_from_slice(&self.width.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.height.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.samples_per_pixel.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.index_scale.to_le_bytes());
        bytes
    }
}

fn tonemap_pipeline(device: &wgpu::Device) -> Pipeline {
    build_pipeline(
        device,
        WGSL_SRC,
        "focusforge-gpu-display-tonemap",
        "main",
        &[
            storage_entry(0, true),
            uniform_entry(1),
            storage_texture_entry(2),
        ],
    )
}

/// Runs the display kernel: packs `src`'s `u16` samples two-per-`u32`
/// (`crate::pack::pack_samples`), uploads them as a storage buffer, and
/// dispatches one invocation per pixel of `target`, writing straight into
/// it. Calls no `download_*` and creates no `MAP_READ` buffer -- readback
/// exists only in [`read_display_texture`], for the parity test.
///
/// `src.len()` must equal exactly
/// `target.width() * target.height() * samples_per_pixel`.
pub fn render(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    target: &DisplayTexture,
    src: &[u16],
    samples_per_pixel: u32,
    index_scale: u32,
) {
    let packed = pack_samples(src);
    let src_buffer =
        create_storage_buffer(device, packed.len() as u64, "focusforge-gpu-display-src");
    queue.write_buffer(&src_buffer, 0, &packed);

    let params = Params {
        width: target.width(),
        height: target.height(),
        samples_per_pixel,
        index_scale,
    };
    let params_buffer = crate::pipeline::upload_params(device, queue, &params.to_bytes());

    let view = target
        .texture()
        .create_view(&wgpu::TextureViewDescriptor::default());
    let pipeline = tonemap_pipeline(device);

    run_pass_2d(
        device,
        queue,
        &pipeline,
        &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: src_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: params_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(&view),
            },
        ],
        target.width(),
        target.height(),
        "focusforge-gpu-display-tonemap",
    );
}
