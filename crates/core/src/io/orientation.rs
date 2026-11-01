// SPDX-License-Identifier: GPL-3.0-or-later

//! EXIF Orientation: how a frame as stored maps onto the upright frame the
//! pipeline sees. All eight maps send rectangles to rectangles, so an upright
//! tile reads exactly one stored rectangle.

use crate::tile::Rect;

/// The eight EXIF Orientation values, named by the transform that turns the
/// stored frame upright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Orientation {
    /// 1: stored as displayed.
    Upright,
    /// 2: mirrored left to right.
    MirrorX,
    /// 3: rotated 180 degrees.
    Rotate180,
    /// 4: mirrored top to bottom.
    MirrorY,
    /// 5: transposed across the main diagonal.
    Transpose,
    /// 6: rotated 90 degrees clockwise to display.
    Rotate90,
    /// 7: transposed across the anti-diagonal.
    Transverse,
    /// 8: rotated 90 degrees counter-clockwise to display.
    Rotate270,
}

impl Orientation {
    /// The orientation for an EXIF tag value; anything outside 1-8 is upright.
    pub(crate) fn from_exif(value: u16) -> Self {
        match value {
            2 => Self::MirrorX,
            3 => Self::Rotate180,
            4 => Self::MirrorY,
            5 => Self::Transpose,
            6 => Self::Rotate90,
            7 => Self::Transverse,
            8 => Self::Rotate270,
            _ => Self::Upright,
        }
    }

    /// The EXIF tag value (1-8) of this orientation.
    pub(crate) fn exif(self) -> u16 {
        match self {
            Self::Upright => 1,
            Self::MirrorX => 2,
            Self::Rotate180 => 3,
            Self::MirrorY => 4,
            Self::Transpose => 5,
            Self::Rotate90 => 6,
            Self::Transverse => 7,
            Self::Rotate270 => 8,
        }
    }

    /// The upright `(width, height)` of a frame stored `stored_w` x `stored_h`.
    pub(crate) fn upright_dims(self, stored_w: u32, stored_h: u32) -> (u32, u32) {
        match self {
            Self::Transpose | Self::Rotate90 | Self::Transverse | Self::Rotate270 => {
                (stored_h, stored_w)
            }
            _ => (stored_w, stored_h),
        }
    }

    /// The stored pixel that upright pixel `(x, y)` shows.
    pub(crate) fn stored_xy(self, x: u32, y: u32, stored_w: u32, stored_h: u32) -> (u32, u32) {
        let (last_x, last_y) = (stored_w - 1, stored_h - 1);
        match self {
            Self::Upright => (x, y),
            Self::MirrorX => (last_x - x, y),
            Self::Rotate180 => (last_x - x, last_y - y),
            Self::MirrorY => (x, last_y - y),
            Self::Transpose => (y, x),
            Self::Rotate90 => (y, last_y - x),
            Self::Transverse => (last_x - y, last_y - x),
            Self::Rotate270 => (last_x - y, x),
        }
    }

    /// The stored rectangle that `upright` reads.
    pub(crate) fn stored_rect(self, upright: Rect, stored_w: u32, stored_h: u32) -> Rect {
        let (ax, ay) = self.stored_xy(upright.x, upright.y, stored_w, stored_h);
        let (bx, by) = self.stored_xy(
            upright.x + upright.width - 1,
            upright.y + upright.height - 1,
            stored_w,
            stored_h,
        );
        Rect {
            x: ax.min(bx),
            y: ay.min(by),
            width: ax.abs_diff(bx) + 1,
            height: ay.abs_diff(by) + 1,
        }
    }

