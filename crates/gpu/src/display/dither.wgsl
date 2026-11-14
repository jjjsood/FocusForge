// SPDX-License-Identifier: GPL-3.0-or-later

// The display kernels' ordered-dither reduction from 16 to 8 bits, declared
// without bindings. Shaders include it ahead of their own source.

// The 4x4 ordered (Bayer) dither matrix -- the WGSL twin of
// `focusforge_core::display::BAYER_4X4`, values `0..=15`.
const BAYER_4X4 = array<u32, 16>(
    0u, 8u, 2u, 10u,
    12u, 4u, 14u, 6u,
    3u, 11u, 1u, 9u,
    15u, 7u, 13u, 5u,
);

fn dither_threshold(x: u32, y: u32) -> u32 {
    return BAYER_4X4[(y % 4u) * 4u + (x % 4u)] * 16u + 8u;
}

// `focusforge_core::display::reduce16to8`'s WGSL twin.
fn reduce16to8(v: u32, x: u32, y: u32) -> u32 {
    let hi = v >> 8u;
    let lo = v & 0xffu;
    var bumped = hi;
    if (lo > dither_threshold(x, y)) {
        bumped = bumped + 1u;
    }
    return min(bumped, 255u);
}
