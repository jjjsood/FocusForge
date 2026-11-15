// SPDX-License-Identifier: GPL-3.0-or-later

//! The brush-preview kernel against `focusforge_core::retouch::preview::
//! brush_preview`, over `fixtures/mantis-512`: the golden composite as the
//! result, a frame as the source, decimated onto lattices of other steps and
//! origins. Integer throughout on both sides, so the expected worst |delta| is
//! exactly 0, and the alpha byte must be equal.

use std::num::NonZeroU32;

use focusforge_core::io::TiffStack;
use focusforge_core::project::{BrushMode, Point};
use focusforge_core::retouch::preview::{
    BRUSH_PREVIEW_STRENGTH_Q16, BrushPreviewGrid, BrushPreviewSpec, brush_preview,
};
use focusforge_core::tile::Rect;
use focusforge_gpu::device::{SharedDevice, adapter_report, shared_device_or_skip};
use focusforge_gpu::display::preview::{BrushPreviewRenderer, plan};
use focusforge_gpu::display::{DisplayTexture, read_display_texture};

use crate::common::{self, frame_samples, worst_delta};

const SIZE: u32 = 512;
const SRC_FRAME: usize = 6;
const MODES: [BrushMode; 4] = [
    BrushMode::Normal,
    BrushMode::Lighten,
    BrushMode::Darken,
    BrushMode::Detail,
];
/// The displayed strength and the test-only full strength.
const STRENGTHS: [u32; 2] = [BRUSH_PREVIEW_STRENGTH_Q16, 65536];

fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// A region of a full image on a lattice of `step`.
struct Lattice {
    rect: Rect,
    step: u32,
    samples: Vec<u16>,
}