    /// Fills `out` with the `upright` tile from `stored`, the samples of
    /// `stored_rect(upright, ..)` row-major with `samples_per_pixel`
    /// interleaved; `stored_dims` is the stored frame's size.
    pub(crate) fn fill_upright(
        self,
        stored: &[u16],
        upright: Rect,
        stored_dims: (u32, u32),
        samples_per_pixel: usize,
        out: &mut [u16],
    ) {
        let source = self.stored_rect(upright, stored_dims.0, stored_dims.1);
        let mut pixels = out.chunks_exact_mut(samples_per_pixel);
        for y in upright.y..upright.y + upright.height {
            for x in upright.x..upright.x + upright.width {
                let (sx, sy) = self.stored_xy(x, y, stored_dims.0, stored_dims.1);
                let index = ((sy - source.y) as usize * source.width as usize
                    + (sx - source.x) as usize)
                    * samples_per_pixel;
                pixels
                    .next()
                    .expect("output holds the upright tile")
                    .copy_from_slice(&stored[index..index + samples_per_pixel]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Orientation; 8] = [
        Orientation::Upright,
        Orientation::MirrorX,
        Orientation::Rotate180,
        Orientation::MirrorY,
        Orientation::Transpose,
        Orientation::Rotate90,
        Orientation::Transverse,
        Orientation::Rotate270,
    ];
    const STORED: (u32, u32) = (7, 4);

    /// Where stored pixel `(x, y)` lands upright, written out per EXIF value.
    fn upright_position(value: u16, x: u32, y: u32) -> (u32, u32) {
        let (w, h) = STORED;
        match value {
            1 => (x, y),
            2 => (w - 1 - x, y),
            3 => (w - 1 - x, h - 1 - y),
            4 => (x, h - 1 - y),
            5 => (y, x),
            6 => (h - 1 - y, x),
            7 => (h - 1 - y, w - 1 - x),
            8 => (y, w - 1 - x),
            _ => unreachable!("EXIF orientation is 1-8"),
        }
    }

    #[test]
    fn from_exif_maps_out_of_range_values_to_upright() {
        for value in [0, 9, 255, u16::MAX] {
            assert_eq!(Orientation::from_exif(value), Orientation::Upright);
        }
        for (value, orientation) in (1..=8).zip(ALL) {
            assert_eq!(Orientation::from_exif(value), orientation);
            assert_eq!(orientation.exif(), value);
        }
    }

    #[test]
    fn upright_dims_swap_for_values_five_to_eight() {
        for (value, orientation) in (1..=8).zip(ALL) {
            let expected = if value >= 5 { (4, 7) } else { (7, 4) };
            assert_eq!(orientation.upright_dims(7, 4), expected, "value {value}");
        }
    }

    #[test]
    fn stored_xy_inverts_the_display_transform() {
        for (value, orientation) in (1..=8).zip(ALL) {
            for y in 0..STORED.1 {
                for x in 0..STORED.0 {
                    let (ux, uy) = upright_position(value, x, y);
                    assert_eq!(
                        orientation.stored_xy(ux, uy, STORED.0, STORED.1),
                        (x, y),
                        "value {value}"
                    );
                }
            }
        }
    }

    #[test]
    fn stored_rect_is_the_bounding_box_of_stored_xy() {
        for (value, orientation) in (1..=8).zip(ALL) {
            let (uw, uh) = orientation.upright_dims(STORED.0, STORED.1);
            let upright = Rect {
                x: 1,
                y: 2,
                width: uw - 2,
                height: uh - 3,
            };
            let (mut min, mut max) = ((u32::MAX, u32::MAX), (0, 0));
            for y in upright.y..upright.y + upright.height {
                for x in upright.x..upright.x + upright.width {
                    let (sx, sy) = orientation.stored_xy(x, y, STORED.0, STORED.1);
                    min = (min.0.min(sx), min.1.min(sy));
                    max = (max.0.max(sx), max.1.max(sy));
                }
            }
            assert_eq!(
                orientation.stored_rect(upright, STORED.0, STORED.1),
                Rect {
                    x: min.0,
                    y: min.1,
                    width: max.0 - min.0 + 1,
                    height: max.1 - min.1 + 1,
                },
                "value {value}"
            );
        }
    }

    #[test]
    fn fill_upright_reads_the_stored_rect_in_display_order() {
        let (w, h) = STORED;
        let spp = 2;
        let sample = |x: u32, y: u32, c: usize| (y * w + x) as u16 * 4 + c as u16;
        for (value, orientation) in (1..=8).zip(ALL) {
            let (uw, uh) = orientation.upright_dims(w, h);
            let upright = Rect {
                x: 1,
                y: 1,
                width: uw - 2,
                height: uh - 1,
            };
            let rect = orientation.stored_rect(upright, w, h);
            let mut stored = Vec::new();
            for y in rect.y..rect.y + rect.height {
                for x in rect.x..rect.x + rect.width {
                    stored.extend((0..spp).map(|c| sample(x, y, c)));
                }
            }
            let mut out = vec![0; (upright.width * upright.height) as usize * spp];
            orientation.fill_upright(&stored, upright, STORED, spp, &mut out);
            for y in 0..h {
                for x in 0..w {
                    let (ux, uy) = upright_position(value, x, y);
                    let inside = (upright.x..upright.x + upright.width).contains(&ux)
                        && (upright.y..upright.y + upright.height).contains(&uy);
                    if inside {
                        let at =
                            (((uy - upright.y) * upright.width + ux - upright.x) as usize) * spp;
                        assert_eq!(out[at], sample(x, y, 0), "value {value} at {ux},{uy}");
                        assert_eq!(out[at + 1], sample(x, y, 1), "value {value} at {ux},{uy}");
                    }
                }
            }
        }
    }
}
