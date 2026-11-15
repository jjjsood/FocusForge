// SPDX-License-Identifier: GPL-3.0-or-later

//! The brush-preview kernel (`preview.wgsl`, the WGSL twin of
//! [`focusforge_core::retouch::preview::brush_preview`]) and its host side: the
//! [`plan`] that checks the inputs are inside what the kernel computes exactly,
//! and the [`BrushPreviewRenderer`] that uploads the samples a plan needs and
//! writes the layer into a [`DisplayTexture`]. Nothing here reads a buffer back
//! or waits for the device.

use focusforge_core::project::{Op, StrokeSource};
use focusforge_core::retouch::blend;
use focusforge_core::retouch::preview::{
    BrushPreviewGrid, BrushPreviewLayout, BrushPreviewSpec, brush_preview_layout,
};
use focusforge_core::tile::Rect;
use focusforge_core::timing::{TimingCounter, measurement_scope, record_timing};
use focusforge_core::zmap::ZMapInfo;

use super::{DisplayTexture, Retained};
use crate::blend::{
    BlendError, COORD_LIMIT, DETAIL_WGSL_SRC, RADIUS_LIMIT_16, blur_chain, entry, mode_code,
    refused, words_to_bytes,
};
use crate::pipeline::{
    Pipeline, build_pipeline, run_pass_2d, storage_entry, storage_texture_entry, uniform_entry,
    upload_params,
};
use crate::transfer::create_storage_buffer;

const WGSL_SRC: &str = concat!(
    include_str!("../mask.wgsl"),
    include_str!("dither.wgsl"),
    include_str!("preview.wgsl")
);

/// Q16 scale of masks, opacities and strengths: 1.0.
const OPAQUE: u32 = 1 << 16;

/// What [`BrushPreviewRenderer::render`] needs for one layer.
#[derive(Debug, Clone)]
pub struct BrushPreviewPlan {
    /// The layer's box on the result grid's lattice: its texel size and, with
    /// the grid's origin and step, its place in the image.
    pub layout: BrushPreviewLayout,
    /// The lattice box whose samples are uploaded: the output box, or Detail's
    /// blur window.
    upload: Rect,
    /// Image pixel of the result lattice's sample (0, 0), and its step.
    origin: (u32, u32),
    step: u32,
    centre: (i32, i32),
    /// Mask radius and core in sixteenths of a pixel.
    radius: u32,
    core: u32,
    /// [`mode_code`] of the brush's mode.
    mode: u32,
    opacity_q16: u32,
    strength_q16: u32,
}

impl BrushPreviewPlan {
    /// Bytes [`BrushPreviewRenderer::render`] uploads: the result and the
    /// source over the upload box, one `u32` a sample.
    pub fn upload_bytes(&self) -> u64 {
        2 * u64::from(self.upload.width) * u64::from(self.upload.height) * 12
    }
}

