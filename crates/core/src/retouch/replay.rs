// SPDX-License-Identifier: GPL-3.0-or-later

//! Replaying a checkpoint's op list against a Z-Map: the one bounds-checked
//! entry point every variant passes through before a pixel is written, plus
//! the pure function that turns a parent map and an op list into the
//! working map.

use crate::project::{Op, Point};
use crate::tile::Rect;
use crate::zmap::{ZMapError, ZMapInfo};

use super::{RetouchError, brush, zmap_io};

/// Reads `rect`'s indices from the nearest fused ancestor's map into `buf`.
type FusedRead<'a> = dyn FnMut(Rect, &mut [u16]) -> Result<(), ZMapError> + 'a;

/// The one bounds-checking entry point for all four [`Op`] variants.
/// `BrushStroke` clips to the map exactly as [`brush::footprint`] already
/// does. `ResetRegion`'s rect must lie inside the map or it is
/// [`RetouchError::RegionOutsideMap`] -- which also catches the zero-size
/// `#[serde(default)]` rect. `FloodFill`'s seed must lie inside the map
/// ([`RetouchError::SeedOutsideMap`] otherwise), but even a valid seed ends
/// in [`RetouchError::FloodFillNotReplayable`]: flood fill has no defined
/// replay semantics. `Refuse` is always
/// [`RetouchError::RefuseNotReplayable`] -- a re-fuse is its own checkpoint.
pub fn footprint(op: &Op, info: ZMapInfo) -> Result<Option<Rect>, RetouchError> {
    match op {
        Op::BrushStroke { .. } => brush::footprint(op, info),
        Op::ResetRegion { rect } => {
            if !info.contains(*rect) {
                return Err(RetouchError::RegionOutsideMap { rect: *rect });
            }
            Ok(Some(*rect))
        }
        Op::FloodFill { seed, .. } => {
            if !point_in_bounds(*seed, info) {
                return Err(RetouchError::SeedOutsideMap { seed: *seed });
            }
            Err(RetouchError::FloodFillNotReplayable)
        }
        Op::Refuse { .. } => Err(RetouchError::RefuseNotReplayable),
    }
}

fn point_in_bounds(p: Point, info: ZMapInfo) -> bool {
    p.x >= 0 && p.y >= 0 && (p.x as u32) < info.width() && (p.y as u32) < info.height()
}

/// The union of every op's footprint over `info`, exactly like
/// [`super::session_tile`] but through [`footprint`] rather than
/// [`brush::footprint`] directly -- so a `FloodFill` or `Refuse` anywhere in
/// the list is caught here, before any tile is read. `Err` carries the
/// 0-based index of the first op that refuses. `Ok` of a 1x1 rect at the
/// origin when no op has a footprint.
pub fn replay_tile(ops: &[Op], info: ZMapInfo) -> Result<Rect, (usize, RetouchError)> {
    let mut union: Option<Rect> = None;
    for (index, op) in ops.iter().enumerate() {
        let region = footprint(op, info).map_err(|err| (index, err))?;
        if let Some(rect) = region {
            union = Some(match union {
                Some(existing) => zmap_io::union_rect(existing, rect),
                None => rect,
            });
        }
    }
    Ok(union.unwrap_or(Rect {
        x: 0,
        y: 0,
        width: 1,
        height: 1,
    }))
}

