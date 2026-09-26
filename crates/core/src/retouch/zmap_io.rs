// SPDX-License-Identifier: GPL-3.0-or-later

//! The file-facing half of an edit session: unioning an op list's
//! footprints into one resident tile, and streaming base-plus-session back
//! out to a [`ZMapWriter`]. Kept out of [`super::session`] so `EditSession`
//! itself stays free of I/O.

use std::io::{Seek, Write};

use crate::project::Op;
use crate::tile::Rect;
use crate::zmap::{ZMapError, ZMapInfo, ZMapReader, ZMapWriter};

use super::{EditSession, RetouchError, brush};

/// The union of every op's footprint over `info`. `Err` carries the
/// 0-based index of the first op that refuses, before any tile is read or
/// written. `Ok` of a 1x1 rect at the origin when no op has a footprint --
/// an empty list, or one whose every op lies entirely off the map.
pub fn session_tile(ops: &[Op], info: ZMapInfo) -> Result<Rect, (usize, RetouchError)> {
    let mut union: Option<Rect> = None;
    for (index, op) in ops.iter().enumerate() {
        let footprint = brush::footprint(op, info).map_err(|err| (index, err))?;
        if let Some(rect) = footprint {
            union = Some(match union {
                Some(existing) => union_rect(existing, rect),
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

fn union_rect(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.width).max(b.x + b.width);
    let y1 = (a.y + a.height).max(b.y + b.height);
    Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    }
}

/// Streams `base` in full-width bands of `rows_per_pass` rows, splicing in
/// `session`'s tile wherever a band overlaps it, and writes each band to
/// `out`. Returns how many pixels of the output differ from `base`.
/// `rows_per_pass == 0` is [`ZMapError::RowsPerPassZero`], as in
/// [`ZMapReader::validate_all`].
///
/// Precondition: `session` was opened over `base`'s own [`ZMapInfo`].
/// `EditSession` exposes no `info()` to check this against; the caller
/// builds both from the same reader, which is what makes it hold.
pub fn write_edited<W: Write + Seek>(
    base: &ZMapReader,
    session: &EditSession,
    rows_per_pass: u32,
    out: &mut ZMapWriter<W>,
) -> Result<u64, ZMapError> {
    if rows_per_pass == 0 {
        return Err(ZMapError::RowsPerPassZero);
    }
    let info = base.info();
    let width = info.width();
    let height = info.height();
    let tile = session.tile();
    let session_indices = session.indices();

    let mut buf = vec![0u16; width as usize * rows_per_pass as usize];
    let mut changed = 0u64;
    let mut y = 0u32;
    while y < height {
        let band_height = rows_per_pass.min(height - y);
        let band = Rect {
            x: 0,
            y,
            width,
            height: band_height,
        };
        let len = width as usize * band_height as usize;
        base.read_tile_into(band, &mut buf[..len])?;

        if let Some(overlap) = brush::intersect(band, tile) {
            for row in overlap.y..overlap.y + overlap.height {
                let band_start = ((row - band.y) * width + overlap.x) as usize;
                let session_start = ((row - tile.y) * tile.width + (overlap.x - tile.x)) as usize;
                for col in 0..overlap.width as usize {
                    let new_index = session_indices[session_start + col];
                    let slot = &mut buf[band_start + col];
                    if *slot != new_index {
                        *slot = new_index;
                        changed += 1;
                    }
                }
            }
        }

        out.write_tile(band, &buf[..len])?;
        y += band_height;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Point;

    fn info_37x23() -> ZMapInfo {
        ZMapInfo::new(37, 23, 10).unwrap()
    }

    fn checker_indices(info: ZMapInfo) -> Vec<u16> {
        (0..info.height())
            .flat_map(|y| (0..info.width()).map(move |x| ((x + y) % info.frame_count()) as u16))
            .collect()
    }

    fn stroke(source_id: u16, path: Vec<Point>, radius: f32) -> Op {
        Op::BrushStroke {
            source_id,
            path,
            radius,
            softness: 0.0,
        }
    }

    fn write_base(
        dir: &std::path::Path,
        name: &str,
        info: ZMapInfo,
        indices: &[u16],
    ) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut writer = ZMapWriter::create(&path, info).unwrap();
        writer
            .write_tile(
                Rect {
                    x: 0,
                    y: 0,
                    width: info.width(),
                    height: info.height(),
                },
                indices,
            )
            .unwrap();
        writer.finish().unwrap();
        path
    }

    #[test]
    fn session_tile_unions_two_disjoint_strokes() {
        let info = info_37x23();
        let ops = vec![
            stroke(1, vec![Point { x: 2, y: 2 }], 1.0),
            stroke(2, vec![Point { x: 30, y: 18 }], 1.0),
        ];
        let tile = session_tile(&ops, info).unwrap();
        assert!(
            tile.x <= 1 && tile.y <= 1,
            "must cover the first stroke: {tile:?}"
        );
        assert!(
            tile.x + tile.width >= 31 && tile.y + tile.height >= 19,
            "must cover the second stroke: {tile:?}"
        );
    }

    #[test]
    fn session_tile_returns_the_first_refused_op_by_position() {
        let info = info_37x23();
        let ops = vec![
            stroke(1, vec![Point { x: 2, y: 2 }], 1.0),
            stroke(10, vec![Point { x: 3, y: 3 }], 1.0),
        ];
        match session_tile(&ops, info) {
            Err((
                1,
                RetouchError::SourceOutOfRange {
                    source_id: 10,
                    frame_count: 10,
                },
            )) => {}
            other => panic!("expected op #1 SourceOutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn session_tile_of_an_all_off_map_list_is_a_1x1_rect_at_the_origin() {
        let info = info_37x23();
        let ops = vec![stroke(1, vec![Point { x: -100, y: -100 }], 1.0)];
        let tile = session_tile(&ops, info).unwrap();
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

    #[test]
    fn write_edited_matches_a_whole_map_replay_at_any_band_height() {
        let dir = tempfile::tempdir().unwrap();
        let info = info_37x23();
        let base_indices = checker_indices(info);
        let base_path = write_base(dir.path(), "base.tif", info, &base_indices);

        let ops = vec![
            stroke(3, vec![Point { x: 0, y: 0 }, Point { x: 10, y: 5 }], 4.0),
            stroke(7, vec![Point { x: 20, y: 15 }], 3.0),
        ];
        let whole_rect = Rect {
            x: 0,
            y: 0,
            width: info.width(),
            height: info.height(),
        };
        let mut expected = base_indices.clone();
        for op in &ops {
            brush::apply(op, info, whole_rect, &mut expected).unwrap();
        }
        let expected_changed = base_indices
            .iter()
            .zip(&expected)
            .filter(|(a, b)| a != b)
            .count() as u64;

        for rows_per_pass in [3u32, 1000u32] {
            let reader = ZMapReader::open(&base_path).unwrap();
            let tile = session_tile(&ops, info).unwrap();
            let mut tile_indices = vec![0u16; info.tile_len(tile)];
            reader.read_tile_into(tile, &mut tile_indices).unwrap();
            let mut session = EditSession::new(info, tile, tile_indices).unwrap();
            for op in &ops {
                session.append(op.clone()).unwrap();
            }

            let out_path = dir.path().join(format!("out-{rows_per_pass}.tif"));
            let mut writer = ZMapWriter::create(&out_path, info).unwrap();
            let changed = write_edited(&reader, &session, rows_per_pass, &mut writer).unwrap();
            writer.finish().unwrap();
            assert_eq!(changed, expected_changed, "rows_per_pass={rows_per_pass}");

            let out_reader = ZMapReader::open(&out_path).unwrap();
            let mut out_indices = vec![0u16; info.tile_len(whole_rect)];
            out_reader
                .read_tile_into(whole_rect, &mut out_indices)
                .unwrap();
            assert_eq!(out_indices, expected, "rows_per_pass={rows_per_pass}");
        }
    }

    #[test]
    fn write_edited_refuses_rows_per_pass_zero() {
        let dir = tempfile::tempdir().unwrap();
        let info = ZMapInfo::new(4, 4, 2).unwrap();
        let indices = vec![0u16; 16];
        let base_path = write_base(dir.path(), "base.tif", info, &indices);
        let reader = ZMapReader::open(&base_path).unwrap();
        let session = EditSession::new(
            info,
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            indices,
        )
        .unwrap();
        let out_path = dir.path().join("out.tif");
        let mut writer = ZMapWriter::create(&out_path, info).unwrap();
        match write_edited(&reader, &session, 0, &mut writer) {
            Err(ZMapError::RowsPerPassZero) => {}
            other => panic!("expected RowsPerPassZero, got {other:?}"),
        }
    }
}