impl Lattice {
    fn decimate(image: &[u16], rect: Rect, step: u32) -> Self {
        let mut samples = Vec::new();
        for j in 0..rect.height.div_ceil(step) {
            for i in 0..rect.width.div_ceil(step) {
                let at = (((rect.y + j * step) * SIZE + rect.x + i * step) * 3) as usize;
                samples.extend_from_slice(&image[at..at + 3]);
            }
        }
        Self {
            rect,
            step,
            samples,
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

/// What one device-versus-twin comparison found.
#[derive(Default)]
struct Tally {
    cases: usize,
    drawn: usize,
    worst: i32,
    worst_alpha: i32,
}

/// The device layer of `brush` over the two lattices, or `None` where the
/// plan is empty; compared with the twin's layer on the way.
fn compare(
    shared: &SharedDevice,
    renderer: &mut BrushPreviewRenderer,
    (result, source): (&Lattice, &Lattice),
    brush: &BrushPreviewSpec,
    strength: u32,
    tally: &mut Tally,
) -> Option<Vec<u8>> {
    let label = format!("{brush:?} strength {strength}");
    let max = shared.device().limits().max_storage_buffer_binding_size;
    let planned = plan(
        (SIZE, SIZE),
        &result.grid(),
        &source.grid(),
        brush,
        strength,
        max,
    )
    .unwrap_or_else(|e| panic!("{label}: {e}"));
    let twin = brush_preview(
        (SIZE, SIZE),
        &result.grid(),
        &source.grid(),
        brush,
        strength,
    );
    tally.cases += 1;
    let (planned, twin) = match (planned, twin) {
        (None, None) => return None,
        (Some(planned), Some(twin)) => (planned, twin),
        (planned, twin) => panic!(
            "{label}: plan {} and the twin {}",
            planned.is_some(),
            twin.is_some()
        ),
    };
    let layout = planned.layout;
    assert_eq!(
        (layout.column, layout.row, layout.width, layout.height),
        (twin.column, twin.row, twin.width, twin.height),
        "{label}"
    );
    let target = DisplayTexture::new(shared.device(), layout.width, layout.height);
    renderer.render(
        shared.device(),
        shared.queue(),
        &target,
        &planned,
        &result.grid(),
        &source.grid(),
    );
    let gpu = read_display_texture(shared.device(), shared.queue(), &target);
    assert_eq!(gpu.len(), twin.texels.len(), "{label}: the texture's size");
    let (worst, at) = worst_delta(&gpu, &twin.texels);
    let alpha = |texels: &[u8]| {
        texels
            .iter()
            .skip(3)
            .step_by(4)
            .copied()
            .collect::<Vec<_>>()
    };
    let (worst_alpha, _) = worst_delta(&alpha(&gpu), &alpha(&twin.texels));
    if worst != 0 {
        println!("{label}: worst |delta| {worst} at byte {at}");
    }
    tally.drawn += 1;
    tally.worst = tally.worst.max(worst);
    tally.worst_alpha = tally.worst_alpha.max(worst_alpha);
    Some(gpu)
}

#[test]
fn preview_layer_matches_cpu_twin() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    let stack = common::open_stack();
    let full = rect(0, 0, SIZE, SIZE);
    let native = stack.header(0).samples_per_pixel as usize;
    assert_eq!(native, 3, "the fixture frames hold three samples a pixel");
    let source_image = frame_samples(&stack, SRC_FRAME, full, 3, native);
    let golden = TiffStack::open_files(&[common::mantis_512_dir()
        .join("golden")
        .join("laplacian.tif")])
    .expect("open the golden composite");
    let mut result_image = vec![0u16; (SIZE * SIZE * 3) as usize];
    golden
        .read_tile_into(0, full, &mut result_image)
        .expect("read the golden composite");

    // Result lattice and source lattice: the same, an origin off the 4-grid
    // with a source on another origin, a coarser result with a finer source,
    // and a source on another step.
    let setups: Vec<(Lattice, Lattice)> = [
        ((full, 1), (full, 1)),
        ((rect(3, 5, 505, 503), 1), (rect(1, 2, 510, 508), 1)),
        ((rect(6, 10, 500, 496), 2), (full, 1)),
        ((full, 2), (rect(2, 2, 508, 508), 3)),
    ]
    .map(|((r_rect, r_step), (s_rect, s_step))| {
        (
            Lattice::decimate(&result_image, r_rect, r_step),
            Lattice::decimate(&source_image, s_rect, s_step),
        )
    })
    .into();

    let mut renderer = BrushPreviewRenderer::new(shared.device());
    let mut tally = Tally::default();
    // Inside the image and offset from the grid's origin, then across its edge.
    for centre in [(250, 260), (4, 5)] {
        for (result, source) in &setups {
            for mode in MODES {
                let mut brushes = Vec::new();
                for opacity in [1.0, 0.5, 0.01] {
                    for radius in [1.5, 12.0, 40.0] {
                        brushes.push(spec(centre, radius, 0.5, mode, opacity));
                    }
                }
                for softness in [0.0, 1.0] {
                    brushes.push(spec(centre, 12.0, softness, mode, 1.0));
                }
                for brush in &brushes {
                    for strength in STRENGTHS {
                        compare(
                            shared,
                            &mut renderer,
                            (result, source),
                            brush,
                            strength,
                            &mut tally,
                        );
                    }
                }
            }
        }
    }

    // Where the twin has no layer, so has the plan: a 1-sample result and a
    // step-4 disc between lattice samples.
    let (result, source) = &setups[0];
    let grey = BrushPreviewGrid {
        samples_per_pixel: 1,
        samples: &result.samples[..(SIZE * SIZE) as usize],
        ..result.grid()
    };
    let ask = |result: &BrushPreviewGrid, brush: &BrushPreviewSpec| {
        let planned =
            plan((SIZE, SIZE), result, &source.grid(), brush, 52429, u64::MAX).expect("no refusal");
        let twin = brush_preview((SIZE, SIZE), result, &source.grid(), brush, 52429);
        (planned.is_some(), twin.is_some())
    };
    assert_eq!(
        ask(&grey, &spec((250, 260), 12.0, 0.5, BrushMode::Normal, 1.0)),
        (false, false)
    );
    let sparse = Lattice::decimate(&result_image, full, 4);
    let between = spec((2, 2), 1.5, 0.5, BrushMode::Normal, 1.0);
    assert_eq!(ask(&sparse.grid(), &between), (false, false));
    tally.cases += 2;

    // The parity is not vacuous: each mode and each opacity below 1 changes
    // the layer against Normal at full opacity.
    let (result, source) = &setups[0];
    let mut at = |mode, opacity| {
        compare(
            shared,
            &mut renderer,
            (result, source),
            &spec((250, 260), 12.0, 0.5, mode, opacity),
            65536,
            &mut tally,
        )
        .expect("a layer")
    };
    let normal = at(BrushMode::Normal, 1.0);
    for mode in [BrushMode::Lighten, BrushMode::Darken, BrushMode::Detail] {
        assert_ne!(at(mode, 1.0), normal, "{mode:?} against Normal");
    }
    for opacity in [0.5, 0.01] {
        assert_ne!(at(BrushMode::Normal, opacity), normal, "opacity {opacity}");
    }

    println!(
        "brush preview parity: {} cases, {} drawn, worst |delta|={}, worst alpha delta={}",
        tally.cases, tally.drawn, tally.worst, tally.worst_alpha
    );
    println!("{}", adapter_report(shared.info()));
    assert!(tally.drawn > 0, "no case drew a layer");
    assert!(
        tally.worst <= 1,
        "worst |delta| {} above 1 LSB",
        tally.worst
    );
    assert_eq!(tally.worst_alpha, 0, "the alpha byte is exact");
}
