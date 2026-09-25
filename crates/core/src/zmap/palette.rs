// SPDX-License-Identifier: GPL-3.0-or-later

//! The legend colour a source index maps to: an undivided hue wheel at
//! fixed saturation and lightness, with a defined swatch for
//! [`NO_SOURCE`].

use super::NO_SOURCE;

/// [`NO_SOURCE`]'s own swatch -- a neutral grey no hue on the wheel ever
/// produces, so it cannot be mistaken for a source colour.
const NO_SOURCE_COLOR: [u8; 3] = [0x7A, 0x7A, 0x7A];

const SATURATION: f32 = 0.62;
const LIGHTNESS: f32 = 0.55;

/// The legend colour for source `index` out of `sources` total sources:
/// hue `index / sources * 360°` at saturation 0.62 and lightness 0.55, no
/// banding. [`NO_SOURCE`] always returns the same neutral grey regardless
/// of `sources`.
pub fn source_color(index: u16, sources: u32) -> [u8; 3] {
    if index == NO_SOURCE {
        return NO_SOURCE_COLOR;
    }
    let hue = (index as f32 / sources as f32) * 360.0;
    hsl_to_rgb(hue, SATURATION, LIGHTNESS)
}

/// Standard HSL-to-RGB, `hue` in degrees (wrapped into `[0, 360)`), `s` and
/// `l` in `[0, 1]`.
fn hsl_to_rgb(hue: f32, s: f32, l: f32) -> [u8; 3] {
    let h = hue.rem_euclid(360.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r1, g1, b1) = match h as u32 {
        0..=59 => (c, x, 0.0),
        60..=119 => (x, c, 0.0),
        120..=179 => (0.0, c, x),
        180..=239 => (0.0, x, c),
        240..=299 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [
        (((r1 + m) * 255.0).round()) as u8,
        (((g1 + m) * 255.0).round()) as u8,
        (((b1 + m) * 255.0).round()) as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hue_zero_is_the_computed_red() {
        // H=0, S=0.62, L=0.55: C=0.558, X=0, m=0.271.
        assert_eq!(source_color(0, 4), [211, 69, 69]);
    }

    #[test]
    fn a_quarter_turn_is_the_computed_green() {
        // index 1 of 4 sources: hue 90 degrees.
        assert_eq!(source_color(1, 4), [140, 211, 69]);
    }

    #[test]
    fn the_wheel_divides_evenly_across_the_source_count() {
        let n = 61;
        let colors: Vec<[u8; 3]> = (0..n).map(|i| source_color(i, n as u32)).collect();
        // Every source in a real 61-frame stack gets its own colour --
        // no two adjacent hues collapse onto the same swatch.
        for pair in colors.windows(2) {
            assert_ne!(pair[0], pair[1]);
        }
    }

    #[test]
    fn no_source_is_a_fixed_neutral_regardless_of_the_source_count() {
        assert_eq!(source_color(NO_SOURCE, 4), NO_SOURCE_COLOR);
        assert_eq!(source_color(NO_SOURCE, 61), NO_SOURCE_COLOR);
    }

    #[test]
    fn no_source_never_collides_with_a_real_hue() {
        for n in [1u32, 2, 4, 61] {
            for i in 0..n as u16 {
                assert_ne!(source_color(i, n), NO_SOURCE_COLOR);
            }
        }
    }
}
