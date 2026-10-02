// SPDX-License-Identifier: GPL-3.0-or-later

//! Replaying an op list against a hand-built Z-Map: exactness against
//! [`EditSession`]'s own accumulation, determinism, order dependence, one
//! test per replayable [`Op`] variant, the two refusal cases and the
//! empty-list identity.

use focusforge_core::project::{Op, Point};
use focusforge_core::retouch::replay::{self, footprint};
use focusforge_core::retouch::{EditSession, RetouchError};
use focusforge_core::tile::Rect;
use focusforge_core::zmap::{NO_SOURCE, ZMapError, ZMapInfo};

/// Not a multiple of 8 -- the Bayer period a brush stroke's soft edge keys
/// on -- so a tile boundary that lands mid-period is exercised.
const WIDTH: u32 = 37;
const HEIGHT: u32 = 23;
const FRAMES: u32 = 6;

fn info() -> ZMapInfo {
    ZMapInfo::new(WIDTH, HEIGHT, FRAMES).unwrap()
}

fn full_rect(info: ZMapInfo) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: info.width(),
        height: info.height(),
    }
}

fn checker(info: ZMapInfo) -> Vec<u16> {
    (0..info.height())
        .flat_map(|y| (0..info.width()).map(move |x| ((x + y) % info.frame_count()) as u16))
        .collect()
}

fn extract(base: &[u16], info: ZMapInfo, tile: Rect) -> Vec<u16> {
    let mut out = Vec::with_capacity(info.tile_len(tile));
    for y in tile.y..tile.y + tile.height {
        let start = (y * info.width() + tile.x) as usize;
        out.extend_from_slice(&base[start..start + tile.width as usize]);
    }
    out
}

fn stroke(source_id: u16, path: Vec<Point>, radius: f32) -> Op {
    Op::BrushStroke {
        source_id,
        path,
        radius,
        softness: 0.0,
    }
}

fn reset(rect: Rect) -> Op {
    Op::ResetRegion { rect }
}

/// A `fused` callback that reads straight out of `source`, at the map's own
/// full-width stride -- what every `ResetRegion` test needs to stand in for
/// a nearest fused ancestor's map.
fn fused_from<'a>(
    source: &'a [u16],
    info: ZMapInfo,
) -> impl FnMut(Rect, &mut [u16]) -> Result<(), ZMapError> + 'a {
    move |rect: Rect, buf: &mut [u16]| {
        for row in 0..rect.height {
            let src = ((rect.y + row) * info.width() + rect.x) as usize;
            let dst = (row * rect.width) as usize;
            buf[dst..dst + rect.width as usize]
                .copy_from_slice(&source[src..src + rect.width as usize]);
        }
        Ok(())
    }
}

fn no_fused(_rect: Rect, _buf: &mut [u16]) -> Result<(), ZMapError> {
    Ok(())
}

#[test]
fn replay_reproduces_the_working_map_exactly() {
    let info = info();
    let base = checker(info);
    let ops = vec![
        stroke(1, vec![Point { x: 0, y: 0 }, Point { x: 10, y: 5 }], 4.0),
        stroke(3, vec![Point { x: 20, y: 15 }], 3.0),
    ];

    let tile = replay::replay_tile(&ops, info).unwrap();
    assert!(
        tile.width < info.width() && tile.height < info.height(),
        "the tile must be smaller than the whole map: {tile:?}"
    );

    let session_seed = extract(&base, info, tile);
    let mut session = EditSession::new(info, tile, session_seed).unwrap();
    for op in &ops {
        session.append(op.clone()).unwrap();
    }

    let mut replayed = extract(&base, info, tile);
    let changed = replay::replay(&ops, info, tile, &mut replayed, &mut no_fused).unwrap();
    assert!(changed > 0);
    assert_eq!(replayed, session.indices());
}

