// SPDX-License-Identifier: GPL-3.0-or-later

//! The WGSL twin of [`focusforge_core::retouch::blend::apply`]: one soft brush
//! stroke blended over a region in the same integer fixed point. WGSL has
//! 32-bit integers, so the kernel emulates the CPU's wide mask inside the
//! bounded domain [`Blender::apply`] states and refuses outside it.

use std::sync::Arc;

use focusforge_core::project::{Op, Point};
use focusforge_core::retouch::{RetouchError, blend};
use focusforge_core::tile::Rect;
use focusforge_core::zmap::ZMapInfo;

use crate::device::GpuError;
use crate::errors::with_validation_scope;
use crate::pipeline::{
    Pipeline, build_pipeline, run_pass_2d, storage_entry, uniform_entry, upload_params,
};
use crate::transfer::{download_buffer, upload_storage};

const WGSL_SRC: &str = include_str!("blend.wgsl");

/// Coordinates the kernel accepts lie strictly between `-COORD_LIMIT` and
/// `COORD_LIMIT`.
const COORD_LIMIT: i32 = 1 << 14;
/// The mask radius, in sixteenths of a pixel, the kernel refuses from.
const RADIUS_LIMIT_16: u64 = 1 << 16;

/// Why [`Blender::apply`] did not blend.
#[derive(Debug, thiserror::Error)]
pub enum BlendError {
    /// The stroke or the buffers are invalid, as for the CPU blend.
    #[error(transparent)]
    Retouch(#[from] RetouchError),

    /// The stroke lies outside what the kernel computes exactly; the CPU
    /// blend handles it. `limit` names the bound: `coordinate`, `radius` or
    /// `buffer`.
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
    let limit = COORD_LIMIT as u32;
    if active.x + active.width > limit || active.y + active.height > limit {
        return Err(refused(
            "coordinate",
            format!("of the region {active:?} reaches {limit}"),
        ));
    }
    let buffer_bytes = u64::from(active.width) * u64::from(active.height) * spp as u64 * 4;
    if buffer_bytes > max_binding {
        return Err(refused(
            "buffer",
            format!(
                "of {buffer_bytes} bytes exceeds max_storage_buffer_binding_size={max_binding}"
            ),
        ));
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

/// The kernel's uniform: eight little-endian words.
fn params_bytes(plan: &Plan, spp: usize) -> [u8; 32] {
    let words = [
        plan.active.x,
        plan.active.y,
        plan.active.width,
        plan.active.height,
        spp as u32,
        plan.radius,
        plan.core,
        plan.segments.len() as u32,
    ];
    let mut bytes = [0u8; 32];
    for (chunk, word) in bytes.chunks_exact_mut(4).zip(words) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    bytes
}

/// The blend kernel bound to a device. Built once on the device it runs on --
/// Slint's in the GUI, the shared compute device in the parity test -- and
/// cheap to clone.
#[derive(Clone)]
pub struct Blender {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: Arc<Pipeline>,
}

impl Blender {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let pipeline = build_pipeline(
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
        Self {
            device: device.clone(),
            queue: queue.clone(),
            pipeline: Arc::new(pipeline),
        }
    }

    /// [`blend::apply`] on the device: blends `op`'s source samples `src` (of
    /// `region`) over `out` (of `out_rect`), exactly as the CPU blend does.
    /// [`BlendError::Refused`] covers coordinates of 2^14 or more in absolute
    /// value, a mask radius of 2^16 sixteenths of a pixel or more, and a region
    /// past one storage-buffer binding at one `u32` a sample. On any `Err`,
    /// `out` is as it was.
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
        let src_bytes = gather(src, region, active, spp);
        let segment_bytes: Vec<u8> = plan
            .segments
            .iter()
            .flatten()
            .flat_map(|v| v.to_le_bytes())
            .collect();

        let (device, queue) = (&self.device, &self.queue);
        let out_buffer = with_validation_scope(device, || {
            let out_buffer = upload_storage(device, queue, &out_bytes, "focusforge-gpu-blend-out");
            let src_buffer = upload_storage(device, queue, &src_bytes, "focusforge-gpu-blend-src");
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
                &self.pipeline,
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
    use focusforge_core::project::Point;
    use focusforge_core::zmap::ZMapInfo;

    fn pt(x: i32, y: i32) -> Point {
        Point { x, y }
    }

    fn stroke(path: Vec<Point>, radius: f32, softness: f32) -> Op {
        Op::BrushStroke {
            source_id: 0,
            path,
            radius,
            softness,
        }
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
        let len = region.width as usize * region.height as usize * spp;
        plan(op, info(), region, len, region, len, spp, max)
    }

    fn refused(result: Result<Option<Plan>, BlendError>) -> &'static str {
        match result {
            Err(BlendError::Refused { limit, .. }) => limit,
            other => panic!("expected a refusal, got {:?}", other.map(|p| p.is_some())),
        }
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
}
