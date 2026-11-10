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

/// A non-soloed source's saturation in [`overlay_table`]'s solo mode: hue
/// and lightness stay put, so the boundary between two non-soloed sources
/// stays faintly visible against the soloed source's full [`SATURATION`].
const SOLO_SATURATION: f32 = 0.34;

/// The hue in degrees for source `index` out of `sources` total sources:
/// the undivided wheel, no banding.
fn hue_degrees(index: u32, sources: u32) -> f32 {
    (index as f32 / sources as f32) * 360.0
}

/// The legend colour for source `index` out of `sources` total sources:
/// hue `index / sources * 360°` at saturation 0.62 and lightness 0.55, no
/// banding. [`NO_SOURCE`] always returns the same neutral grey regardless
/// of `sources`.
pub fn source_color(index: u16, sources: u32) -> [u8; 3] {
    if index == NO_SOURCE {
        return NO_SOURCE_COLOR;
    }
    hsl_to_rgb(
        hue_degrees(u32::from(index), sources),
        SATURATION,
        LIGHTNESS,
    )
}

/// The legend colour for checkpoint number `colour_index` among `sources`
/// frames: the golden-ratio step `colour_index * 0.618034` picks one of the
/// `sources` hue slots and the colour sits halfway between that slot's frame
/// hue and the next, so it never equals a [`source_color`] and is never grey.
pub fn checkpoint_color(colour_index: u32, sources: u32) -> [u8; 3] {
    let n = f64::from(sources);
    let slot = (f64::from(colour_index) * 0.618034).fract() * n;
    let hue = (slot.floor() + 0.5) / n * 360.0;
    hsl_to_rgb(hue as f32, SATURATION, LIGHTNESS)
}

/// The overlay's colour table: `sources + 1` entries, index `i` the colour
/// [`source_color`] gives for source `i`, last entry `NO_SOURCE_COLOR`.
/// With `solo` set, every entry except the soloed index desaturates to
/// `SOLO_SATURATION` at the same hue and lightness -- none are removed,
/// so the unselected structure stays visible.
pub fn overlay_table(sources: u32, solo: Option<u16>) -> Vec<[u8; 3]> {
    (0..sources)
        .map(|i| {
            let saturation = match solo {
                Some(soloed) if u32::from(soloed) != i => SOLO_SATURATION,
                _ => SATURATION,
            };
            hsl_to_rgb(hue_degrees(i, sources), saturation, LIGHTNESS)
        })
        .chain(std::iter::once(NO_SOURCE_COLOR))
        .collect()
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
#[test]
fn hue_is_index_over_count_at_the_settled_constants() {
    // H=0, S=0.62, L=0.55: C=0.558, X=0, m=0.271.
    assert_eq!(source_color(0, 4), [211, 69, 69]);
    // index 1 of 61 sources: hue 5.9016... degrees, C=0.558, X=0.0549, m=0.271.
    assert_eq!(source_color(1, 61), [211, 83, 69]);
}

#[cfg(test)]
#[test]
fn a_quarter_turn_is_the_computed_green() {
    // index 1 of 4 sources: hue 90 degrees.
    assert_eq!(source_color(1, 4), [140, 211, 69]);
}

#[cfg(test)]
#[test]
fn the_wheel_divides_evenly_across_the_source_count() {
    let n = 61;
    let colors: Vec<[u8; 3]> = (0..n).map(|i| source_color(i, n as u32)).collect();
    // Every source in a real 61-frame stack gets its own colour -- no two
    // adjacent hues collapse onto the same swatch.
    for pair in colors.windows(2) {
        assert_ne!(pair[0], pair[1]);
    }
}

#[cfg(test)]
#[test]
fn no_source_is_a_fixed_neutral_regardless_of_the_source_count() {
    assert_eq!(source_color(NO_SOURCE, 4), NO_SOURCE_COLOR);
    assert_eq!(source_color(NO_SOURCE, 61), NO_SOURCE_COLOR);
}

#[cfg(test)]
#[test]
fn checkpoint_color_sits_between_frame_hues_at_the_pinned_values() {
    let hex = |c: [u8; 3]| format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]);
    let at = |sources: u32| -> Vec<String> {
        (0..4).map(|c| hex(checkpoint_color(c, sources))).collect()
    };
    assert_eq!(at(61), ["#d34c45", "#4571d3", "#97d345", "#d345bc"]);
    assert_eq!(at(8), ["#d37a45", "#459ed3", "#c2d345", "#c245d3"]);
    // Checkpoint 0 of 8 frames sits at hue 22.5 degrees, halfway between
    // frames 0 and 1.
    assert_eq!(
        checkpoint_color(0, 8),
        hsl_to_rgb(22.5, SATURATION, LIGHTNESS)
    );
}

#[cfg(test)]
#[test]
fn checkpoint_color_never_equals_a_frame_colour_or_the_no_source_grey() {
    for sources in [8u32, 61] {
        let frames: Vec<[u8; 3]> = (0..sources as u16)
            .map(|i| source_color(i, sources))
            .collect();
        for c in 0..64 {
            assert!(
                !frames.contains(&checkpoint_color(c, sources)),
                "checkpoint {c} of {sources} frames equals a frame colour"
            );
        }
    }
    for sources in 1u32..=1024 {
        for c in 0..64 {
            assert_ne!(
                checkpoint_color(c, sources),
                NO_SOURCE_COLOR,
                "checkpoint {c} of {sources} frames is the NO_SOURCE grey"
            );
        }
    }
}

/// `NO_SOURCE` never collides with a real hue, and no real hue is ever
/// grey, for every frame count `1..=1024` -- far past the fixture's 8 and
/// the reference stack's 61.
#[cfg(test)]
#[test]
fn no_source_is_outside_the_wheel() {
    for n in 1u32..=1024 {
        for i in 0..n as u16 {
            let color = source_color(i, n);
            assert_ne!(
                color, NO_SOURCE_COLOR,
                "source {i} of {n} collides with NO_SOURCE"
            );
            assert!(
                color[0] != color[1] || color[1] != color[2],
                "source {i} of {n} is grey: {color:?}"
            );
        }
    }
}