/// The preview layer of `spec` over `result` and `source` as the kernel
/// computes it, or `None` where [`focusforge_core::retouch::preview::brush_preview`]
/// has no layer for the same inputs: a grid with other than 3 samples a pixel
/// or a buffer of the wrong length, a strength above 65536, a brush out of
/// range, a footprint with no lattice sample of nonzero mask, a `result` that
/// does not hold the footprint (and Detail's window), or a `source` that does
/// not cover the whole box the kernel reads.
///
/// [`BlendError::Refused`] names what the kernel does not compute exactly:
/// `radius` (a mask radius of 2^16 sixteenths of a pixel or more), `coordinate`
/// (a lattice pixel of the upload box or the centre at 2^14 or more) and
/// `buffer` (an upload box past `max_binding` bytes at one `u32` a sample).
pub fn plan(
    image_size: (u32, u32),
    result: &BrushPreviewGrid,
    source: &BrushPreviewGrid,
    spec: &BrushPreviewSpec,
    strength_q16: u32,
    max_binding: u64,
) -> Result<Option<BrushPreviewPlan>, BlendError> {
    if strength_q16 > OPAQUE {
        return Ok(None);
    }
    for grid in [result, source] {
        let lattice = grid.rect.width.div_ceil(grid.step.get()) as usize
            * grid.rect.height.div_ceil(grid.step.get()) as usize;
        if grid.samples_per_pixel != 3 || grid.samples.len() != lattice * 3 {
            return Ok(None);
        }
    }
    let Ok(info) = ZMapInfo::new(image_size.0, image_size.1, 1) else {
        return Ok(None);
    };
    let op = Op::BrushStroke {
        source: StrokeSource::Frame(0),
        path: vec![spec.centre],
        radius: spec.radius,
        softness: spec.softness,
        mode: spec.mode,
        opacity: spec.opacity,
    };
    let Ok((radius, core)) = blend::mask_ramp(&op, info) else {
        return Ok(None);
    };
    if radius >= RADIUS_LIMIT_16 {
        return Err(refused(
            "radius",
            format!("of {radius} sixteenths of a pixel reaches {RADIUS_LIMIT_16}"),
        ));
    }
    let Some(layout) = brush_preview_layout(image_size, result, spec) else {
        return Ok(None);
    };

    let upload = match layout.detail {
        Some((window, _)) => window,
        None => Rect {
            x: layout.column,
            y: layout.row,
            width: layout.width,
            height: layout.height,
        },
    };
    let step = result.step.get();
    let limit = COORD_LIMIT as u32;
    let far = |origin: u32, from: u32, len: u32| {
        u64::from(origin) + u64::from(from + len - 1) * u64::from(step)
    };
    let (far_x, far_y) = (
        far(result.rect.x, upload.x, upload.width),
        far(result.rect.y, upload.y, upload.height),
    );
    if let Some(v) = [far_x, far_y]
        .into_iter()
        .chain([spec.centre.x, spec.centre.y].map(|c| u64::from(c.unsigned_abs())))
        .find(|&v| v >= u64::from(limit))
    {
        return Err(refused("coordinate", format!("{v} reaches {limit}")));
    }
    let bytes = u64::from(upload.width) * u64::from(upload.height) * 12;
    if bytes > max_binding {
        return Err(refused(
            "buffer",
            format!("of {bytes} bytes exceeds max_storage_buffer_binding_size={max_binding}"),
        ));
    }

    // The source answers wherever the kernel reads it: the box is a rectangle
    // of lattice pixels, so its two corners decide.
    let read = match layout.detail {
        Some((window, _)) => window,
        None => layout.footprint,
    };
    let answers = |i: u32, j: u32| {
        source
            .nearest(result.rect.x + i * step, result.rect.y + j * step)
            .is_some()
    };
    if !answers(read.x, read.y) || !answers(read.x + read.width - 1, read.y + read.height - 1) {
        return Ok(None);
    }

    Ok(Some(BrushPreviewPlan {
        layout,
        upload,
        origin: (result.rect.x, result.rect.y),
        step,
        centre: (spec.centre.x, spec.centre.y),
        radius: radius as u32,
        core: core as u32,
        mode: mode_code(spec.mode),
        opacity_q16: blend::opacity_q16(spec.opacity),
        strength_q16,
    }))
}

/// Words of `plan`'s upload box, one `u32` a sample: the result's, and the
/// source read by nearest sample at each lattice sample's pixel (0 outside the
/// footprint box, where the mask is 0, unless Detail reads all of the box).
/// `source` must cover what [`plan`] checked.
fn gather_words(
    plan: &BrushPreviewPlan,
    result: &BrushPreviewGrid,
    source: &BrushPreviewGrid,
    result_words: &mut Vec<u32>,
    source_words: &mut Vec<u32>,
) {
    let (up, fp) = (plan.upload, plan.layout.footprint);
    let columns = result.rect.width.div_ceil(plan.step) as usize;
    let whole = plan.layout.detail.is_some();
    result_words.clear();
    source_words.clear();
    for j in up.y..up.y + up.height {
        let at = (j as usize * columns + up.x as usize) * 3;
        result_words.extend(
            result.samples[at..at + up.width as usize * 3]
                .iter()
                .map(|&s| u32::from(s)),
        );
        for i in up.x..up.x + up.width {
            let inside =
                whole || (i >= fp.x && i < fp.x + fp.width && j >= fp.y && j < fp.y + fp.height);
            if inside {
                let pixel = source
                    .nearest(plan.origin.0 + i * plan.step, plan.origin.1 + j * plan.step)
                    .expect("the plan checked that the source covers the box");
                source_words.extend(pixel.iter().map(|&s| u32::from(s)));
            } else {
                source_words.extend([0; 3]);
            }
        }
    }
}

