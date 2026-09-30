// SPDX-License-Identifier: GPL-3.0-or-later

//! The host half of the warp kernel: per-destination-pixel coordinate
//! mapping and the out-of-footprint precheck, in `f64` as `crates/core`
//! does. Names no `wgpu` type, so it is not feature-gated. [`map_coords`]
//! checks per pixel in raster order, not from the corners, so a
//! `SourceTileTooSmall` carries the CPU twin's exact `needed` rect.

use focusforge_core::align::warp::clamp_tap;
use focusforge_core::align::{AlignError, Transform};
use focusforge_core::tile::Rect;

/// One destination pixel's mapped source coordinate, laid out to match the
/// WGSL `Coord` struct (`warp_bicubic.wgsl`) exactly: 24 bytes, every member
/// 4-byte aligned. `ix`/`iy` are `floor(sx)`/`floor(sy)` in *absolute frame*
/// pixel coordinates (not relative to the source tile -- the kernel
/// subtracts the source tile's own origin); `tx`/`ty` are the `f32`
/// fractional part, the single `f64 -> f32` cast the coordinate decision
/// rests on; `covered` is `1` for a covered pixel and `0` otherwise (an
/// uncovered `Coord`'s other fields are `0`, never read by the kernel).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coord {
    pub ix: i32,
    pub iy: i32,
    pub tx: f32,
    pub ty: f32,
    pub covered: u32,
    pub _pad: u32,
}

const UNCOVERED: Coord = Coord {
    ix: 0,
    iy: 0,
    tx: 0.0,
    ty: 0.0,
    covered: 0,
    _pad: 0,
};

/// The host-computed coordinate pass for one destination tile: one [`Coord`]
/// per destination pixel plus the same coverage the caller returns as
/// [`focusforge_core::align::warp::WarpedTile`]'s own `covered` vector --
/// `covered` here is never re-derived from a GPU readback, it is the host's
/// own decision.
#[derive(Debug)]
pub struct WarpCoords {
    pub coords: Vec<Coord>,
    pub covered: Vec<bool>,
}

/// `warp_from_tile_into`'s per-pixel head, stopping short of sampling: per
/// destination pixel in raster order, decides coverage via `t.map_point`,
/// floors the source coordinate, forms the `f32` fraction once, and returns
/// the CPU twin's [`AlignError::SourceTileTooSmall`] (same `needed`, same
/// `have`) on the first covered pixel whose taps do not fit `src_rect`.
pub fn map_coords(
    src_rect: Rect,
    frame: (u32, u32),
    dst: Rect,
    t: &Transform,
) -> Result<WarpCoords, AlignError> {
    let (frame_w, frame_h) = frame;
    let pixel_count = dst.width as usize * dst.height as usize;
    let mut coords = Vec::with_capacity(pixel_count);
    let mut covered = Vec::with_capacity(pixel_count);

    for dy in 0..dst.height {
        for dx in 0..dst.width {
            let ddx = (dst.x + dx) as f64;
            let ddy = (dst.y + dy) as f64;

            let mapped = t.map_point(ddx, ddy);
            let covered_centre = mapped.is_some_and(|(sx, sy)| {
                sx >= 0.0 && sx <= (frame_w - 1) as f64 && sy >= 0.0 && sy <= (frame_h - 1) as f64
            });

            if !covered_centre {
                coords.push(UNCOVERED);
                covered.push(false);
                continue;
            }

            let (sx, sy) = mapped.expect("covered_centre implies mapped is Some");
            let ix = sx.floor() as i64;
            let iy = sy.floor() as i64;
            let tx = (sx - ix as f64) as f32;
            let ty = (sy - iy as f64) as f32;

            let cols0 = clamp_tap(ix - 1, frame_w);
            let cols3 = clamp_tap(ix + 2, frame_w);
            let rows0 = clamp_tap(iy - 1, frame_h);
            let rows3 = clamp_tap(iy + 2, frame_h);
            let needed = Rect {
                x: cols0 as u32,
                y: rows0 as u32,
                width: (cols3 - cols0 + 1) as u32,
                height: (rows3 - rows0 + 1) as u32,
            };
            let fits = needed.x >= src_rect.x
                && needed.y >= src_rect.y
                && needed.x + needed.width <= src_rect.x + src_rect.width
                && needed.y + needed.height <= src_rect.y + src_rect.height;
            if !fits {
                return Err(AlignError::SourceTileTooSmall {
                    needed,
                    have: src_rect,
                });
            }

            coords.push(Coord {
                ix: ix as i32,
                iy: iy as i32,
                tx,
                ty,
                covered: 1,
                _pad: 0,
            });
            covered.push(true);
        }
    }

    Ok(WarpCoords { coords, covered })
}

