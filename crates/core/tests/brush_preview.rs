// SPDX-License-Identifier: GPL-3.0-or-later

//! The brush-preview layer: alpha from core's mask through the stated integer
//! formula, RGB from the mode's output, the nearest-sample source read, the
//! dither-aligned output box and the empty answers.

mod common;

use std::num::NonZeroU32;

use focusforge_core::display::{reduce16to8, tonemap_cpu};
use focusforge_core::project::{BrushMode, Op, Point, StrokeSource};
use focusforge_core::retouch::blend;
use focusforge_core::retouch::preview::{
    BRUSH_PREVIEW_STRENGTH_Q16, BrushPreview, BrushPreviewGrid, BrushPreviewSpec, brush_preview,
};
use focusforge_core::tile::Rect;
use focusforge_core::zmap::ZMapInfo;

const OPAQUE: u32 = 65536;
const MODES: [BrushMode; 4] = [
    BrushMode::Normal,
    BrushMode::Lighten,
    BrushMode::Darken,
    BrushMode::Detail,
];

fn info(width: u32, height: u32) -> ZMapInfo {
    ZMapInfo::new(width, height, 1).unwrap()
}

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

/// The one-point stroke a real brush press at the spec's centre records.
fn stroke_of(spec: &BrushPreviewSpec) -> Op {
    Op::BrushStroke {
        source: StrokeSource::Frame(0),
        path: vec![spec.centre],
        radius: spec.radius,
        softness: spec.softness,
        mode: spec.mode,
        opacity: spec.opacity,
    }
}

/// Deterministic 16-bit samples that differ in channel sum from seed to seed.
fn noise(seed: u32, len: usize) -> Vec<u16> {
    let mut s = seed.wrapping_mul(2_654_435_761).max(1);
    (0..len)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s >> 8) as u16
        })
        .collect()
}

/// A 3-sample grid and the buffer behind it.
struct Owned {
    rect: Rect,
    step: u32,
    samples: Vec<u16>,
}