/// The brush-preview pipelines and the buffers they read, retained across
/// calls to [`Self::render`].
pub struct BrushPreviewRenderer {
    pipeline: Pipeline,
    box_pass: Pipeline,
    detail_target: Pipeline,
    params: wgpu::Buffer,
    /// Stands in for the Detail target of the other modes.
    placeholder: wgpu::Buffer,
    result: Retained,
    source: Retained,
    scratch: Retained,
    blurred_result: Retained,
    blurred_source: Retained,
    target: Retained,
    result_words: Vec<u32>,
    source_words: Vec<u32>,
    bytes: Vec<u8>,
}

impl BrushPreviewRenderer {
    pub fn new(device: &wgpu::Device) -> Self {
        let pipeline = build_pipeline(
            device,
            WGSL_SRC,
            "focusforge-gpu-brush-preview",
            "main",
            &[
                storage_entry(0, true),
                storage_entry(1, true),
                storage_entry(2, true),
                uniform_entry(3),
                storage_texture_entry(4),
            ],
        );
        let box_pass = build_pipeline(
            device,
            DETAIL_WGSL_SRC,
            "focusforge-gpu-detail-box",
            "box_pass",
            &[
                storage_entry(0, true),
                storage_entry(1, false),
                uniform_entry(2),
            ],
        );
        let detail_target = build_pipeline(
            device,
            DETAIL_WGSL_SRC,
            "focusforge-gpu-detail-target",
            "detail_target",
            &[
                storage_entry(3, true),
                storage_entry(4, true),
                storage_entry(5, true),
                storage_entry(6, false),
                uniform_entry(7),
            ],
        );
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("focusforge-gpu-brush-preview-params"),
            size: PARAM_WORDS * 4,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            box_pass,
            detail_target,
            params,
            placeholder: create_storage_buffer(device, 4, "focusforge-gpu-brush-preview-none"),
            result: Retained::default(),
            source: Retained::default(),
            scratch: Retained::default(),
            blurred_result: Retained::default(),
            blurred_source: Retained::default(),
            target: Retained::default(),
            result_words: Vec::new(),
            source_words: Vec::new(),
            bytes: Vec::new(),
        }
    }

    /// Writes `plan`'s layer into `target`, which holds `plan.layout.width` by
    /// `plan.layout.height` texels. `result` and `source` are the grids `plan`
    /// was made from. The call encodes and submits and returns: no `MAP_READ`
    /// buffer, no device poll.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target: &DisplayTexture,
        plan: &BrushPreviewPlan,
        result: &BrushPreviewGrid,
        source: &BrushPreviewGrid,
    ) {
        let _scope = measurement_scope("gpu_brush_preview_render");
        let layout = &plan.layout;
        debug_assert_eq!(
            (target.width(), target.height()),
            (layout.width, layout.height)
        );
        let up = plan.upload;
        let bytes = u64::from(up.width) * u64::from(up.height) * 12;
        gather_words(
            plan,
            result,
            source,
            &mut self.result_words,
            &mut self.source_words,
        );
        for (retained, words, label) in [
            (
                &mut self.result,
                &self.result_words,
                "focusforge-gpu-brush-preview-result",
            ),
            (
                &mut self.source,
                &self.source_words,
                "focusforge-gpu-brush-preview-source",
            ),
        ] {
            retained.ensure(device, bytes, label);
            self.bytes.clear();
            self.bytes
                .extend(words.iter().flat_map(|w| w.to_le_bytes()));
            queue.write_buffer(retained.buffer(), 0, &self.bytes);
            record_timing(TimingCounter::UploadBytes, bytes);
        }

        let fp = layout.footprint;
        if let Some((window, pass)) = layout.detail {
            let window_bytes = u64::from(window.width) * u64::from(window.height) * 12;
            for (retained, size, label) in [
                (&mut self.scratch, window_bytes, "scratch"),
                (&mut self.blurred_result, window_bytes, "blurred-result"),
                (&mut self.blurred_source, window_bytes, "blurred-source"),
                (
                    &mut self.target,
                    u64::from(fp.width) * u64::from(fp.height) * 12,
                    "target",
                ),
            ] {
                retained.ensure(
                    device,
                    size,
                    &format!("focusforge-gpu-brush-preview-{label}"),
                );
            }
            let (r, s) = (self.result.buffer(), self.source.buffer());
            let (b, cr, cs) = (
                self.scratch.buffer(),
                self.blurred_result.buffer(),
                self.blurred_source.buffer(),
            );
            // Three horizontal and vertical passes ending in `cr` and `cs`,
            // leaving the unblurred samples as they were.
            blur_chain(
                device,
                queue,
                &self.box_pass,
                [r, b, cr, b, cr, b, cr],
                (window, 3, pass),
            );
            blur_chain(
                device,
                queue,
                &self.box_pass,
                [s, b, cs, b, cs, b, cs],
                (window, 3, pass),
            );
            let params = upload_params(
                device,
                queue,
                &words_to_bytes(&[
                    fp.x,
                    fp.y,
                    fp.width,
                    fp.height,
                    3,
                    window.x,
                    window.y,
                    window.width,
                    window.x,
                    window.y,
                    window.width,
                    0,
                ]),
            );
            run_pass_2d(
                device,
                queue,
                &self.detail_target,
                &[
                    entry(3, cr),
                    entry(4, s),
                    entry(5, cs),
                    entry(6, self.target.buffer()),
                    entry(7, &params),
                ],
                fp.width,
                fp.height,
                "focusforge-gpu-detail-target",
            );
        }

        let words: [u32; PARAM_WORDS as usize] = [
            layout.width,
            layout.height,
            layout.column,
            layout.row,
            up.x,
            up.y,
            up.width,
            fp.x,
            fp.y,
            fp.width,
            plan.origin.0,
            plan.origin.1,
            plan.step,
            plan.radius,
            plan.core,
            plan.mode,
            u32::from(layout.detail.is_some()),
            plan.opacity_q16,
            plan.strength_q16,
            plan.centre.0 as u32,
            plan.centre.1 as u32,
            0,
            0,
            0,
        ];
        queue.write_buffer(&self.params, 0, &words_to_bytes(&words));
        let view = target
            .texture()
            .create_view(&wgpu::TextureViewDescriptor::default());
        let detail = if layout.detail.is_some() {
            self.target.buffer()
        } else {
            &self.placeholder
        };
        run_pass_2d(
            device,
            queue,
            &self.pipeline,
            &[
                entry(0, self.result.buffer()),
                entry(1, self.source.buffer()),
                entry(2, detail),
                entry(3, &self.params),
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
            ],
            layout.width,
            layout.height,
            "focusforge-gpu-brush-preview",
        );
    }
}

