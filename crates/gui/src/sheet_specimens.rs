// SPDX-License-Identifier: GPL-3.0-or-later

//! Specimen images for the theme sheet, read from the `mantis-512` fixture.

use std::path::Path;

/// The sheet's brush preview specimens, built by the preview's CPU twin over a
/// tile of the `mantis-512` fixture: the result image under
/// `golden/`, source frame `07`. Each is a backdrop of the result's own display and the layer of
/// a one-point stroke over it, both placed from the stroke's centre at
/// `scale` screen pixels per image pixel.
pub fn preview_specimens() -> Result<Vec<crate::theme_sheet::PreviewSpecimen>, String> {
    use crate::theme_sheet::PreviewSpecimen;
    use focusforge_core::io::TiffStack;
    use focusforge_core::project::{BrushMode, Point};
    use focusforge_core::retouch::preview::{
        BRUSH_PREVIEW_STRENGTH_Q16, BrushPreview, BrushPreviewGrid, BrushPreviewSpec, brush_preview,
    };
    use focusforge_core::tile::Rect;

    const IMAGE: (u32, u32) = (512, 512);
    const TILE: Rect = Rect {
        x: 272,
        y: 10,
        width: 240,
        height: 200,
    };
    const CENTRE: Point = Point { x: 400, y: 110 };
    const OPAQUE_Q16: u32 = 1 << 16;
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/mantis-512");
    let read = |path: &str| -> Result<Vec<u16>, String> {
        let stack =
            TiffStack::open_staged(&fixtures.join(path)).map_err(|e| format!("{path}: {e}"))?;
        let mut samples = vec![0u16; TILE.width as usize * TILE.height as usize * 3];
        stack
            .read_tile_into(0, TILE, &mut samples)
            .map_err(|e| format!("{path}: {e}"))?;
        Ok(samples)
    };
    let golden = fixtures.join("golden");
    let result_name = std::fs::read_dir(&golden)
        .map_err(|e| format!("{}: {e}", golden.display()))?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.ends_with(".tif") && !name.contains("zmap"))
        .min()
        .ok_or_else(|| format!("{}: no result image", golden.display()))?;
    let result = read(&format!("golden/{result_name}"))?;
    let source = read("frames/07.tif")?;
    fn grid(samples: &[u16]) -> BrushPreviewGrid<'_> {
        BrushPreviewGrid {
            rect: TILE,
            step: std::num::NonZeroU32::MIN,
            samples_per_pixel: 3,
            samples,
        }
    }
    let (result_grid, source_grid) = (grid(&result), grid(&source));
    let image = |layer: &BrushPreview, opaque: bool| {
        let mut pixels =
            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(layer.width, layer.height);
        for (px, t) in pixels
            .make_mut_slice()
            .iter_mut()
            .zip(layer.texels.chunks_exact(4))
        {
            *px = slint::Rgba8Pixel {
                r: t[0],
                g: t[1],
                b: t[2],
                a: if opaque { 255 } else { t[3] },
            };
        }
        slint::Image::from_rgba8(pixels)
    };
    // `(x, y, w, h)` of a layer's pixel box relative to the stroke centre's
    // pixel centre, in screen pixels.
    let rect = |layer: &BrushPreview, scale: f32| {
        let at = |origin: u32, lattice: u32, centre: i32| {
            (origin + lattice) as f32 - centre as f32 - 0.5
        };
        (
            at(TILE.x, layer.column, CENTRE.x) * scale,
            at(TILE.y, layer.row, CENTRE.y) * scale,
            layer.width as f32 * scale,
            layer.height as f32 * scale,
        )
    };
    let specimen = |scale: f32, backdrop_radius: f32, radius: f32, mode, opacity| {
        let spec = |radius, softness, mode, opacity| BrushPreviewSpec {
            centre: CENTRE,
            radius,
            softness,
            mode,
            opacity,
        };
        let backdrop = brush_preview(
            IMAGE,
            &result_grid,
            &result_grid,
            &spec(backdrop_radius, 0.0, BrushMode::Normal, 1.0),
            OPAQUE_Q16,
        )
        .ok_or("backdrop preview")?;
        let layer = brush_preview(
            IMAGE,
            &result_grid,
            &source_grid,
            &spec(radius, 0.55, mode, opacity),
            BRUSH_PREVIEW_STRENGTH_Q16,
        )
        .ok_or("stroke preview")?;
        let (bx, by, bw, bh) = rect(&backdrop, scale);
        let (lx, ly, lw, lh) = rect(&layer, scale);
        Ok::<_, String>(PreviewSpecimen {
            backdrop: image(&backdrop, true),
            backdrop_x: bx,
            backdrop_y: by,
            backdrop_width: bw,
            backdrop_height: bh,
            layer: image(&layer, false),
            layer_x: lx,
            layer_y: ly,
            layer_width: lw,
            layer_height: lh,
        })
    };
    Ok(vec![
        specimen(1.0, 98.0, 64.0, BrushMode::Normal, 1.0)?,
        specimen(3.0, 30.0, 64.0 / 3.0, BrushMode::Normal, 1.0)?,
        specimen(1.0, 98.0, 64.0, BrushMode::Lighten, 0.6)?,
    ])
}