/// Replays `ops` in order against `tile`'s resident `indices`
/// (`info.tile_len(tile)` long, row-major over `tile`). `BrushStroke` paints
/// through [`brush::apply`]; `ResetRegion` copies its rect, intersected with
/// `tile`, from `fused` -- the nearest fused ancestor's map. Every op in the
/// list passes [`footprint`] before the first write of *any* op, so a
/// refusal anywhere in the list -- including one that sits after an
/// otherwise-valid op -- leaves `indices` completely untouched. Returns how
/// many pixels changed in total; `Err` carries the 0-based index of the op
/// that refused.
pub fn replay(
    ops: &[Op],
    info: ZMapInfo,
    tile: Rect,
    indices: &mut [u16],
    fused: &mut FusedRead<'_>,
) -> Result<u64, (usize, RetouchError)> {
    let mut regions = Vec::with_capacity(ops.len());
    for (index, op) in ops.iter().enumerate() {
        regions.push(footprint(op, info).map_err(|err| (index, err))?);
    }

    let mut changed = 0u64;
    for (index, (op, region)) in ops.iter().zip(regions).enumerate() {
        let Some(region) = region else {
            continue;
        };
        match op {
            Op::BrushStroke { .. } => {
                changed += brush::apply(op, info, tile, indices).map_err(|err| (index, err))?;
            }
            Op::ResetRegion { .. } => {
                let Some(overlap) = region.intersect(tile) else {
                    continue;
                };
                let mut buf = vec![0u16; info.tile_len(overlap)];
                fused(overlap, &mut buf).map_err(|err| (index, RetouchError::Fused(err)))?;
                for row in 0..overlap.height {
                    let tile_row = (overlap.y + row) - tile.y;
                    let tile_start = (tile_row * tile.width + (overlap.x - tile.x)) as usize;
                    let buf_start = (row * overlap.width) as usize;
                    for col in 0..overlap.width as usize {
                        let new_index = buf[buf_start + col];
                        let slot = &mut indices[tile_start + col];
                        if *slot != new_index {
                            *slot = new_index;
                            changed += 1;
                        }
                    }
                }
            }
            Op::FloodFill { .. } | Op::Refuse { .. } => {
                unreachable!("footprint refuses FloodFill and Refuse before this match")
            }
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Op;

    fn info() -> ZMapInfo {
        ZMapInfo::new(16, 16, 4).unwrap()
    }

    #[test]
    fn footprint_of_brush_stroke_delegates_to_brush_footprint() {
        let op = Op::BrushStroke {
            source_id: 1,
            path: vec![crate::project::Point { x: 4, y: 4 }],
            radius: 2.0,
            softness: 0.0,
        };
        let region = footprint(&op, info()).unwrap();
        assert_eq!(region, brush::footprint(&op, info()).unwrap());
    }

    #[test]
    fn footprint_of_reset_region_refuses_a_rect_outside_the_map() {
        let op = Op::ResetRegion {
            rect: Rect {
                x: 10,
                y: 10,
                width: 10,
                height: 10,
            },
        };
        match footprint(&op, info()) {
            Err(RetouchError::RegionOutsideMap { .. }) => {}
            other => panic!("expected RegionOutsideMap, got {other:?}"),
        }
    }

    #[test]
    fn footprint_of_reset_region_accepts_an_on_map_rect() {
        let rect = Rect {
            x: 1,
            y: 1,
            width: 2,
            height: 2,
        };
        let op = Op::ResetRegion { rect };
        assert_eq!(footprint(&op, info()).unwrap(), Some(rect));
    }

    #[test]
    fn footprint_of_flood_fill_refuses_a_seed_outside_the_map() {
        let op = Op::FloodFill {
            seed: crate::project::Point { x: -1, y: 0 },
            source_id: 0,
            tolerance: 0.1,
        };
        match footprint(&op, info()) {
            Err(RetouchError::SeedOutsideMap { .. }) => {}
            other => panic!("expected SeedOutsideMap, got {other:?}"),
        }
    }

    #[test]
    fn footprint_of_flood_fill_is_not_replayable_even_with_a_valid_seed() {
        let op = Op::FloodFill {
            seed: crate::project::Point { x: 2, y: 2 },
            source_id: 0,
            tolerance: 0.1,
        };
        match footprint(&op, info()) {
            Err(RetouchError::FloodFillNotReplayable) => {}
            other => panic!("expected FloodFillNotReplayable, got {other:?}"),
        }
    }

    #[test]
    fn footprint_of_refuse_is_not_replayable() {
        let op = Op::Refuse {
            algorithm: "laplacian".to_string(),
            params: crate::params::ParamValues::new(),
        };
        match footprint(&op, info()) {
            Err(RetouchError::RefuseNotReplayable) => {}
            other => panic!("expected RefuseNotReplayable, got {other:?}"),
        }
    }
}
