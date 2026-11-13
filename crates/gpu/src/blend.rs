// SPDX-License-Identifier: GPL-3.0-or-later

//! The WGSL twin of [`focusforge_core::retouch::blend::apply`]: one soft brush
//! stroke blended over a region in the same integer fixed point. WGSL has
//! 32-bit integers, so the kernel emulates the CPU's wide mask inside the
//! bounded domain [`Blender::apply`] states and refuses outside it.

use std::sync::Arc;

use focusforge_core::project::{BrushMode, Op, Point};
use focusforge_core::retouch::{RetouchError, blend};
use focusforge_core::tile::Rect;
use focusforge_core::zmap::ZMapInfo;

use crate::device::GpuError;
use crate::errors::with_validation_scope;
use crate::pipeline::{
    Pipeline, build_pipeline, run_pass_2d, storage_entry, uniform_entry, upload_params,
};
use crate::transfer::{create_storage_buffer, download_buffer, upload_storage};

const WGSL_SRC: &str = include_str!("blend.wgsl");
const DETAIL_WGSL_SRC: &str = include_str!("detail.wgsl");

/// Coordinates the kernel accepts lie strictly between `-COORD_LIMIT` and
/// `COORD_LIMIT`.
const COORD_LIMIT: i32 = 1 << 14;
/// The mask radius, in sixteenths of a pixel, the kernel refuses from.
const RADIUS_LIMIT_16: u64 = 1 << 16;
/// Most samples a pixel may have: a channel sum of that many full-scale
/// samples still fits `u32`.
const MAX_SPP: usize = 65_537;

