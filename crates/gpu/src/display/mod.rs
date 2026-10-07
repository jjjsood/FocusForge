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

use focusforge_core::timing::{TimingCounter, measurement_scope, record_timing};

use crate::pack::{pack_samples, pack_samples_into};
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

/// The kernel's uniform: 20 bytes, far under
/// `max_uniform_buffer_binding_size = 16 KiB`. Explicit `to_le_bytes`,
/// never a `bytemuck` transmute -- the workspace denies `unsafe_code`,
/// the same reason `crate::pack` gives.
struct Params {
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    index_scale: u32,
    /// Only read by the `overlay` entry point; `render` leaves this at 0.
    alpha: u32,
}

impl Params {
    fn to_bytes(&self) -> [u8; 20] {
        let mut bytes = [0u8; 20];
        bytes[0..4].copy_from_slice(&self.width.to_le_bytes());
        bytes[4..8].copy_from_slice(&self.height.to_le_bytes());
        bytes[8..12].copy_from_slice(&self.samples_per_pixel.to_le_bytes());
        bytes[12..16].copy_from_slice(&self.index_scale.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.alpha.to_le_bytes());
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

fn packed_upload_bytes(packed: &[u8]) -> u64 {
    packed.len() as u64
}

fn overlay_pipeline(device: &wgpu::Device) -> Pipeline {
    build_pipeline(
        device,
        WGSL_SRC,
        "focusforge-gpu-display-overlay",
        "overlay",
        &[
            storage_entry(0, true),
            uniform_entry(1),
            storage_texture_entry(2),
            storage_entry(3, true),
        ],
    )
}

/// Packs `colors` into one little-endian `u32` per colour, `0x00BBGGRR` --
/// the WGSL kernel's `overlay_colors` layout.
fn pack_colors(colors: &[[u8; 3]]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(colors.len() * 4);
    for &[r, g, b] in colors {
        let word = u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16);
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    bytes
}

/// Uploads `bytes` into a new [`create_storage_buffer`] via
/// `queue.write_buffer`. Timed and byte-counted so every caller's upload
/// cost is attributed, including [`render_overlay`]'s colour buffer.
fn upload_storage(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    bytes: &[u8],
    label: &str,
) -> wgpu::Buffer {
    let _scope = measurement_scope("gpu_display_upload");
    let buffer = create_storage_buffer(device, bytes.len() as u64, label);
    queue.write_buffer(&buffer, 0, bytes);
    record_timing(TimingCounter::UploadBytes, packed_upload_bytes(bytes));
    buffer
}

/// [`pack_samples`] plus [`upload_storage`], for the `u16` buffers both
/// [`DisplayRenderer::render`] and [`render_overlay`] send the kernel.
/// Packing is timed and byte-counted separately from the upload it feeds.
fn upload_samples(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    data: &[u16],
    label: &str,
) -> wgpu::Buffer {
    let packed = {
        let _scope = measurement_scope("gpu_display_pack");
        pack_samples(data)
    };
    record_timing(TimingCounter::PackedBytes, packed_upload_bytes(&packed));
    upload_storage(device, queue, &packed, label)
}

/// The display kernel's pipeline, params buffer, source buffer and pack
/// scratch, retained across calls to [`Self::render`] rather than rebuilt
/// per frame -- one instance per attached device, shared by every pane that
/// draws through it.
pub struct DisplayRenderer {
    pipeline: Pipeline,
    params_buffer: wgpu::Buffer,
    source_buffer: Option<wgpu::Buffer>,
    source_capacity: u64,
    pack_scratch: Vec<u8>,
}

impl DisplayRenderer {
    /// Builds the tonemap pipeline and the fixed-size params buffer once;
    /// the source buffer is created lazily on the first [`Self::render`],
    /// once its byte length is known.
    pub fn new(device: &wgpu::Device) -> Self {
        let pipeline = tonemap_pipeline(device);
        let params_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("focusforge-gpu-display-params"),
            size: 20,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        record_timing(TimingCounter::AllocationBytes, 20);
        record_timing(TimingCounter::AllocationOperations, 1);
        Self {
            pipeline,
            params_buffer,
            source_buffer: None,
            source_capacity: 0,
            pack_scratch: Vec::new(),
        }
    }

    /// Keeps the source buffer while `needed <= capacity <= 4 * needed`,
    /// otherwise re-creates it at exactly `needed` bytes, so after every call
    /// the retained capacity lies within `[needed, 4 * needed]` of the
    /// current draw.
    fn ensure_source_buffer(&mut self, device: &wgpu::Device, needed: u64) {
        let reuse = self.source_buffer.is_some()
            && needed <= self.source_capacity
            && needed * 4 >= self.source_capacity;
        if reuse {
            return;
        }
        let buffer = create_storage_buffer(device, needed, "focusforge-gpu-display-src");
        record_timing(TimingCounter::AllocationBytes, needed);
        record_timing(TimingCounter::AllocationOperations, 1);
        self.source_buffer = Some(buffer);
        self.source_capacity = needed;
    }

    /// Runs the display kernel: packs `src` (`crate::pack::pack_samples_into`,
    /// into the retained scratch), uploads it into the retained source
    /// buffer, and dispatches one invocation per pixel of `target`. Creates
    /// no `MAP_READ` buffer -- readback exists only in
    /// [`read_display_texture`]. `src.len()` must equal exactly
    /// `target.width() * target.height() * samples_per_pixel`.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target: &DisplayTexture,
        src: &[u16],
        samples_per_pixel: u32,
        index_scale: u32,
    ) {
        let _scope = measurement_scope("gpu_display_render");
        let packed_len = {
            let _scope = measurement_scope("gpu_display_pack");
            pack_samples_into(src, &mut self.pack_scratch);
            self.pack_scratch.len() as u64
        };
        record_timing(TimingCounter::PackedBytes, packed_len);
        self.ensure_source_buffer(device, packed_len);
        {
            let _scope = measurement_scope("gpu_display_upload");
            let source_buffer = self
                .source_buffer
                .as_ref()
                .expect("ensure_source_buffer always leaves a buffer set");
            queue.write_buffer(source_buffer, 0, &self.pack_scratch);
            record_timing(TimingCounter::UploadBytes, packed_len);
        }

        let params = Params {
            width: target.width(),
            height: target.height(),
            samples_per_pixel,
            index_scale,
            alpha: 0,
        };
        queue.write_buffer(&self.params_buffer, 0, &params.to_bytes());

        let view = target
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default());

        run_pass_2d(
            device,
            queue,
            &self.pipeline,
            &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self
                        .source_buffer
                        .as_ref()
                        .expect("ensure_source_buffer always leaves a buffer set")
                        .as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: self.params_buffer.as_entire_binding(),
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
}

/// Blends each pixel's colour in `colors` over `composite` at `alpha` (`0`
/// transparent, `255` opaque) into `target`. `composite` is the edited
/// three-sample-per-pixel image. `composite.len()` must equal
/// `target.width() * target.height() * 3`; `colors.len()` must equal
/// `target.width() * target.height()`.
pub fn render_overlay(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    target: &DisplayTexture,
    composite: &[u16],
    colors: &[[u8; 3]],
    alpha: u8,
) {
    let _scope = measurement_scope("gpu_display_render_overlay");
    let composite_buffer =
        upload_samples(device, queue, composite, "focusforge-gpu-overlay-composite");
    let colors_buffer = upload_storage(
        device,
        queue,
        &pack_colors(colors),
        "focusforge-gpu-overlay-colors",
    );

    let params = Params {
        width: target.width(),
        height: target.height(),
        samples_per_pixel: 3,
        index_scale: 0,
        alpha: u32::from(alpha),
    };
    let params_buffer = crate::pipeline::upload_params(device, queue, &params.to_bytes());

    let view = target
        .texture()
        .create_view(&wgpu::TextureViewDescriptor::default());
    let pipeline = overlay_pipeline(device);

    run_pass_2d(
        device,
        queue,
        &pipeline,
        &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: composite_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: params_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: colors_buffer.as_entire_binding(),
            },
        ],
        target.width(),
        target.height(),
        "focusforge-gpu-display-overlay",
    );
}

#[cfg(test)]
mod tests {
    use super::packed_upload_bytes;
    use crate::pack::pack_samples;

    #[test]
    fn packed_source_byte_count_is_the_upload_byte_count_for_even_and_odd_samples() {
        for (samples, expected) in [(&[1_u16, 2, 3, 4][..], 8), (&[1_u16, 2, 3, 4, 5][..], 12)] {
            let packed = pack_samples(samples);
            assert_eq!(packed.len(), expected);
            assert_eq!(packed_upload_bytes(&packed), expected as u64);
        }
    }
}