/// Words of the kernel's uniform.
const PARAM_WORDS: u64 = 24;

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use focusforge_core::display::reduce16to8;
    use focusforge_core::project::{BrushMode, Point};
    use focusforge_core::retouch::preview::brush_preview;

    use super::*;
    use crate::blend::tests::kernel;

    const BIG: u64 = 1 << 30;
    const STRENGTHS: [u32; 2] = [52429, 65536];

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn spec(
        centre: (i32, i32),
        radius: f32,
        softness: f32,
        mode: BrushMode,
        opacity: f32,
    ) -> BrushPreviewSpec {
        BrushPreviewSpec {
            centre: Point {
                x: centre.0,
                y: centre.1,
            },
            radius,
            softness,
            mode,
            opacity,
        }
    }

    /// A grid and the buffer behind it.
    struct Owned {
        rect: Rect,
        step: u32,
        samples: Vec<u16>,
    }

    impl Owned {
        fn new(rect: Rect, step: u32, sample: impl Fn(usize) -> u16) -> Self {
            let len = (rect.width.div_ceil(step) * rect.height.div_ceil(step) * 3) as usize;
            Self {
                rect,
                step,
                samples: (0..len).map(sample).collect(),
            }
        }

        fn grid(&self) -> BrushPreviewGrid<'_> {
            BrushPreviewGrid {
                rect: self.rect,
                step: NonZeroU32::new(self.step).unwrap(),
                samples_per_pixel: 3,
                samples: &self.samples,
            }
        }
    }

    fn noise(seed: u32) -> impl Fn(usize) -> u16 {
        move |i| {
            let mut s = (i as u32 ^ seed).wrapping_mul(2_654_435_761).max(1);
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s >> 8) as u16
        }
    }

    /// The kernel's per-texel arithmetic in its own `u32` steps, over the
    /// words the renderer uploads and the blur it runs: RGBA8 row-major.
    fn mirror(
        plan: &BrushPreviewPlan,
        result: &BrushPreviewGrid,
        source: &BrushPreviewGrid,
    ) -> Vec<u8> {
        let (mut r, mut s) = (Vec::new(), Vec::new());
        gather_words(plan, result, source, &mut r, &mut s);
        let (up, fp) = (plan.upload, plan.layout.footprint);
        let word_at = |i: u32, j: u32| ((j - up.y) * up.width + (i - up.x)) as usize * 3;
        let target = plan.layout.detail.map(|(_, pass)| {
            let blur = |words: &[u32]| {
                let mut v = words.to_vec();
                for _ in 0..3 {
                    for horizontal in [true, false] {
                        v = kernel::box_pass(&v, (up.width, up.height), 3, pass, horizontal);
                    }
                }
                v
            };
            let (blurred_result, blurred_source) = (blur(&r), blur(&s));
            let mut target = Vec::new();
            for j in fp.y..fp.y + fp.height {
                for i in fp.x..fp.x + fp.width {
                    let at = word_at(i, j);
                    for c in 0..3 {
                        target.push(kernel::detail_target(
                            blurred_result[at + c],
                            s[at + c],
                            blurred_source[at + c],
                        ));
                    }
                }
            }
            target
        });

        let layout = &plan.layout;
        let mut texels = Vec::new();
        for y in 0..layout.height {
            for x in 0..layout.width {
                let (i, j) = (layout.column + x, layout.row + y);
                let (px, py) = (
                    (plan.origin.0 + i * plan.step) as i32,
                    (plan.origin.1 + j * plan.step) as i32,
                );
                let (cx, cy) = plan.centre;
                let d2 = kernel::segment_d2_256(px, py, [cx, cy, cx, cy]);
                let mask = kernel::alpha(plan.radius, plan.core, kernel::isqrt(d2));
                let at = word_at(i, j);
                let mut rgb = [r[at], r[at + 1], r[at + 2]];
                if mask != 0 {
                    let src: Vec<u32> = match &target {
                        Some(target) => {
                            let t = ((j - fp.y) * fp.width + (i - fp.x)) as usize * 3;
                            target[t..t + 3].to_vec()
                        }
                        None => s[at..at + 3].to_vec(),
                    };
                    kernel::mix_pixel(plan.mode, &mut rgb, &src, OPAQUE);
                }
                let a16 = kernel::scaled_alpha(
                    kernel::scaled_alpha(mask, plan.opacity_q16),
                    plan.strength_q16,
                );
                for v in rgb {
                    texels.push(reduce16to8(v as u16, x, y));
                }
                texels.push(((a16 * 255 + 32768) >> 16) as u8);
            }
        }
        texels
    }

    /// Plans and mirrors one case against the twin; whether the twin drew.
    fn check(
        image: (u32, u32),
        (result, source): (&Owned, &Owned),
        brush: &BrushPreviewSpec,
        strength: u32,
    ) -> bool {
        let label = format!("{brush:?} strength {strength}");
        let planned = plan(image, &result.grid(), &source.grid(), brush, strength, BIG).unwrap();
        let twin = brush_preview(image, &result.grid(), &source.grid(), brush, strength);
        match (planned, twin) {
            (None, None) => false,
            (Some(plan), Some(twin)) => {
                let layout = plan.layout;
                assert_eq!(
                    (layout.column, layout.row, layout.width, layout.height),
                    (twin.column, twin.row, twin.width, twin.height),
                    "{label}"
                );
                assert_eq!(layout.column % 4 + layout.row % 4, 0, "{label}");
                assert!(
                    mirror(&plan, &result.grid(), &source.grid()) == twin.texels,
                    "{label}: the mirror differs from the twin"
                );
                true
            }
            (planned, twin) => panic!(
                "{label}: plan {} and the twin {}",
                planned.is_some(),
                twin.is_some()
            ),
        }
    }

    #[test]
    fn the_kernel_mirror_gives_the_twins_layer_at_the_extremes() {
        let image = (96, 80);
        let area = rect(1, 1, 95, 79);
        // Dark under bright, bright under dark, equal grids, noise.
        type Fill = Box<dyn Fn(usize) -> u16>;
        let fills: [(Fill, Fill); 4] = [
            (Box::new(|_| 0), Box::new(|_| 65535)),
            (Box::new(|_| 65535), Box::new(|_| 0)),
            (Box::new(|_| 65535), Box::new(|_| 65535)),
            (Box::new(noise(1)), Box::new(noise(2))),
        ];
        let mut drawn = [0usize; 4];
        for (result, source) in &fills {
            let result = Owned::new(area, 2, result);
            let source = Owned::new(area, 2, source);
            for (m, mode) in [
                BrushMode::Normal,
                BrushMode::Lighten,
                BrushMode::Darken,
                BrushMode::Detail,
            ]
            .into_iter()
            .enumerate()
            {
                for opacity in [1.0 / 65536.0, 0.5, 1.0] {
                    for softness in [0.0, 0.5, 1.0] {
                        for radius in [1.5, 20.0] {
                            for strength in STRENGTHS {
                                let brush = spec((48, 42), radius, softness, mode, opacity);
                                drawn[m] +=
                                    usize::from(check(image, (&result, &source), &brush, strength));
                            }
                        }
                    }
                }
            }
        }
        assert!(drawn.iter().all(|&n| n > 0), "every mode drew: {drawn:?}");
    }

    #[test]
    fn the_kernel_mirror_holds_at_the_widest_detail_pass() {
        // A radius just under 4096 px: each of Detail's passes is 1365 samples
        // wide, and a window sum of full-scale samples stays within u32.
        let image = (9000, 4);
        let whole = rect(0, 0, 9000, 4);
        for (result, source) in [(0u16, 65535u16), (65535, 0)] {
            let result = Owned::new(whole, 1, move |_| result);
            let source = Owned::new(whole, 1, move |_| source);
            let brush = spec((4500, 2), 4095.9, 0.5, BrushMode::Detail, 1.0);
            let planned = plan(image, &result.grid(), &source.grid(), &brush, 65536, BIG)
                .unwrap()
                .expect("a layer");
            assert_eq!(planned.layout.detail.map(|(_, pass)| pass), Some(1365));
            assert!(check(image, (&result, &source), &brush, 65536));
        }
    }

    #[test]
    fn the_plan_refuses_by_name_and_is_empty_where_the_twin_is() {
        let image = (300, 300);
        let full = rect(0, 0, 300, 300);
        let result = Owned::new(full, 1, noise(1));
        let source = Owned::new(full, 1, noise(2));
        let ask = |result: &BrushPreviewGrid,
                   source: &BrushPreviewGrid,
                   brush: &BrushPreviewSpec,
                   strength: u32,
                   max: u64| plan(image, result, source, brush, strength, max);
        let ok = spec((150, 150), 10.0, 0.5, BrushMode::Normal, 1.0);
        let (r, s) = (result.grid(), source.grid());
        assert!(ask(&r, &s, &ok, 52429, BIG).unwrap().is_some());

        let refusal = |outcome: Result<Option<BrushPreviewPlan>, BlendError>| match outcome {
            Err(BlendError::Refused { limit, .. }) => limit,
            other => panic!("expected a refusal, got {other:?}"),
        };
        let huge = spec((150, 150), 4096.0, 0.5, BrushMode::Normal, 1.0);
        assert_eq!(refusal(ask(&r, &s, &huge, 52429, BIG)), "radius");
        assert_eq!(refusal(ask(&r, &s, &ok, 52429, 100)), "buffer");

        // A box whose far lattice pixel reaches 2^14.
        let wide = rect(0, 0, 17_000, 30);
        let (far_result, far_source) =
            (Owned::new(wide, 8, noise(1)), Owned::new(wide, 8, noise(2)));
        let far = spec((16_990, 15), 10.0, 0.5, BrushMode::Normal, 1.0);
        let outcome = plan(
            (17_000, 30),
            &far_result.grid(),
            &far_source.grid(),
            &far,
            52429,
            BIG,
        );
        assert_eq!(refusal(outcome), "coordinate");

        let empty = |what: &str, outcome: Result<Option<BrushPreviewPlan>, BlendError>| {
            assert!(matches!(outcome, Ok(None)), "{what}: {outcome:?}");
        };
        empty("strength above 1.0", ask(&r, &s, &ok, 65537, BIG));
        let grey = BrushPreviewGrid {
            samples_per_pixel: 1,
            samples: &result.samples[..300 * 300],
            ..result.grid()
        };
        empty("a 1-sample result", ask(&grey, &s, &ok, 52429, BIG));
        empty("a 1-sample source", ask(&r, &grey, &ok, 52429, BIG));
        let short = BrushPreviewGrid {
            samples: &result.samples[3..],
            ..result.grid()
        };
        empty("a short buffer", ask(&short, &s, &ok, 52429, BIG));
        let sparse = (Owned::new(full, 4, noise(1)), Owned::new(full, 4, noise(2)));
        let between = spec((2, 2), 1.5, 0.5, BrushMode::Normal, 1.0);
        empty(
            "a disc between lattice samples",
            ask(&sparse.0.grid(), &sparse.1.grid(), &between, 52429, BIG),
        );
        let part = Owned::new(rect(0, 0, 150, 150), 1, noise(2));
        empty(
            "a source short of the disc",
            ask(&r, &part.grid(), &ok, 52429, BIG),
        );
    }

    #[test]
    fn the_upload_at_radius_100_stays_within_the_stated_size() {
        let image = (600, 600);
        let full = rect(0, 0, 600, 600);
        let (result, source) = (Owned::new(full, 1, noise(1)), Owned::new(full, 1, noise(2)));
        for (mode, limit) in [
            (BrushMode::Normal, 1_000_000),
            (BrushMode::Lighten, 1_000_000),
            (BrushMode::Darken, 1_000_000),
            (BrushMode::Detail, 4_000_000),
        ] {
            let brush = spec((300, 300), 100.0, 0.5, mode, 1.0);
            let planned = plan(image, &result.grid(), &source.grid(), &brush, 52429, BIG)
                .unwrap()
                .expect("a layer");
            eprintln!("{mode:?} uploads {} bytes", planned.upload_bytes());
            assert!(planned.upload_bytes() <= limit, "{mode:?}");
        }
    }
}