/// Explicit `to_le_bytes` per field, never a `bytemuck` transmute -- the
/// workspace denies `unsafe_code`, the same reason `crate::pack` gives.
pub fn coords_bytes(coords: &[Coord]) -> Vec<u8> {
    let mut out = Vec::with_capacity(coords.len() * 24);
    for c in coords {
        out.extend_from_slice(&c.ix.to_le_bytes());
        out.extend_from_slice(&c.iy.to_le_bytes());
        out.extend_from_slice(&c.tx.to_le_bytes());
        out.extend_from_slice(&c.ty.to_le_bytes());
        out.extend_from_slice(&c.covered.to_le_bytes());
        out.extend_from_slice(&c._pad.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use focusforge_core::align::{AlignError, Transform};
    use focusforge_core::tile::{Rect, Tile};

    use super::{Coord, coords_bytes, map_coords};

    #[test]
    fn identity_transform_gives_t_zero_and_covered() {
        let t = Transform::identity();
        let src_rect = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let dst = Rect {
            x: 5,
            y: 5,
            width: 1,
            height: 1,
        };
        let frame = (10, 10);

        let result = map_coords(src_rect, frame, dst, &t).expect("identity always fits");

        assert_eq!(result.coords.len(), 1);
        assert_eq!(result.covered, vec![true]);
        let c = &result.coords[0];
        assert_eq!(c.ix, 5);
        assert_eq!(c.iy, 5);
        assert_eq!(c.tx, 0.0);
        assert_eq!(c.ty, 0.0);
        assert_eq!(c.covered, 1);
    }

    #[test]
    fn w_vanishing_coordinate_is_reported_uncovered() {
        // w = 1 - x: column dx=0 (x=0) has w=1 (covered), dx>=1 crosses the
        // horizon (w <= 0), matching `crates/core`'s vanishing-w test.
        let t = Transform::projective([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 1.0]]);
        let src_rect = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let dst = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        let frame = (100, 100);

        let result = map_coords(src_rect, frame, dst, &t).expect("no dispatch-blocking error");

        for dy in 0..4usize {
            for dx in 0..4usize {
                let idx = dy * 4 + dx;
                if dx == 0 {
                    assert!(result.covered[idx], "dx=0 dy={dy} must be covered");
                    let c = &result.coords[idx];
                    assert_eq!(c.ix, 0);
                    assert_eq!(c.iy, dy as i32);
                    assert_eq!(c.covered, 1);
                } else {
                    assert!(!result.covered[idx], "dx={dx} dy={dy} must be uncovered");
                    assert_eq!(result.coords[idx].covered, 0);
                }
            }
        }
    }

    #[test]
    fn source_too_small_matches_the_cpu_twins_payload() {
        // Same geometry as the CPU twin's own too-small-source test.
        let src = Tile {
            rect: Rect {
                x: 5,
                y: 5,
                width: 1,
                height: 1,
            },
            samples_per_pixel: 1,
            samples: vec![1000],
        };
        let dst = Rect {
            x: 5,
            y: 5,
            width: 1,
            height: 1,
        };
        let t = Transform::identity();
        let frame = (100, 100);

        let gpu_err = map_coords(src.rect, frame, dst, &t)
            .expect_err("a source tile with no kernel margin must be refused");
        let cpu_err = focusforge_core::align::warp::warp_from_tile(&src, frame, dst, &t)
            .expect_err("the CPU twin must refuse the same geometry");

        match (gpu_err, cpu_err) {
            (
                AlignError::SourceTileTooSmall {
                    needed: gpu_needed,
                    have: gpu_have,
                },
                AlignError::SourceTileTooSmall {
                    needed: cpu_needed,
                    have: cpu_have,
                },
            ) => {
                assert_eq!(gpu_needed, cpu_needed);
                assert_eq!(gpu_have, cpu_have);
            }
            other => panic!("expected SourceTileTooSmall on both sides, got {other:?}"),
        }
    }

    #[test]
    fn coords_bytes_stride_is_24_bytes_per_coord() {
        let t = Transform::identity();
        let src_rect = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let dst = Rect {
            x: 5,
            y: 5,
            width: 1,
            height: 1,
        };
        let frame = (10, 10);
        let result = map_coords(src_rect, frame, dst, &t).expect("identity always fits");

        let bytes = coords_bytes(&result.coords);

        assert_eq!(bytes.len(), 24);
        assert_eq!(std::mem::size_of::<Coord>(), 24);
    }
}