/// Why [`Blender::apply`] did not blend.
#[derive(Debug, thiserror::Error)]
pub enum BlendError {
    /// The stroke or the buffers are invalid, as for the CPU blend.
    #[error(transparent)]
    Retouch(#[from] RetouchError),

    /// The stroke lies outside what the kernel computes exactly; the CPU
    /// blend handles it. `limit` names the bound: `coordinate`, `radius`,
    /// `buffer` or `samples`.
    #[error("the blend kernel refuses this stroke: {limit} {detail}")]
    Refused { limit: &'static str, detail: String },

    /// The device failed or was lost during the blend.
    #[error(transparent)]
    Gpu(#[from] GpuError),
}

fn refused(limit: &'static str, detail: String) -> BlendError {
    BlendError::Refused { limit, detail }
}

/// What the kernel needs for one stroke over one region.
#[derive(Debug)]
struct Plan {
    /// The part of the region the stroke can change.
    active: Rect,
    /// Mask radius and core, in sixteenths of a pixel.
    radius: u32,
    core: u32,
    /// The result and source samples the stroke reads: `active` widened by
    /// the mode's read margin, inside `out_rect` and `region` respectively.
    result_read: Rect,
    source_read: Rect,
    mode: BrushMode,
    /// Radius of each of Detail's box passes; 0 for every other mode.
    pass: u32,
    /// The stroke's opacity in Q16 ([`blend::opacity_q16`]).
    q16: u32,
    /// The path segments that can reach `active`, as `[ax, ay, bx, by]`.
    segments: Vec<[i32; 4]>,
}

/// Validates like [`blend::apply`], then reduces the stroke to a [`Plan`], or
/// `None` when it changes nothing in `region`. `max_binding` is the device's
/// largest storage-buffer binding.
#[allow(clippy::too_many_arguments)]
fn plan(
    op: &Op,
    info: ZMapInfo,
    out_rect: Rect,
    out_len: usize,
    region: Rect,
    src_len: usize,
    spp: usize,
    max_binding: u64,
) -> Result<Option<Plan>, BlendError> {
    let footprint = blend::footprint(op, info)?;
    let Op::BrushStroke { mode, opacity, .. } = op else {
        return Err(RetouchError::Unsupported.into());
    };
    if spp > MAX_SPP {
        return Err(refused(
            "samples",
            format!("of {spp} a pixel exceeds {MAX_SPP}"),
        ));
    }
    if !info.contains(out_rect) || out_rect.intersect(region) != Some(region) {
        return Err(RetouchError::TileOutsideMap { tile: region }.into());
    }
    let samples = |rect: Rect| rect.width as usize * rect.height as usize * spp;
    for (rect, len) in [(out_rect, out_len), (region, src_len)] {
        if len != samples(rect) {
            return Err(RetouchError::TileBufferSize {
                expected: samples(rect),
                actual: len,
            }
            .into());
        }
    }
    let Some(active) = footprint.and_then(|fp| region.intersect(fp)) else {
        return Ok(None);
    };
    let (radius, core) = blend::mask_ramp(op, info)?;
    if radius >= RADIUS_LIMIT_16 {
        return Err(refused(
            "radius",
            format!("of {radius} sixteenths of a pixel reaches {RADIUS_LIMIT_16}"),
        ));
    }
    // Detail reads a margin beyond `active` in both buffers; every other
    // mode reads `active` alone.
    let reach = blend::grow(active, blend::read_margin(op, info)?, info);
    let result_read = reach
        .intersect(out_rect)
        .expect("active lies inside out_rect");
    let source_read = reach.intersect(region).expect("active lies inside region");
    let pass = if *mode == BrushMode::Detail {
        // The radius refusal above bounds the pass radius by 1365, so a
        // window sum of 2p + 1 samples fits u32.
        let p = blend::pass_radius(op, info)?;
        debug_assert!((2 * u64::from(p) + 1) * 65_535 + u64::from(p) < 1 << 28);
        p
    } else {
        0
    };
    let limit = COORD_LIMIT as u32;
    for rect in [result_read, source_read] {
        if rect.x + rect.width > limit || rect.y + rect.height > limit {
            return Err(refused(
                "coordinate",
                format!("of the region {rect:?} reaches {limit}"),
            ));
        }
        let buffer_bytes = u64::from(rect.width) * u64::from(rect.height) * spp as u64 * 4;
        if buffer_bytes > max_binding {
            return Err(refused(
                "buffer",
                format!(
                    "of {buffer_bytes} bytes exceeds max_storage_buffer_binding_size={max_binding}"
                ),
            ));
        }
    }

    let Op::BrushStroke {
        path, radius: px, ..
    } = op
    else {
        return Err(RetouchError::Unsupported.into());
    };
    let pairs: Vec<(Point, Point)> = match path.as_slice() {
        [p] => vec![(*p, *p)],
        path => path.windows(2).map(|w| (w[0], w[1])).collect(),
    };
    // A pixel lies within `radius` of a segment only inside its bounding box
    // widened by the radius; a segment whose box misses `active` leaves every
    // pixel's alpha at 0, so the minimum over the rest is unchanged.
    let reach = f64::from(*px) + 1.0;
    let (x0, y0) = (f64::from(active.x), f64::from(active.y));
    let (x1, y1) = (
        f64::from(active.x + active.width - 1),
        f64::from(active.y + active.height - 1),
    );
    let segments: Vec<[i32; 4]> = pairs
        .iter()
        .filter(|(a, b)| {
            f64::from(a.x.max(b.x)) + reach >= x0
                && f64::from(a.x.min(b.x)) - reach <= x1
                && f64::from(a.y.max(b.y)) + reach >= y0
                && f64::from(a.y.min(b.y)) - reach <= y1
        })
        .map(|(a, b)| [a.x, a.y, b.x, b.y])
        .collect();
    if segments.is_empty() {
        return Ok(None);
    }
    if let Some(v) = segments
        .iter()
        .flatten()
        .find(|v| v.unsigned_abs() >= COORD_LIMIT.unsigned_abs())
    {
        return Err(refused("coordinate", format!("{v} reaches {COORD_LIMIT}")));
    }
    if segments.len() as u64 * 16 > max_binding {
        return Err(refused(
            "buffer",
            format!(
                "of {} path segments exceeds max_storage_buffer_binding_size={max_binding}",
                segments.len()
            ),
        ));
    }
    Ok(Some(Plan {
        active,
        radius: radius as u32,
        core: core as u32,
        result_read,
        source_read,
        mode: *mode,
        pass,
        q16: blend::opacity_q16(*opacity),
        segments,
    }))
}

/// `rect`'s samples inside `active` as little-endian `u32` words, one a
/// sample: a word shared by two samples could not be written without a race.
fn gather(samples: &[u16], rect: Rect, active: Rect, spp: usize) -> Vec<u8> {
    let row_len = active.width as usize * spp;
    let mut bytes = Vec::with_capacity(row_len * active.height as usize * 4);
    for y in active.y..active.y + active.height {
        let at = ((y - rect.y) as usize * rect.width as usize + (active.x - rect.x) as usize) * spp;
        for &s in &samples[at..at + row_len] {
            bytes.extend_from_slice(&u32::from(s).to_le_bytes());
        }
    }
    bytes
}

fn entry(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

/// The kernel's mode word: 0 mixes toward the source (Normal, and Detail's
/// target), 1 Lighten, 2 Darken.
fn mode_code(mode: BrushMode) -> u32 {
    match mode {
        BrushMode::Normal | BrushMode::Detail => 0,
        BrushMode::Lighten => 1,
        BrushMode::Darken => 2,
    }
}

/// `words` as little-endian bytes.
fn words_to_bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// The kernel's uniform: twelve little-endian words, the last two padding.
fn params_bytes(plan: &Plan, spp: usize) -> Vec<u8> {
    words_to_bytes(&[
        plan.active.x,
        plan.active.y,
        plan.active.width,
        plan.active.height,
        spp as u32,
        plan.radius,
        plan.core,
        plan.segments.len() as u32,
        mode_code(plan.mode),
        plan.q16,
        0,
        0,
    ])
}

/// The box pass uniform: eight words, the last three padding.
fn box_params_bytes(read: Rect, spp: usize, p: u32, horizontal: bool) -> Vec<u8> {
    words_to_bytes(&[
        read.width,
        read.height,
        spp as u32,
        p,
        u32::from(horizontal),
        0,
        0,
        0,
    ])
}

/// The Detail target uniform: twelve words, the last one padding.
fn target_params_bytes(plan: &Plan, spp: usize) -> Vec<u8> {
    words_to_bytes(&[
        plan.active.x,
        plan.active.y,
        plan.active.width,
        plan.active.height,
        spp as u32,
        plan.result_read.x,
        plan.result_read.y,
        plan.result_read.width,
        plan.source_read.x,
        plan.source_read.y,
        plan.source_read.width,
        0,
    ])
}

/// The blend kernel bound to a device. Built once on the device it runs on --
/// Slint's in the GUI, the shared compute device in the parity test -- and
/// cheap to clone.
#[derive(Clone)]
pub struct Blender {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipelines: Arc<Pipelines>,
}

/// The blend pass and Detail's two prepass kernels.
struct Pipelines {
    blend: Pipeline,
    box_pass: Pipeline,
    detail_target: Pipeline,
}

impl Blender {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let blend = build_pipeline(
            device,
            WGSL_SRC,
            "focusforge-gpu-blend",
            "main",
            &[
                storage_entry(0, false),
                storage_entry(1, true),
                storage_entry(2, true),
                uniform_entry(3),
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
        Self {
            device: device.clone(),
            queue: queue.clone(),
            pipelines: Arc::new(Pipelines {
                blend,
                box_pass,
                detail_target,
            }),
        }
    }

    /// Detail's target over `plan.active`, in a new buffer: the box blur of
    /// the result and of the source (three horizontal and vertical passes
    /// each, over their read rects) and then blurred result + source -
    /// blurred source. Both images are read from copies, never from the
    /// buffer the blend pass writes.
    fn detail_target(
        &self,
        plan: &Plan,
        (out_rect, out): (Rect, &[u16]),
        (region, src): (Rect, &[u16]),
        spp: usize,
    ) -> wgpu::Buffer {
        let (device, queue) = (&self.device, &self.queue);
        let image = |samples: &[u16], from: Rect, read: Rect, label: &str| {
            upload_storage(device, queue, &gather(samples, from, read, spp), label)
        };
        let scratch = |read: Rect| {
            let bytes = u64::from(read.width) * u64::from(read.height) * spp as u64 * 4;
            create_storage_buffer(device, bytes, "focusforge-gpu-detail-scratch")
        };
        let (result_read, source_read) = (plan.result_read, plan.source_read);
        let result = image(out, out_rect, result_read, "focusforge-gpu-detail-result");
        let source = image(src, region, source_read, "focusforge-gpu-detail-source");
        let result_scratch = scratch(result_read);
        let (source_a, source_b) = (scratch(source_read), scratch(source_read));
        // Six passes, H V H V H V, each reading the buffer the last wrote:
        // the result ends where it began, the source in `source_b`.
        self.blur(
            [
                &result,
                &result_scratch,
                &result,
                &result_scratch,
                &result,
                &result_scratch,
                &result,
            ],
            (result_read, spp, plan.pass),
        );
        self.blur(
            [
                &source, &source_a, &source_b, &source_a, &source_b, &source_a, &source_b,
            ],
            (source_read, spp, plan.pass),
        );
        let active = plan.active;
        let samples = u64::from(active.width) * u64::from(active.height) * spp as u64;
        let target = create_storage_buffer(device, samples * 4, "focusforge-gpu-detail-target");
        let params = upload_params(device, queue, &target_params_bytes(plan, spp));
        run_pass_2d(
            device,
            queue,
            &self.pipelines.detail_target,
            &[
                entry(3, &result),
                entry(4, &source),
                entry(5, &source_b),
                entry(6, &target),
                entry(7, &params),
            ],
            active.width,
            active.height,
            "focusforge-gpu-detail-target",
        );
        target
    }

    /// Runs the six box passes of `chain`: pass `i` reads `chain[i]` and
    /// writes `chain[i + 1]`, horizontal when `i` is even.
    fn blur(&self, chain: [&wgpu::Buffer; 7], (read, spp, p): (Rect, usize, u32)) {
        let (device, queue) = (&self.device, &self.queue);
        let params = [true, false].map(|horizontal| {
            upload_params(device, queue, &box_params_bytes(read, spp, p, horizontal))
        });
        for i in 0..6 {
            let lines = if i % 2 == 0 { read.height } else { read.width };
            run_pass_2d(
                device,
                queue,
                &self.pipelines.box_pass,
                &[
                    entry(0, chain[i]),
                    entry(1, chain[i + 1]),
                    entry(2, &params[i % 2]),
                ],
                lines,
                spp as u32,
                "focusforge-gpu-detail-box",
            );
        }
    }

    /// [`blend::apply`] on the device: blends `op`'s source samples `src` (of
    /// `region`) over `out` (of `out_rect`), exactly as the CPU blend does.
    /// [`BlendError::Refused`] covers coordinates of 2^14 or more in absolute
    /// value, a mask radius of 2^16 sixteenths of a pixel or more, a region
    /// (with a Detail stroke's read margin) past one storage-buffer binding
    /// at one `u32` a sample, and more than 65537 samples a pixel. All four
    /// modes blend at any valid opacity. On any `Err`, `out` is as it was.
    #[allow(clippy::too_many_arguments)]
    pub fn apply(
        &self,
        op: &Op,
        info: ZMapInfo,
        out_rect: Rect,
        out: &mut [u16],
        region: Rect,
        src: &[u16],
        spp: usize,
    ) -> Result<(), BlendError> {
        let limit = self.device.limits().max_storage_buffer_binding_size;
        let Some(plan) = plan(op, info, out_rect, out.len(), region, src.len(), spp, limit)? else {
            return Ok(());
        };
        let active = plan.active;
        let out_bytes = gather(out, out_rect, active, spp);
        let segment_bytes: Vec<u8> = plan
            .segments
            .iter()
            .flatten()
            .flat_map(|v| v.to_le_bytes())
            .collect();

        let (device, queue) = (&self.device, &self.queue);
        let out_buffer = with_validation_scope(device, || {
            let out_buffer = upload_storage(device, queue, &out_bytes, "focusforge-gpu-blend-out");
            // Detail mixes toward its own target; the other modes toward the
            // source frame.
            let src_buffer = if plan.mode == BrushMode::Detail {
                self.detail_target(&plan, (out_rect, &*out), (region, src), spp)
            } else {
                let bytes = gather(src, region, active, spp);
                upload_storage(device, queue, &bytes, "focusforge-gpu-blend-src")
            };
            let segment_buffer = upload_storage(
                device,
                queue,
                &segment_bytes,
                "focusforge-gpu-blend-segments",
            );
            let params = upload_params(device, queue, &params_bytes(&plan, spp));
            run_pass_2d(
                device,
                queue,
                &self.pipelines.blend,
                &[
                    entry(0, &out_buffer),
                    entry(1, &src_buffer),
                    entry(2, &segment_buffer),
                    entry(3, &params),
                ],
                active.width,
                active.height,
                "focusforge-gpu-blend",
            );
            out_buffer
        })?;
        let blended = download_buffer(device, queue, &out_buffer, out_bytes.len() as u64)?;

        let row_len = active.width as usize * spp;
        let mut words = blended.chunks_exact(4);
        for y in active.y..active.y + active.height {
            let at = ((y - out_rect.y) as usize * out_rect.width as usize
                + (active.x - out_rect.x) as usize)
                * spp;
            for (sample, word) in out[at..at + row_len].iter_mut().zip(&mut words) {
                *sample =
                    u32::from_le_bytes(word.try_into().expect("chunks_exact(4) yields 4 bytes"))
                        as u16;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use focusforge_core::project::{BrushMode, Point, StrokeSource};
    use focusforge_core::zmap::ZMapInfo;

    fn pt(x: i32, y: i32) -> Point {
        Point { x, y }
    }

    fn brush(path: Vec<Point>, radius: f32, softness: f32, mode: BrushMode, opacity: f32) -> Op {
        Op::BrushStroke {
            source: StrokeSource::Frame(0),
            path,
            radius,
            softness,
            mode,
            opacity,
        }
    }

    fn stroke(path: Vec<Point>, radius: f32, softness: f32) -> Op {
        brush(path, radius, softness, BrushMode::Normal, 1.0)
    }

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn info() -> ZMapInfo {
        ZMapInfo::new(20_000, 20_000, 4).unwrap()
    }

    const BIG: u64 = u64::MAX;

    fn plan_over(op: &Op, region: Rect, spp: usize, max: u64) -> Result<Option<Plan>, BlendError> {
        plan_in(op, region, region, spp, max)
    }

    fn refused(result: Result<Option<Plan>, BlendError>) -> &'static str {
        match result {
            Err(BlendError::Refused { limit, .. }) => limit,
            other => panic!("expected a refusal, got {:?}", other.map(|p| p.is_some())),
        }
    }

    fn styled(path: Vec<Point>, mode: BrushMode, opacity: f32) -> Op {
        brush(path, 4.0, 0.5, mode, opacity)
    }

    #[test]
    fn modes_and_opacities_plan_without_refusal() {
        let region = rect(0, 0, 16, 16);
        let clear = rect(100, 100, 16, 16);
        for mode in ALL_MODES {
            for opacity in [1.0, 0.5, 0.99, 1e-9, 1.0 - f32::EPSILON / 2.0] {
                let op = styled(vec![pt(8, 8)], mode, opacity);
                let plan = plan_over(&op, region, 1, BIG).unwrap().unwrap();
                assert_eq!(
                    (plan.mode, plan.q16),
                    (mode, blend::opacity_q16(opacity)),
                    "{mode:?} at {opacity}"
                );
                assert!(plan_over(&op, clear, 1, BIG).unwrap().is_none());
            }
        }
    }

    const ALL_MODES: [BrushMode; 4] = [
        BrushMode::Normal,
        BrushMode::Lighten,
        BrushMode::Darken,
        BrushMode::Detail,
    ];

    #[test]
    fn an_invalid_opacity_is_the_core_error_in_every_mode() {
        let region = rect(0, 0, 16, 16);
        for mode in ALL_MODES {
            for opacity in [0.0, -0.5, 1.5, f32::NAN] {
                let op = styled(vec![pt(8, 8)], mode, opacity);
                assert!(
                    matches!(
                        plan_over(&op, region, 1, BIG),
                        Err(BlendError::Retouch(RetouchError::InvalidOpacity { .. }))
                    ),
                    "{mode:?} at {opacity}"
                );
            }
        }
    }

    fn detail(path: Vec<Point>) -> Op {
        styled_radius(path, BrushMode::Detail, 12.0)
    }

    fn styled_radius(path: Vec<Point>, mode: BrushMode, radius: f32) -> Op {
        brush(path, radius, 0.5, mode, 1.0)
    }

    fn plan_in(
        op: &Op,
        out_rect: Rect,
        region: Rect,
        spp: usize,
        max: u64,
    ) -> Result<Option<Plan>, BlendError> {
        let samples = |r: Rect| r.width as usize * r.height as usize * spp;
        plan(
            op,
            info(),
            out_rect,
            samples(out_rect),
            region,
            samples(region),
            spp,
            max,
        )
    }

    #[test]
    fn detail_plan_gathers_the_margin_inside_both_buffers() {
        // Radius 12 gives pass radius 4 and read margin 12 around the 25 x 25
        // footprint at (88, 88).
        let op = detail(vec![pt(100, 100)]);
        let out_rect = rect(80, 60, 300, 300);
        let region = rect(80, 82, 300, 200);
        let plan = plan_in(&op, out_rect, region, 3, BIG).unwrap().unwrap();
        assert_eq!(plan.active, rect(88, 88, 25, 25));
        assert_eq!(
            plan.result_read,
            rect(80, 76, 45, 49),
            "clipped by out_rect on the left"
        );
        assert_eq!(
            plan.source_read,
            rect(80, 82, 45, 43),
            "clipped by region above"
        );

        let edge = detail(vec![pt(5, 5)]);
        let whole = rect(0, 0, 200, 200);
        let plan = plan_in(&edge, whole, whole, 1, BIG).unwrap().unwrap();
        assert_eq!(plan.active, rect(0, 0, 18, 18));
        assert_eq!(
            plan.result_read,
            rect(0, 0, 30, 30),
            "clipped by the map at 0"
        );
        assert_eq!(plan.source_read, plan.result_read);

        let normal = styled_radius(vec![pt(100, 100)], BrushMode::Normal, 12.0);
        let plan = plan_in(&normal, out_rect, region, 3, BIG).unwrap().unwrap();
        assert_eq!(
            (plan.result_read, plan.source_read),
            (plan.active, plan.active)
        );
    }

    #[test]
    fn a_detail_margin_reaching_two_to_the_fourteenth_is_refused_where_normal_is_not() {
        let region = rect(16_000, 0, 500, 300);
        let at = vec![pt(16_365, 100)];
        let normal = styled_radius(at.clone(), BrushMode::Normal, 12.0);
        assert!(plan_over(&normal, region, 1, BIG).unwrap().is_some());
        assert_eq!(
            refused(plan_over(&detail(at), region, 1, BIG)),
            "coordinate"
        );
        // The read rect of a stroke at x reaches x + 25; 2^14 is the first refused end.
        let last_ok = detail(vec![pt(16_359, 100)]);
        assert!(plan_over(&last_ok, region, 1, BIG).unwrap().is_some());
        let first_past = detail(vec![pt(16_360, 100)]);
        assert_eq!(
            refused(plan_over(&first_past, region, 1, BIG)),
            "coordinate"
        );
    }

    #[test]
    fn a_detail_margin_past_the_binding_is_refused_where_normal_is_not() {
        let region = rect(0, 0, 400, 400);
        let at = vec![pt(200, 200)];
        // Active 25 x 25 samples, read rect 49 x 49, four bytes each.
        let (active, read) = (25 * 25 * 4, 49 * 49 * 4);
        let normal = styled_radius(at.clone(), BrushMode::Normal, 12.0);
        assert!(plan_over(&normal, region, 1, active).unwrap().is_some());
        let op = detail(at);
        assert_eq!(refused(plan_over(&op, region, 1, active)), "buffer");
        assert_eq!(refused(plan_over(&op, region, 1, read - 1)), "buffer");
        assert!(plan_over(&op, region, 1, read).unwrap().is_some());
    }

    #[test]
    fn samples_past_the_channel_sum_bound_are_refused_by_name() {
        let region = rect(0, 0, 16, 16);
        let op = stroke(vec![pt(8, 8)], 4.0, 0.5);
        // 65537 samples of 65535 sum to u32::MAX.
        assert!(plan_over(&op, region, 65_537, BIG).unwrap().is_some());
        assert_eq!(refused(plan_over(&op, region, 65_538, BIG)), "samples");
    }

    #[test]
    fn radius_is_refused_from_4096_pixels() {
        let region = rect(0, 0, 16, 16);
        let ok = stroke(vec![pt(8, 8)], 4095.9, 0.5);
        assert!(plan_over(&ok, region, 1, BIG).unwrap().is_some());
        let past = stroke(vec![pt(8, 8)], 4096.0, 0.5);
        assert_eq!(refused(plan_over(&past, region, 1, BIG)), "radius");
    }

    #[test]
    fn coordinates_are_refused_from_two_to_the_fourteenth() {
        let region = rect(0, 0, 16, 16);
        for (x, ok) in [
            (16_383, true),
            (16_384, false),
            (-16_383, true),
            (-16_384, false),
            (i32::MIN, false),
            (i32::MAX, false),
        ] {
            let op = stroke(vec![pt(8, 8), pt(x, 8)], 20_000.0 / 16.0, 0.5);
            let result = plan_over(&op, region, 1, BIG);
            assert_eq!(result.is_ok(), ok, "endpoint {x}");
            if !ok {
                assert_eq!(refused(result), "coordinate");
            }
        }
        let far = rect(16_380, 0, 8, 4);
        let op = stroke(vec![pt(16_383, 2)], 4.0, 0.5);
        assert_eq!(refused(plan_over(&op, far, 1, BIG)), "coordinate");
        let edge = rect(16_380, 0, 4, 4);
        assert!(plan_over(&op, edge, 1, BIG).unwrap().is_some());
    }

    #[test]
    fn a_segment_out_of_range_but_clear_of_the_region_is_culled_not_refused() {
        let region = rect(0, 0, 32, 32);
        let op = stroke(
            vec![pt(10, 10), pt(20, 12), pt(20, 400), pt(30_000, 400)],
            3.0,
            0.5,
        );
        let plan = plan_over(&op, region, 1, BIG).unwrap().unwrap();
        assert_eq!(plan.segments, vec![[10, 10, 20, 12], [20, 12, 20, 400]]);
    }

    #[test]
    fn segments_keep_only_what_can_reach_the_region() {
        let region = rect(100, 100, 20, 20);
        let op = stroke(
            vec![
                pt(0, 0),
                pt(50, 0),
                pt(110, 90),
                pt(110, 130),
                pt(500, 130),
                pt(500, 0),
                pt(900, 0),
            ],
            12.0,
            0.5,
        );
        let plan = plan_over(&op, region, 1, BIG).unwrap().unwrap();
        // A segment stays when its bounding box, widened by the radius and a
        // pixel, meets the region.
        assert_eq!(
            plan.segments,
            vec![[50, 0, 110, 90], [110, 90, 110, 130], [110, 130, 500, 130]]
        );
        assert_eq!(plan.active, rect(100, 100, 20, 20));
    }

    #[test]
    fn a_stroke_clear_of_the_region_plans_nothing() {
        let op = stroke(vec![pt(500, 500)], 4.0, 0.5);
        assert!(
            plan_over(&op, rect(0, 0, 16, 16), 1, BIG)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_active_rect_is_the_footprint_inside_the_region() {
        let op = stroke(vec![pt(20, 20)], 4.0, 0.5);
        let plan = plan_over(&op, rect(10, 10, 100, 100), 3, BIG)
            .unwrap()
            .unwrap();
        assert_eq!(plan.active, rect(16, 16, 9, 9));
        assert_eq!(plan.segments, vec![[20, 20, 20, 20]]);
        let (radius, core) = focusforge_core::retouch::blend::mask_ramp(&op, info()).unwrap();
        assert_eq!(
            (u64::from(plan.radius), u64::from(plan.core)),
            (radius, core)
        );
    }

    #[test]
    fn a_buffer_past_the_binding_limit_is_refused_by_name() {
        let op = stroke(vec![pt(20, 20)], 4.0, 0.5);
        let region = rect(10, 10, 100, 100);
        // The active rect is 9 x 9 pixels, three samples each, four bytes a sample.
        let need = 9 * 9 * 3 * 4;
        assert!(plan_over(&op, region, 3, need).unwrap().is_some());
        assert_eq!(refused(plan_over(&op, region, 3, need - 1)), "buffer");
    }

    #[test]
    fn core_errors_pass_through_unchanged() {
        let region = rect(0, 0, 16, 16);
        let empty = stroke(vec![], 4.0, 0.5);
        assert!(matches!(
            plan_over(&empty, region, 1, BIG),
            Err(BlendError::Retouch(RetouchError::EmptyPath))
        ));
        let op = stroke(vec![pt(8, 8)], 4.0, 0.5);
        let outside = rect(19_990, 0, 16, 16);
        assert!(matches!(
            plan_over(&op, outside, 1, BIG),
            Err(BlendError::Retouch(RetouchError::TileOutsideMap { .. }))
        ));
        let len = 16 * 16;
        assert!(matches!(
            plan(&op, info(), region, len, region, len - 1, 1, BIG),
            Err(BlendError::Retouch(RetouchError::TileBufferSize { .. }))
        ));
    }

    /// The kernel's arithmetic written out in the kernel's own `u32` and
    /// `i32` steps. Debug builds panic on an overflow the shader would wrap,
    /// so a pass shows every intermediate fits.
    mod kernel {
        const SATURATED: u32 = u32::MAX;

        fn point_d2_256(dx: i32, dy: i32) -> u32 {
            let s = (dx * dx + dy * dy) as u32;
            if s >= 1 << 24 { SATURATED } else { s << 8 }
        }

        fn square64(c: u32) -> (u32, u32) {
            let (ch, cl) = (c >> 16, c & 0xffff);
            let mid = 2 * ch * cl;
            let lo = cl * cl;
            let low = lo.wrapping_add((mid & 0xffff) << 16);
            let carry = u32::from(low < lo);
            (ch * ch + (mid >> 16) + carry, low)
        }

        fn projected_d2_256(cross: u32, len2: u32) -> u32 {
            let (high, low) = square64(cross);
            let (mut r, mut q) = (0u32, 0u32);
            for i in (0..64).rev() {
                let word = if i >= 32 { high } else { low };
                r = (r << 1) | ((word >> (i & 31)) & 1);
                q <<= 1;
                if r >= len2 {
                    r -= len2;
                    q |= 1;
                }
            }
            if q >= 1 << 24 {
                return SATURATED;
            }
            let mut f = 0u32;
            for _ in 0..8 {
                r <<= 1;
                f <<= 1;
                if r >= len2 {
                    r -= len2;
                    f |= 1;
                }
            }
            (q << 8) | f
        }

        pub fn segment_d2_256(px: i32, py: i32, s: [i32; 4]) -> u32 {
            let (dx, dy) = (s[2] - s[0], s[3] - s[1]);
            let (ux, uy) = (px - s[0], py - s[1]);
            let len2 = dx * dx + dy * dy;
            let dot = ux * dx + uy * dy;
            if len2 == 0 || dot <= 0 {
                return point_d2_256(ux, uy);
            }
            if dot >= len2 {
                return point_d2_256(px - s[2], py - s[3]);
            }
            projected_d2_256((ux * dy - uy * dx).unsigned_abs(), len2 as u32)
        }

        pub fn isqrt(value: u32) -> u32 {
            let (mut n, mut res, mut bit) = (value, 0u32, 1u32 << 30);
            while bit > n {
                bit >>= 2;
            }
            while bit != 0 {
                if n >= res + bit {
                    n -= res + bit;
                    res = (res >> 1) + bit;
                } else {
                    res >>= 1;
                }
                bit >>= 2;
            }
            res
        }

        pub fn alpha(radius: u32, core: u32, d16: u32) -> u32 {
            if d16 >= radius {
                0
            } else if d16 <= core {
                1 << 16
            } else {
                ((radius - d16) << 16) / (radius - core)
            }
        }

        /// The mask alpha scaled by the Q16 opacity `q`.
        pub fn scaled_alpha(alpha: u32, q: u32) -> u32 {
            if q == 1 << 16 {
                alpha
            } else {
                (alpha * q + 32768) >> 16
            }
        }

        pub fn channel_sum(pixel: &[u32]) -> u32 {
            pixel.iter().sum()
        }

        /// One of Detail's box passes over a `width x height` buffer of `spp`
        /// samples a pixel, as the kernel's sliding window runs it: the window
        /// of radius `p` repeats the buffer's edge sample past the edge, and
        /// the window sum stays below 2^28.
        pub fn box_pass(
            src: &[u32],
            (width, height): (u32, u32),
            spp: usize,
            p: u32,
            horizontal: bool,
        ) -> Vec<u32> {
            let (lines, n) = if horizontal {
                (height, width as i32)
            } else {
                (width, height as i32)
            };
            let pixel = |i: u32, line: u32| {
                if horizontal {
                    line * width + i
                } else {
                    i * width + line
                }
            };
            let at = |a: i32, line: u32, c: usize| {
                src[pixel(a.clamp(0, n - 1) as u32, line) as usize * spp + c]
            };
            let p = p as i32;
            let mut dst = vec![0u32; src.len()];
            for line in 0..lines {
                for c in 0..spp {
                    let mut sum = 0u32;
                    for j in -p..=p {
                        sum += at(j, line, c);
                    }
                    for k in 0..n {
                        dst[pixel(k as u32, line) as usize * spp + c] =
                            (sum + p as u32) / (2 * p as u32 + 1);
                        sum = sum + at(k + p + 1, line, c) - at(k - p, line, c);
                    }
                }
            }
            dst
        }

        /// Detail's target sample from the blurred result, the source and
        /// the blurred source.
        pub fn detail_target(blurred_result: u32, source: u32, blurred_source: u32) -> u32 {
            (blurred_result as i32 + source as i32 - blurred_source as i32).clamp(0, 65_535) as u32
        }

        /// The kernel's per-pixel tail; `alpha` is scaled and not zero.
        pub fn mix_pixel(mode: u32, out: &mut [u32], src: &[u32], alpha: u32) {
            let acts = match mode {
                1 => channel_sum(src) > channel_sum(out),
                2 => channel_sum(src) < channel_sum(out),
                _ => true,
            };
            if acts {
                for (o, &s) in out.iter_mut().zip(src) {
                    *o = (*o * ((1 << 16) - alpha) + s * alpha + 32768) >> 16;
                }
            }
        }
    }

    #[test]
    fn the_kernel_arithmetic_gives_the_alpha_of_the_cpu_twin() {
        let info = info();
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |bound: i64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) as i64) % bound
        };
        for case in 0..60_000 {
            // Half the cases span the whole coordinate range, half sit close
            // enough for the ramp to be crossed.
            let span: i64 = if case % 2 == 0 { 16_383 } else { 40 };
            let centre = (next(8_000), next(8_000));
            let count = 1 + next(4);
            let mut path = Vec::new();
            for _ in 0..count {
                path.push(pt(
                    (centre.0 + next(2 * span + 1) - span) as i32,
                    (centre.1 + next(2 * span + 1) - span) as i32,
                ));
            }
            let radius = [1.5f32, 12.0, 40.0, 700.5, 4095.9][next(5) as usize];
            let softness = [0.0f32, 0.3, 0.5, 1.0][next(4) as usize];
            let op = stroke(path.clone(), radius, softness);
            let (r16, c16) = focusforge_core::retouch::blend::mask_ramp(&op, info).unwrap();
            let (x, y) = (
                (centre.0 + next(2 * span + 1) - span).clamp(0, 16_383),
                (centre.1 + next(2 * span + 1) - span).clamp(0, 16_383),
            );
            let segments: Vec<[i32; 4]> = if path.len() == 1 {
                vec![[path[0].x, path[0].y, path[0].x, path[0].y]]
            } else {
                path.windows(2)
                    .map(|w| [w[0].x, w[0].y, w[1].x, w[1].y])
                    .collect()
            };
            let d2 = segments
                .iter()
                .map(|&s| kernel::segment_d2_256(x as i32, y as i32, s))
                .min()
                .unwrap();
            let got = kernel::alpha(r16 as u32, c16 as u32, kernel::isqrt(d2));
            let want =
                focusforge_core::retouch::blend::alpha(&op, info, x as u32, y as u32).unwrap();
            assert_eq!(
                got, want,
                "case {case}: {path:?} r {radius} s {softness} at ({x}, {y})"
            );
        }
    }

    #[test]
    fn the_kernel_isqrt_is_the_integer_square_root() {
        for n in (0..=u32::MAX)
            .step_by(65_537)
            .chain([0, 1, 3, 4, 15, 16, u32::MAX])
        {
            assert_eq!(u64::from(kernel::isqrt(n)), u64::from(n).isqrt(), "{n}");
        }
        for r in [1u32, 255, 256, 46_340, 46_341, 65_535] {
            let sq = r * r;
            assert_eq!(kernel::isqrt(sq), r);
            assert_eq!(kernel::isqrt(sq - 1), r - 1);
        }
    }

    fn widen(pixel: &[u16]) -> Vec<u32> {
        pixel.iter().map(|&s| u32::from(s)).collect()
    }

    /// Every pixel of `spp` samples drawn from `values`.
    fn all_pixels(values: &[u16], spp: usize) -> Vec<Vec<u16>> {
        let mut pixels = vec![vec![]];
        for _ in 0..spp {
            pixels = pixels
                .into_iter()
                .flat_map(|p| {
                    values.iter().map(move |&v| {
                        let mut next = p.clone();
                        next.push(v);
                        next
                    })
                })
                .collect();
        }
        pixels
    }

    #[test]
    fn the_kernel_mix_is_the_cpu_blend_per_mode_at_the_extremes() {
        let sets: [(&[u16], usize); 4] = [
            (&[0, 1, 32767, 32768, 65534, 65535], 1),
            (&[0, 1, 32768, 65535], 3),
            (&[0, 65534, 65535], 4),
            (&[0, 65535], 2),
        ];
        for mode in [BrushMode::Normal, BrushMode::Lighten, BrushMode::Darken] {
            for (values, spp) in sets {
                let pixels = all_pixels(values, spp);
                for alpha in [0u32, 1, 32768, 65535, 65536] {
                    for q in [1u32, 32768, 65535, 65536] {
                        let scaled = kernel::scaled_alpha(alpha, q);
                        let cpu_scaled = ((u64::from(alpha) * u64::from(q) + 32768) >> 16) as u32;
                        assert_eq!(scaled, cpu_scaled, "alpha {alpha} q {q}");
                        if scaled == 0 {
                            continue;
                        }
                        for out in &pixels {
                            for src in &pixels {
                                let mut want = out.clone();
                                blend::blend_pixel(mode, &mut want, src, scaled);
                                let mut got = widen(out);
                                kernel::mix_pixel(mode_code(mode), &mut got, &widen(src), scaled);
                                assert_eq!(
                                    got,
                                    widen(&want),
                                    "{mode:?} out {out:?} src {src:?} alpha {alpha} q {q}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_channel_sum_of_the_largest_pixel_fits_u32() {
        assert_eq!(kernel::channel_sum(&vec![65_535; 65_537]), u32::MAX);
        assert_eq!(kernel::channel_sum(&[65_535; 4]), 4 * 65_535);
    }

    #[test]
    fn the_kernel_blend_is_the_cpu_apply_across_opacity_and_softness() {
        let (side, spp) = (24u32, 3usize);
        let info = ZMapInfo::new(side, side, 4).unwrap();
        let region = rect(0, 0, side, side);
        let path = vec![pt(6, 8), pt(17, 14), pt(12, 20)];
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            match (state >> 33) % 8 {
                0 => 0,
                1 => 65_535,
                _ => (state >> 17) as u16,
            }
        };
        let base: Vec<u16> = (0..side as usize * side as usize * spp)
            .map(|_| next())
            .collect();
        let src: Vec<u16> = (0..side as usize * side as usize * spp)
            .map(|_| next())
            .collect();
        let opacities = [1.0, 1.0 - f32::EPSILON / 2.0, 0.9999, 0.5, 0.01, 1e-5, 1e-9];
        let softnesses = [0.0, 0.25, 0.5, 0.75, 1.0];
        let segments: Vec<[i32; 4]> = path
            .windows(2)
            .map(|w| [w[0].x, w[0].y, w[1].x, w[1].y])
            .collect();
        let mut changed = 0;
        for mode in [BrushMode::Normal, BrushMode::Lighten, BrushMode::Darken] {
            for opacity in opacities {
                for softness in softnesses {
                    let op = Op::BrushStroke {
                        source: StrokeSource::Frame(0),
                        path: path.clone(),
                        radius: 7.0,
                        softness,
                        mode,
                        opacity,
                    };
                    let mut want = base.clone();
                    blend::apply(&op, info, region, &mut want, region, &src, spp).unwrap();
                    changed += want.iter().zip(&base).filter(|(w, b)| w != b).count();

                    let (radius, core) = blend::mask_ramp(&op, info).unwrap();
                    let q = blend::opacity_q16(opacity);
                    let mut got = widen(&base);
                    for y in 0..side as i32 {
                        for x in 0..side as i32 {
                            let d2 = segments
                                .iter()
                                .map(|&s| kernel::segment_d2_256(x, y, s))
                                .min()
                                .unwrap();
                            let alpha =
                                kernel::alpha(radius as u32, core as u32, kernel::isqrt(d2));
                            let alpha = kernel::scaled_alpha(alpha, q);
                            if alpha == 0 {
                                continue;
                            }
                            let at = (y as usize * side as usize + x as usize) * spp;
                            kernel::mix_pixel(
                                mode_code(mode),
                                &mut got[at..at + spp],
                                &widen(&src[at..at + spp]),
                                alpha,
                            );
                        }
                    }
                    assert_eq!(
                        got,
                        widen(&want),
                        "{mode:?} at {opacity}, softness {softness}"
                    );
                }
            }
        }
        assert!(changed > 0, "the comparison saw changed samples");
    }

    /// `samples` of `from` cropped to `to` (inside `from`), widened to `u32`.
    fn crop(samples: &[u16], from: Rect, to: Rect, spp: usize) -> Vec<u32> {
        let mut out = Vec::new();
        for y in to.y..to.y + to.height {
            let at = ((y - from.y) as usize * from.width as usize + (to.x - from.x) as usize) * spp;
            out.extend(
                samples[at..at + to.width as usize * spp]
                    .iter()
                    .map(|&s| u32::from(s)),
            );
        }
        out
    }

    /// Detail's target over `active` as the device computes it: six whole-rect
    /// box passes over each image's read rect, then the target.
    fn mirrored_detail(
        p: u32,
        info: ZMapInfo,
        (result_rect, result): (Rect, &[u16]),
        (source_rect, source): (Rect, &[u16]),
        active: Rect,
        spp: usize,
    ) -> Vec<u16> {
        let reach = blend::grow(active, 3 * p, info);
        let result_read = reach.intersect(result_rect).unwrap();
        let source_read = reach.intersect(source_rect).unwrap();
        let blur = |samples: &[u16], from: Rect, read: Rect| {
            let mut buf = crop(samples, from, read, spp);
            for pass in 0..6 {
                buf = kernel::box_pass(&buf, (read.width, read.height), spp, p, pass % 2 == 0);
            }
            buf
        };
        let (blurred_result, blurred_source) = (
            blur(result, result_rect, result_read),
            blur(source, source_rect, source_read),
        );
        let source_crop = crop(source, source_rect, source_read, spp);
        let mut target = Vec::new();
        for y in active.y..active.y + active.height {
            for x in active.x..active.x + active.width {
                for c in 0..spp {
                    let at = |read: Rect| {
                        ((y - read.y) as usize * read.width as usize + (x - read.x) as usize) * spp
                            + c
                    };
                    target.push(kernel::detail_target(
                        blurred_result[at(result_read)],
                        source_crop[at(source_read)],
                        blurred_source[at(source_read)],
                    ) as u16);
                }
            }
        }
        target
    }

    #[test]
    fn the_kernel_detail_target_is_the_cpu_detail_output() {
        let info = ZMapInfo::new(200, 160, 4).unwrap();
        let whole = rect(0, 0, 200, 160);
        let mut state = 0x1234_5678_9abc_def1_u64;
        let mut sample = move || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            match (state >> 33) % 6 {
                0 => 0,
                1 => 65_535,
                _ => (state >> 17) as u16,
            }
        };
        // (result rect, source rect, active): clipped by the buffers on some
        // sides and not on others, and by the map at its corner.
        let cases = [
            (whole, whole, rect(80, 60, 20, 15)),
            (
                rect(50, 30, 120, 100),
                rect(60, 40, 130, 110),
                rect(80, 60, 20, 15),
            ),
            (
                rect(60, 40, 100, 80),
                rect(70, 30, 120, 110),
                rect(80, 60, 20, 15),
            ),
            (whole, rect(0, 0, 150, 130), rect(0, 0, 25, 18)),
            (rect(100, 90, 100, 70), whole, rect(160, 130, 40, 30)),
        ];
        let mut changed = 0;
        for p in [1u32, 4, 13] {
            for spp in [1usize, 3, 4] {
                for (result_rect, source_rect, active) in cases {
                    let mut buffer = |rect: Rect| -> Vec<u16> {
                        (0..rect.width as usize * rect.height as usize * spp)
                            .map(|_| sample())
                            .collect()
                    };
                    let (result, source) = (buffer(result_rect), buffer(source_rect));
                    let mut want = vec![0u16; active.width as usize * active.height as usize * spp];
                    blend::detail_output(
                        p,
                        (result_rect, &result),
                        (source_rect, &source),
                        active,
                        spp,
                        &mut want,
                    );
                    let got = mirrored_detail(
                        p,
                        info,
                        (result_rect, &result),
                        (source_rect, &source),
                        active,
                        spp,
                    );
                    assert_eq!(
                        got, want,
                        "p {p} spp {spp} {result_rect:?} {source_rect:?} {active:?}"
                    );
                    let before: Vec<u16> = crop(&result, result_rect, active, spp)
                        .into_iter()
                        .map(|s| s as u16)
                        .collect();
                    changed += usize::from(want != before);
                }
            }
        }
        assert!(changed > 0, "the target differs from the result somewhere");
    }
}