impl Owned {
    fn new(rect: Rect, step: u32, seed: u32) -> Self {
        let len = (rect.width.div_ceil(step) * rect.height.div_ceil(step) * 3) as usize;
        Self {
            rect,
            step,
            samples: noise(seed, len),
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

    fn columns(&self) -> u32 {
        self.rect.width.div_ceil(self.step)
    }

    fn rows(&self) -> u32 {
        self.rect.height.div_ceil(self.step)
    }

    /// The image pixel of lattice sample `(i, j)`.
    fn pixel(&self, i: u32, j: u32) -> (u32, u32) {
        (self.rect.x + i * self.step, self.rect.y + j * self.step)
    }

    fn at(&self, i: u32, j: u32) -> &[u16] {
        let at = ((j * self.columns() + i) * 3) as usize;
        &self.samples[at..at + 3]
    }

    /// The whole grid through `tonemap_cpu`, RGBA8 row-major.
    fn display(&self) -> Vec<u8> {
        display_of(&self.samples, self.columns(), self.rows())
    }
}

fn display_of(samples: &[u16], columns: u32, rows: u32) -> Vec<u8> {
    let mut out = vec![0u8; (columns * rows * 4) as usize];
    tonemap_cpu(samples, columns, rows, 3, 0, &mut out);
    out
}

/// Every texel of `layer` as (lattice column, lattice row, RGBA).
fn texels(layer: &BrushPreview) -> Vec<(u32, u32, [u8; 4])> {
    assert_eq!(
        layer.texels.len(),
        (layer.width * layer.height * 4) as usize
    );
    let mut all = Vec::new();
    for r in 0..layer.height {
        for c in 0..layer.width {
            let at = ((r * layer.width + c) * 4) as usize;
            let t = &layer.texels[at..at + 4];
            all.push((layer.column + c, layer.row + r, [t[0], t[1], t[2], t[3]]));
        }
    }
    all
}

fn make(
    image: (u32, u32),
    result: &Owned,
    source: &Owned,
    spec: &BrushPreviewSpec,
    strength: u32,
) -> Option<BrushPreview> {
    brush_preview(image, &result.grid(), &source.grid(), spec, strength)
}

#[test]
fn alpha_follows_core_mask() {
    let map = info(100, 100);
    let rnd16 = |x: u64| (x + 32768) >> 16;
    for step in [1u32, 3] {
        let area = rect(5, 7, 80, 70);
        let result = Owned::new(area, step, 1);
        let source = Owned::new(area, step, 2);
        for softness in [0.0f32, 0.5, 1.0] {
            for opacity in [1.0f32, 0.5, 0.01] {
                for mode in MODES {
                    for strength in [BRUSH_PREVIEW_STRENGTH_Q16, OPAQUE] {
                        let label = format!(
                            "step {step} softness {softness} opacity {opacity} {mode:?} strength {strength}"
                        );
                        let s = spec((36, 36), 12.0, softness, mode, opacity);
                        let op = stroke_of(&s);
                        let layer = make((100, 100), &result, &source, &s, strength)
                            .unwrap_or_else(|| panic!("{label}: a layer"));
                        let o = u64::from(blend::opacity_q16(opacity));
                        let mut full = 0;
                        for (i, j, texel) in texels(&layer) {
                            let (x, y) = result.pixel(i, j);
                            let m = blend::alpha(&op, map, x, y).unwrap();
                            let a16 = rnd16(rnd16(u64::from(m) * o) * u64::from(strength));
                            let a8 = ((a16 * 255 + 32768) >> 16) as u8;
                            assert_eq!(texel[3], a8, "{label} at ({x}, {y})");
                            let (dx, dy) = (f64::from(x) - 36.0, f64::from(y) - 36.0);
                            if dx.hypot(dy) > 12.0 {
                                assert_eq!(texel[3], 0, "{label}: outside the radius");
                            }
                            if m == OPAQUE && opacity == 1.0 && strength == OPAQUE {
                                assert_eq!(texel[3], 255, "{label}: full mask is opaque");
                                full += 1;
                            }
                        }
                        if softness == 0.0 && opacity == 1.0 && strength == OPAQUE {
                            assert!(full > 0, "{label}: a sample at full mask");
                        }
                    }
                }
            }
        }
    }
    assert_eq!(BRUSH_PREVIEW_STRENGTH_Q16, 52429);
    assert_eq!(
        include_str!("../src/retouch/preview.rs")
            .matches("52429")
            .count(),
        1,
        "the strength constant is written once"
    );
}

/// Detail's target over the lattice box of the footprint, from the public
/// helper on whole-grid buffers: its box in lattice indices and its samples.
fn detail_target(
    result: &Owned,
    source: &Owned,
    op: &Op,
    map: ZMapInfo,
    step: u32,
) -> (Rect, Vec<u16>) {
    let fp = blend::footprint(op, map).unwrap().unwrap();
    let p = blend::pass_radius(op, map).unwrap();
    let p_lattice = ((p + step / 2) / step).max(1);
    let range = |from: u32, len: u32| (from.div_ceil(step), (from + len).div_ceil(step));
    let (x0, x1) = range(fp.x, fp.width);
    let (y0, y1) = range(fp.y, fp.height);
    let at = rect(x0, y0, x1 - x0, y1 - y0);
    let whole = rect(0, 0, result.columns(), result.rows());
    let mut out = vec![0u16; (at.width * at.height * 3) as usize];
    blend::detail_output(
        p_lattice,
        (whole, &result.samples),
        (whole, &source.samples),
        at,
        3,
        &mut out,
    );
    (at, out)
}

#[test]
fn rgb_is_mode_output_per_mode() {
    let map = info(64, 64);
    for step in [1u32, 2] {
        let result = Owned::new(rect(0, 0, 64, 64), step, 1);
        let source = Owned::new(rect(0, 0, 64, 64), step, 2);
        let columns = result.columns();
        for mode in MODES {
            let label = format!("step {step} {mode:?}");
            let s = spec((31, 29), 10.0, 0.5, mode, 0.7);
            let op = stroke_of(&s);
            let layer = make((64, 64), &result, &source, &s, OPAQUE)
                .unwrap_or_else(|| panic!("{label}: a layer"));
            let detail = (mode == BrushMode::Detail)
                .then(|| detail_target(&result, &source, &op, map, step));
            let mut expected = result.samples.clone();
            let (mut acts, mut skips) = (0, 0);
            for j in 0..result.rows() {
                for i in 0..columns {
                    let (x, y) = result.pixel(i, j);
                    if blend::alpha(&op, map, x, y).unwrap() == 0 {
                        continue;
                    }
                    let at = ((j * columns + i) * 3) as usize;
                    let src: &[u16] = match &detail {
                        Some((b, target)) => {
                            let k = (((j - b.y) * b.width + (i - b.x)) * 3) as usize;
                            &target[k..k + 3]
                        }
                        None => source.at(i, j),
                    };
                    if blend::mode_acts(mode, src, &result.samples[at..at + 3]) {
                        acts += 1;
                    } else {
                        skips += 1;
                    }
                    blend::blend_pixel(mode, &mut expected[at..at + 3], src, OPAQUE);
                }
            }
            if matches!(mode, BrushMode::Lighten | BrushMode::Darken) {
                assert!(acts > 0 && skips > 0, "{label}: both picks occur");
            }
            let display = display_of(&expected, columns, result.rows());
            for (i, j, texel) in texels(&layer) {
                let at = ((j * columns + i) * 4) as usize;
                assert_eq!(&texel[..3], &display[at..at + 3], "{label} at ({i}, {j})");
            }
        }
    }
}

/// The index the stated rule picks: `min((offset + (step - 1) / 2) / step, count - 1)`.
fn nearest_index(offset: u32, step: u32, count: u32) -> u32 {
    ((offset + (step - 1) / 2) / step).min(count - 1)
}

#[test]
fn source_is_read_by_nearest_sample() {
    // Each sample holds its own lattice index.
    let indexed = |area: Rect, step: u32| -> Vec<u16> {
        let (columns, rows) = (area.width.div_ceil(step), area.height.div_ceil(step));
        (0..rows)
            .flat_map(|j| (0..columns).flat_map(move |i| [i as u16, j as u16, 0]))
            .collect()
    };
    // Per offset from the rect origin, the index listed by hand from the rule.
    let step4 = [0u16, 0, 0, 1, 1, 1, 1, 2, 2, 2];
    let step2 = [0u16, 0, 1, 1, 2, 2, 3, 3, 4];
    for (step, width, expected) in [(4u32, 10u32, &step4[..]), (2, 9, &step2[..])] {
        let area = rect(3, 2, width, width);
        let samples = indexed(area, step);
        let grid = BrushPreviewGrid {
            rect: area,
            step: NonZeroU32::new(step).unwrap(),
            samples_per_pixel: 3,
            samples: &samples,
        };
        for (offset, &want) in expected.iter().enumerate() {
            let offset = offset as u32;
            let got = grid.nearest(3 + offset, 2 + offset).expect("inside");
            assert_eq!(
                got[..2],
                [want, want],
                "step {step} offset {offset} (ties go to the lower index)"
            );
        }
        assert!(grid.nearest(2, 2).is_none(), "left of the rect");
        assert!(grid.nearest(3 + width, 2).is_none(), "right of the rect");
        assert!(grid.nearest(3, 2 + width).is_none(), "below the rect");
    }

    // Through the layer: a source with its own origin, step and size.
    let map = info(40, 40);
    let result = Owned::new(rect(0, 0, 40, 40), 1, 1);
    let source = Owned::new(rect(5, 6, 30, 28), 4, 2);
    let s = spec((20, 20), 8.0, 0.5, BrushMode::Normal, 1.0);
    let op = stroke_of(&s);
    let layer = make((40, 40), &result, &source, &s, OPAQUE).expect("the source covers the disc");
    let mut read = 0;
    for (x, y, texel) in texels(&layer) {
        if blend::alpha(&op, map, x, y).unwrap() == 0 {
            continue;
        }
        let si = nearest_index(x - 5, 4, source.columns());
        let sj = nearest_index(y - 6, 4, source.rows());
        for (c, &sample) in source.at(si, sj).iter().enumerate() {
            let want = reduce16to8(sample, x, y);
            assert_eq!(texel[c], want, "pixel ({x}, {y}) channel {c}");
        }
        read += 1;
    }
    assert!(read > 100, "the disc was read");

    let off_source = spec((8, 20), 8.0, 0.5, BrushMode::Normal, 1.0);
    assert!(
        make((40, 40), &result, &source, &off_source, OPAQUE).is_none(),
        "a disc texel whose source coordinate is outside the source rect"
    );
}

#[test]
fn nearest_sample_clamps_at_the_last_column_and_row() {
    // When the extent is a multiple of the step, the last offsets round up to
    // one past the last sample. Per offset, the index listed by hand.
    let cases: [(u32, u32, &[u16]); 3] = [
        (4, 8, &[0, 0, 0, 1, 1, 1, 1, 1]),
        (4, 12, &[0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 2]),
        (3, 9, &[0, 0, 1, 1, 1, 2, 2, 2, 2]),
    ];
    for (step, extent, expected) in cases {
        let grid = |area: Rect| {
            let columns = area.width.div_ceil(step);
            let rows = area.height.div_ceil(step);
            let samples: Vec<u16> = (0..rows)
                .flat_map(|j| (0..columns).flat_map(move |i| [i as u16, j as u16, 0]))
                .collect();
            (area, samples)
        };

        // Along x in the middle row: an unclamped column would read the next
        // row's first sample.
        let (area, samples) = grid(rect(3, 2, extent, 3 * step));
        let across = BrushPreviewGrid {
            rect: area,
            step: NonZeroU32::new(step).unwrap(),
            samples_per_pixel: 3,
            samples: &samples,
        };
        for (offset, &want) in expected.iter().enumerate() {
            let got = across.nearest(3 + offset as u32, 2 + step).expect("inside");
            assert_eq!(
                got[..2],
                [want, 1],
                "step {step} extent {extent} column offset {offset}"
            );
        }

        // Along y in the middle column, down to the last row.
        let (area, samples) = grid(rect(3, 2, 3 * step, extent));
        let down = BrushPreviewGrid {
            rect: area,
            step: NonZeroU32::new(step).unwrap(),
            samples_per_pixel: 3,
            samples: &samples,
        };
        for (offset, &want) in expected.iter().enumerate() {
            let got = down.nearest(3 + step, 2 + offset as u32).expect("inside");
            assert_eq!(
                got[..2],
                [1, want],
                "step {step} extent {extent} row offset {offset}"
            );
        }
    }
}

#[test]
fn painted_normal_stroke_matches_preview() {
    let dir = common::mantis_512_dir();
    let (width, height, spp, result) = common::read_tiff(&dir.join("golden/laplacian.tif"));
    let (frame_width, frame_height, frame_spp, frame) =
        common::read_tiff(&dir.join("frames/00.tif"));
    assert_eq!(
        (width, height, spp),
        (512, 512, 3),
        "the fixture is a 512 x 512 RGB image"
    );
    assert_eq!((frame_width, frame_height, frame_spp), (512, 512, 3));
    let area = rect(0, 0, width, height);
    let map = info(width, height);
    let s = spec((200, 180), 30.0, 0.5, BrushMode::Normal, 1.0);
    let op = stroke_of(&s);

    let mut painted = result.clone();
    blend::apply::<u16>(&op, map, area, &mut painted, area, &frame, 3).unwrap();
    let painted = display_of(&painted, width, height);
    let before = display_of(&result, width, height);

    let result = Owned {
        rect: area,
        step: 1,
        samples: result,
    };
    let source = Owned {
        rect: area,
        step: 1,
        samples: frame,
    };
    let layer = make((width, height), &result, &source, &s, OPAQUE).expect("a layer");
    let (mut full, mut soft) = (0, 0);
    for (x, y, texel) in texels(&layer) {
        if texel[3] == 0 {
            continue;
        }
        let at = ((y * width + x) * 4) as usize;
        let a = u32::from(texel[3]);
        let full_mask = blend::alpha(&op, map, x, y).unwrap() == OPAQUE;
        for c in 0..3 {
            let shown = i32::from(painted[at + c]);
            let preview = i32::from(texel[c]);
            let mixed = ((u32::from(before[at + c]) * (255 - a) + u32::from(texel[c]) * a + 127)
                / 255) as i32;
            assert!(
                (shown - mixed).abs() <= 2,
                "({x}, {y}) channel {c}: painted {shown}, mix {mixed}"
            );
            if full_mask {
                assert!(
                    (shown - preview).abs() <= 1,
                    "({x}, {y}) channel {c}: painted {shown}, preview {preview}"
                );
            }
        }
        if full_mask {
            full += 1;
        } else {
            soft += 1;
        }
    }
    assert!(full > 100 && soft > 100, "full {full}, soft {soft}");
}

#[test]
fn output_on_dither_grid() {
    let map = info(80, 80);
    for step in [1u32, 2] {
        for (ox, oy) in [(0u32, 0u32), (3, 5), (5, 3)] {
            let label = format!("step {step} origin ({ox}, {oy})");
            let area = rect(ox, oy, 70, 66);
            let result = Owned::new(area, step, 1);
            let source = Owned::new(area, step, 2);
            let s = spec((30, 32), 9.0, 0.5, BrushMode::Normal, 1.0);
            let op = stroke_of(&s);
            let layer = make((80, 80), &result, &source, &s, OPAQUE)
                .unwrap_or_else(|| panic!("{label}: a layer"));
            assert_eq!((layer.column % 4, layer.row % 4), (0, 0), "{label}");

            let fp = blend::footprint(&op, map).unwrap().unwrap();
            let first = |from: u32, origin: u32| (from - origin).div_ceil(step);
            let (i0, i1) = (first(fp.x, ox), first(fp.x + fp.width, ox));
            let (j0, j1) = (first(fp.y, oy), first(fp.y + fp.height, oy));
            assert!(
                layer.column <= i0 && layer.column + layer.width >= i1,
                "{label}: columns"
            );
            assert!(
                layer.row <= j0 && layer.row + layer.height >= j1,
                "{label}: rows"
            );

            let display = result.display();
            let mut clear = 0;
            for (i, j, texel) in texels(&layer) {
                if texel[3] == 0 {
                    let at = ((j * result.columns() + i) * 4) as usize;
                    assert_eq!(&texel[..3], &display[at..at + 3], "{label} at ({i}, {j})");
                    clear += 1;
                }
            }
            assert!(clear > 0, "{label}: the box holds texels outside the disc");
        }
    }
}

#[test]
fn edge_and_bad_inputs_are_empty() {
    let full = rect(0, 0, 40, 40);
    let result = Owned::new(full, 1, 1);
    let source = Owned::new(full, 1, 2);
    let (kept_result, kept_source) = (result.samples.clone(), source.samples.clone());
    let normal =
        |centre: (i32, i32), radius: f32| spec(centre, radius, 0.5, BrushMode::Normal, 1.0);
    let call = |s: &BrushPreviewSpec| make((40, 40), &result, &source, s, OPAQUE);

    let edge = call(&normal((2, 2), 10.0)).expect("the in-image part of the disc");
    assert_eq!(
        (edge.column, edge.row, edge.width, edge.height),
        (0, 0, 13, 13),
        "texels only for the in-image part"
    );
    assert!(call(&normal((200, 200), 10.0)).is_none(), "wholly outside");
    assert!(call(&normal((-50, -50), 10.0)).is_none(), "wholly outside");

    let sparse = Owned::new(full, 4, 1);
    let sparse_source = Owned::new(full, 4, 2);
    assert!(
        make(
            (40, 40),
            &sparse,
            &sparse_source,
            &normal((2, 2), 0.02),
            OPAQUE
        )
        .is_none(),
        "a disc between two samples at step 4"
    );

    let at = (20, 20);
    for (what, bad) in [
        ("radius NaN", normal(at, f32::NAN)),
        ("radius 0", normal(at, 0.0)),
        ("softness 1.5", spec(at, 8.0, 1.5, BrushMode::Normal, 1.0)),
        (
            "softness NaN",
            spec(at, 8.0, f32::NAN, BrushMode::Normal, 1.0),
        ),
        (
            "opacity NaN",
            spec(at, 8.0, 0.5, BrushMode::Normal, f32::NAN),
        ),
        ("opacity 0", spec(at, 8.0, 0.5, BrushMode::Normal, 0.0)),
        ("opacity 1.5", spec(at, 8.0, 0.5, BrushMode::Normal, 1.5)),
    ] {
        assert!(call(&bad).is_none(), "{what}");
    }
    let ok = normal(at, 8.0);
    assert!(call(&ok).is_some());
    assert!(
        make((40, 40), &result, &source, &ok, OPAQUE + 1).is_none(),
        "a strength above 1.0"
    );

    let short = BrushPreviewGrid {
        samples: &result.samples[..result.samples.len() - 3],
        ..result.grid()
    };
    assert!(
        brush_preview((40, 40), &short, &source.grid(), &ok, OPAQUE).is_none(),
        "result buffer one pixel short"
    );
    let short = BrushPreviewGrid {
        samples: &source.samples[3..],
        ..source.grid()
    };
    assert!(
        brush_preview((40, 40), &result.grid(), &short, &ok, OPAQUE).is_none(),
        "source buffer one pixel short"
    );
    let grey = BrushPreviewGrid {
        samples_per_pixel: 1,
        samples: &result.samples[..1600],
        ..result.grid()
    };
    assert!(
        brush_preview((40, 40), &grey, &source.grid(), &ok, OPAQUE).is_none(),
        "grey result"
    );
    assert!(
        brush_preview((40, 40), &result.grid(), &grey, &ok, OPAQUE).is_none(),
        "grey source"
    );

    // Inputs that do not cover the disc.
    let part = |area: Rect| Owned::new(area, 1, 3);
    let near = part(rect(10, 10, 10, 10));
    assert!(
        make((40, 40), &near, &source, &normal((30, 30), 5.0), OPAQUE).is_none(),
        "result grid short of the disc"
    );
    assert!(
        make((40, 40), &result, &near, &normal((30, 30), 5.0), OPAQUE).is_none(),
        "source grid short of the disc"
    );

    // Detail reads 3p past the disc, clamped to the map and not to the grid.
    let detail = |centre: (i32, i32)| spec(centre, 6.0, 0.5, BrushMode::Detail, 1.0);
    let covers_disc = part(rect(10, 10, 20, 20));
    assert!(
        make(
            (40, 40),
            &covers_disc,
            &source,
            &normal((20, 20), 6.0),
            OPAQUE
        )
        .is_some(),
        "the grid covers the disc for a mode with no margin"
    );
    assert!(
        make((40, 40), &covers_disc, &source, &detail((20, 20)), OPAQUE).is_none(),
        "the result grid stops short of Detail's margin"
    );
    assert!(
        make((40, 40), &result, &covers_disc, &detail((20, 20)), OPAQUE).is_none(),
        "the source grid stops short of Detail's margin"
    );
    assert!(
        call(&detail((3, 3))).is_some(),
        "Detail at the image corner"
    );
    let to_edge = part(rect(0, 0, 16, 16));
    assert!(
        make((40, 40), &to_edge, &to_edge, &detail((3, 3)), OPAQUE).is_some(),
        "the margin clamps to the map, so a grid reaching the edge answers"
    );
    let short_of_margin = part(rect(0, 0, 15, 16));
    assert!(
        make(
            (40, 40),
            &short_of_margin,
            &to_edge,
            &detail((3, 3)),
            OPAQUE
        )
        .is_none(),
        "a grid one column short of the clamped margin"
    );

    assert_eq!(result.samples, kept_result);
    assert_eq!(source.samples, kept_source);
}