#[test]
fn replay_is_deterministic() {
    let info = info();
    let base = checker(info);
    let ops = vec![
        stroke(2, vec![Point { x: 5, y: 5 }, Point { x: 15, y: 10 }], 3.0),
        reset(Rect {
            x: 1,
            y: 1,
            width: 5,
            height: 4,
        }),
    ];
    let full = full_rect(info);

    let mut whole_a = base.clone();
    replay::replay(&ops, info, full, &mut whole_a, &mut fused_from(&base, info)).unwrap();
    let mut whole_b = base.clone();
    replay::replay(&ops, info, full, &mut whole_b, &mut fused_from(&base, info)).unwrap();
    assert_eq!(
        whole_a, whole_b,
        "two whole-map replays must be byte-identical"
    );

    let tile = replay::replay_tile(&ops, info).unwrap();
    let mut tile_a = extract(&base, info, tile);
    replay::replay(&ops, info, tile, &mut tile_a, &mut fused_from(&base, info)).unwrap();
    let mut tile_b = extract(&base, info, tile);
    replay::replay(&ops, info, tile, &mut tile_b, &mut fused_from(&base, info)).unwrap();
    assert_eq!(tile_a, tile_b, "two tile replays must be byte-identical");

    for y in tile.y..tile.y + tile.height {
        let whole_start = (y * info.width() + tile.x) as usize;
        let tile_start = ((y - tile.y) * tile.width) as usize;
        assert_eq!(
            &whole_a[whole_start..whole_start + tile.width as usize],
            &tile_a[tile_start..tile_start + tile.width as usize],
            "the whole-map replay and the tile replay must agree over the tile"
        );
    }
}

#[test]
fn overlapping_ops_are_order_dependent() {
    let info = info();
    let base = checker(info);
    let full = full_rect(info);

    let stroke_op = stroke(4, vec![Point { x: 10, y: 10 }], 5.0);
    let reset_op = reset(Rect {
        x: 8,
        y: 8,
        width: 6,
        height: 6,
    });
    let forward = vec![stroke_op.clone(), reset_op.clone()];
    let backward = vec![reset_op, stroke_op];

    let mut a = base.clone();
    replay::replay(&forward, info, full, &mut a, &mut fused_from(&base, info)).unwrap();
    let mut b = base.clone();
    replay::replay(&backward, info, full, &mut b, &mut fused_from(&base, info)).unwrap();
    assert_ne!(
        a, b,
        "stroke-then-reset must differ from reset-then-stroke over the same region"
    );

    let s1 = stroke(1, vec![Point { x: 20, y: 15 }], 2.0);
    let s2 = stroke(5, vec![Point { x: 20, y: 15 }], 2.0);
    let mut c = base.clone();
    replay::replay(&[s1.clone(), s2.clone()], info, full, &mut c, &mut no_fused).unwrap();
    let mut d = base.clone();
    replay::replay(&[s2, s1], info, full, &mut d, &mut no_fused).unwrap();
    assert_ne!(
        c, d,
        "two overlapping strokes with different source_id must differ by order"
    );
}

#[test]
fn op_variant_brush_stroke_replays() {
    let info = info();
    let base = checker(info);
    let full = full_rect(info);
    let op = stroke(2, vec![Point { x: 5, y: 5 }], 3.0);
    let mut indices = base.clone();
    let changed = replay::replay(&[op], info, full, &mut indices, &mut no_fused).unwrap();
    assert!(changed > 0);
    assert_ne!(indices, base);
}

#[test]
fn op_variant_reset_region_restores_the_fused_ancestor() {
    // A chain fused -> ops -> ops: the reset must read the fused root's map,
    // never the op-bearing parent's.
    let info = info();
    let fused_root = checker(info);
    let full = full_rect(info);

    let mut parent = fused_root.clone();
    let stroke_on_parent = stroke(5, vec![Point { x: 2, y: 2 }, Point { x: 30, y: 18 }], 6.0);
    replay::replay(&[stroke_on_parent], info, full, &mut parent, &mut no_fused).unwrap();
    assert_ne!(
        parent, fused_root,
        "the parent must actually differ from the fused root"
    );

    let rect = Rect {
        x: 1,
        y: 1,
        width: 4,
        height: 4,
    };
    let mut working = parent.clone();
    replay::replay(
        &[reset(rect)],
        info,
        full,
        &mut working,
        &mut fused_from(&fused_root, info),
    )
    .unwrap();

    for y in rect.y..rect.y + rect.height {
        for x in rect.x..rect.x + rect.width {
            let idx = (y * info.width() + x) as usize;
            assert_eq!(
                working[idx], fused_root[idx],
                "reset must restore the fused root's value at ({x},{y})"
            );
        }
    }
}

