// SPDX-License-Identifier: GPL-3.0-or-later

//! The overlay blend: one colour per pixel, from
//! [`overlay_colors`](crate::retouch::overlay_colors), blended over the
//! composite at an adjustable opacity. Integer throughout, the same reduction
//! [`crate::display::tonemap_cpu`] uses, so `crates/gpu`'s WGSL twin agrees bit-exactly.

use rayon::prelude::*;

use super::reduce16to8;

/// `opacity` (`0.0..=1.0`) as the integer alpha [`overlay_cpu`] blends
/// with: `0` leaves the composite untouched, `255` is the table colour
/// alone.
pub fn alpha_from_opacity(opacity: f32) -> u8 {
    (opacity * 255.0).round() as u8
}

/// Blends each pixel's overlay colour in `colors` over `composite` at
/// `alpha`, in `tonemap_cpu`'s RGBA8 buffer-local-coordinate shape.
/// `composite.len()` must be `width * height * 3`, `colors.len()` and
/// `out.len() / 4` must be `width * height`.
pub fn overlay_cpu(
    composite: &[u16],
    colors: &[[u8; 3]],
    width: u32,
    height: u32,
    alpha: u8,
    out: &mut [u8],
) {
    debug_assert_eq!(composite.len(), width as usize * height as usize * 3);
    debug_assert_eq!(colors.len(), width as usize * height as usize);
    debug_assert_eq!(out.len(), width as usize * height as usize * 4);
    let alpha = u32::from(alpha);
    out.par_chunks_exact_mut(4)
        .enumerate()
        .for_each(|(i, pixel)| {
            let x = (i as u32) % width;
            let y = (i as u32) / width;
            let base_index = i * 3;
            let over = colors[i];
            for c in 0..3 {
                let base = u32::from(reduce16to8(composite[base_index + c], x, y));
                let blended = (base * (255 - alpha) + u32::from(over[c]) * alpha + 127) / 255;
                pixel[c] = blended as u8;
            }
            pixel[3] = 255;
        });
}

#[cfg(test)]
#[test]
fn opacity_endpoints_are_exact() {
    use crate::display::tonemap_cpu;
    use crate::retouch::overlay_colors;
    use crate::zmap::{NO_SOURCE, overlay_table};

    let width = 37u32;
    let height = 23u32;
    let n = width as usize * height as usize;
    let mut composite = vec![0u16; n * 3];
    let mut indices = vec![0u16; n];
    for i in 0..n {
        composite[i * 3] = (i as u16).wrapping_mul(773);
        composite[i * 3 + 1] = (i as u16).wrapping_mul(511);
        composite[i * 3 + 2] = (i as u16).wrapping_mul(997);
        indices[i] = if i % 7 == 0 {
            NO_SOURCE
        } else {
            (i % 5) as u16
        };
    }
    let table = overlay_table(5, None);
    let info = crate::zmap::ZMapInfo::new(width, height, 5).unwrap();
    let rect = crate::tile::Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let mut colors = vec![[0u8; 3]; n];
    overlay_colors(
        &[],
        info,
        rect,
        std::num::NonZeroU32::MIN,
        &table,
        &indices,
        &mut colors,
    );

    // Opacity 0 leaves tonemap_cpu's own composite output untouched.
    let mut expected = vec![0u8; n * 4];
    tonemap_cpu(&composite, width, height, 3, 0, &mut expected);
    let mut actual = vec![0u8; n * 4];
    overlay_cpu(
        &composite,
        &colors,
        width,
        height,
        alpha_from_opacity(0.0),
        &mut actual,
    );
    assert_eq!(
        actual, expected,
        "opacity 0 must equal tonemap_cpu's output"
    );

    // Opacity 1 leaves every pixel exactly its table colour, NO_SOURCE
    // pixels exactly the table's last entry.
    overlay_cpu(
        &composite,
        &colors,
        width,
        height,
        alpha_from_opacity(1.0),
        &mut actual,
    );
    for i in 0..n {
        let expected = table[(usize::from(indices[i])).min(table.len() - 1)];
        assert_eq!(&actual[i * 4..i * 4 + 3], &expected[..], "pixel {i}");
        assert_eq!(actual[i * 4 + 3], 255);
    }
}

#[cfg(test)]
#[test]
fn solo_desaturates_rather_than_hides() {
    use crate::zmap::overlay_table;

    for n in [8u32, 61] {
        let soloed_index = 3u16;
        let unsoloed = overlay_table(n, None);
        let soloed = overlay_table(n, Some(soloed_index));
        assert_eq!(soloed.len(), n as usize + 1);

        // The soloed index keeps its unsoloed colour.
        assert_eq!(
            soloed[soloed_index as usize],
            unsoloed[soloed_index as usize]
        );

        for i in 0..n as usize {
            if i == soloed_index as usize {
                continue;
            }
            let color = soloed[i];
            assert!(
                color[0] != color[1] || color[1] != color[2],
                "entry {i} is grey at n={n}: {color:?}"
            );
            assert_ne!(
                color,
                [0x7A, 0x7A, 0x7A],
                "entry {i} equals NO_SOURCE_COLOR at n={n}"
            );

            // Desaturated: narrower channel spread than the unsoloed
            // colour at the same index.
            let spread = |c: [u8; 3]| {
                i32::from(*c.iter().max().unwrap()) - i32::from(*c.iter().min().unwrap())
            };
            assert!(
                spread(color) < spread(unsoloed[i]),
                "entry {i} not desaturated at n={n}: {color:?} vs {:?}",
                unsoloed[i]
            );

            // Same hue and lightness: the same channel ordering, and the
            // same (max+min)/2 -- HSL's lightness identity.
            let rank = |c: [u8; 3]| {
                let mut idx = [0usize, 1, 2];
                idx.sort_by_key(|&k| c[k]);
                idx
            };
            assert_eq!(
                rank(color),
                rank(unsoloed[i]),
                "entry {i} changed hue sector at n={n}"
            );
            let lightness_proxy = |c: [u8; 3]| {
                i32::from(*c.iter().max().unwrap()) + i32::from(*c.iter().min().unwrap())
            };
            assert!(
                (lightness_proxy(color) - lightness_proxy(unsoloed[i])).abs() <= 1,
                "entry {i} changed lightness at n={n}"
            );
        }

        // NO_SOURCE's own entry is untouched by solo.
        assert_eq!(soloed[n as usize], unsoloed[n as usize]);
    }
}