#[test]
fn op_variant_flood_fill_is_refused() {
    let info = info();
    let full = full_rect(info);
    let mut indices = checker(info);
    let op = Op::FloodFill {
        seed: Point { x: 2, y: 2 },
        source_id: 0,
        tolerance: 0.1,
    };
    match replay::replay(&[op], info, full, &mut indices, &mut no_fused) {
        Err((0, RetouchError::FloodFillNotReplayable)) => {}
        other => panic!("expected (0, FloodFillNotReplayable), got {other:?}"),
    }
}

#[test]
fn op_variant_refuse_is_refused() {
    let info = info();
    let full = full_rect(info);
    let mut indices = checker(info);
    let op = Op::Refuse {
        algorithm: "laplacian".to_string(),
        params: focusforge_core::params::ParamValues::new(),
    };
    match replay::replay(&[op], info, full, &mut indices, &mut no_fused) {
        Err((0, RetouchError::RefuseNotReplayable)) => {}
        other => panic!("expected (0, RefuseNotReplayable), got {other:?}"),
    }
}

#[test]
fn out_of_range_source_id_is_refused() {
    let info = info();
    let full = full_rect(info);
    let base = checker(info);
    let valid = stroke(1, vec![Point { x: 2, y: 2 }], 2.0);
    let invalid = stroke(NO_SOURCE, vec![Point { x: 3, y: 3 }], 2.0);
    let mut indices = base.clone();
    match replay::replay(&[valid, invalid], info, full, &mut indices, &mut no_fused) {
        Err((
            1,
            RetouchError::SourceOutOfRange {
                source_id: NO_SOURCE,
                ..
            },
        )) => {}
        other => panic!("expected (1, SourceOutOfRange), got {other:?}"),
    }
    assert_eq!(
        indices, base,
        "no pixel may be written before a later op's refusal"
    );
}

#[test]
fn out_of_bounds_geometry_is_refused() {
    let info = info();
    let full = full_rect(info);
    let base = checker(info);

    let partly_off = reset(Rect {
        x: info.width() - 2,
        y: 0,
        width: 5,
        height: 3,
    });
    match footprint(&partly_off, info) {
        Err(RetouchError::RegionOutsideMap { .. }) => {}
        other => panic!("expected RegionOutsideMap for a partly-off rect, got {other:?}"),
    }

    let zero_rect = reset(Rect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    });
    match footprint(&zero_rect, info) {
        Err(RetouchError::RegionOutsideMap { .. }) => {}
        other => panic!("expected RegionOutsideMap for a zero-size rect, got {other:?}"),
    }

    let seed_negative = Op::FloodFill {
        seed: Point { x: -1, y: 0 },
        source_id: 0,
        tolerance: 0.1,
    };
    match footprint(&seed_negative, info) {
        Err(RetouchError::SeedOutsideMap { .. }) => {}
        other => panic!("expected SeedOutsideMap for seed (-1, 0), got {other:?}"),
    }

    let seed_over = Op::FloodFill {
        seed: Point {
            x: info.width() as i32,
            y: 0,
        },
        source_id: 0,
        tolerance: 0.1,
    };
    match footprint(&seed_over, info) {
        Err(RetouchError::SeedOutsideMap { .. }) => {}
        other => panic!("expected SeedOutsideMap for seed (width, 0), got {other:?}"),
    }

    let partial_stroke = stroke(
        2,
        vec![Point {
            x: info.width() as i32 - 1,
            y: 5,
        }],
        3.0,
    );
    let mut applied = base.clone();
    let changed =
        replay::replay(&[partial_stroke], info, full, &mut applied, &mut no_fused).unwrap();
    assert!(
        changed > 0,
        "the on-map portion of a partly-off stroke must still apply"
    );

    let off_stroke = stroke(2, vec![Point { x: -100, y: -100 }], 3.0);
    let mut untouched = base.clone();
    let changed_off =
        replay::replay(&[off_stroke], info, full, &mut untouched, &mut no_fused).unwrap();
    assert_eq!(changed_off, 0);
    assert_eq!(untouched, base);
}

#[test]
fn empty_op_list_is_the_identity() {
    let info = info();
    let full = full_rect(info);
    let base = checker(info);
    let mut indices = base.clone();
    let changed = replay::replay(&[], info, full, &mut indices, &mut no_fused).unwrap();
    assert_eq!(changed, 0);
    assert_eq!(indices, base);

    let tile = replay::replay_tile(&[], info).unwrap();
    assert_eq!(
        tile,
        Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 1
        }
    );
}
