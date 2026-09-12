// SPDX-License-Identifier: GPL-3.0-or-later

//! The pure tiling arithmetic over a frame extent: no I/O, unit-testable on
//! its own. [`TilePlan::new`] partitions `width x height` into interior
//! rectangles that tile the extent exactly once, each grown by a margin on
//! every side (clamped to the extent) to form the *read* rectangle a
//! [`crate::algorithm::TileReader`] is asked for.
//!
//! Memory formula (restated at the scheduler, which is what actually bounds
//! it): `frames x tile_w x tile_h x spp x 2 B x in_flight` -- the bytes held
//! by `in_flight` simultaneously-live frame-tile sets, each `tile_w x tile_h`
//! (the *read* rect, not the interior) across every frame, `u16` samples.

use crate::tile::{Rect, TileError};

/// The default read-rect size a [`TileScheduler`](super::scheduler::TileScheduler)
/// uses when a caller does not override it.
pub const DEFAULT_TILE_SIZE: u32 = 512;

/// The pyramid-depth cap: exists so `1u32 << levels` in
/// [`margin_for_levels`] cannot overflow.
pub const MAX_LEVELS: u32 = 16;

/// The overlap (margin, in pixels, on every side that has a neighbour) a
/// tiling plan reserves for `levels` pyramid levels.
///
/// Returns the **measured** margin -- the number of pixels at which a
/// tile's interior is independent of everything outside its read rect to
/// within 1 `u16` LSB -- rounded up to a multiple of `2^levels`:
///
/// - `measured` is [`crate::fuse::pyramid::MEASURED_MARGIN`]'s
///   measurement of exactly the criterion this margin is supposed to
///   deliver: a worst-case, full-scale difference everywhere outside a
///   window moves nothing inside that window, inset by the margin, by more
///   than 1 LSB. The table lives beside the test that measures it
///   (`fuse::pyramid::tests::stated_margin_matches_the_measurement`) rather
///   than being duplicated here, so there is exactly one source of truth.
/// - above the table's range, `analytic = 4 * (2^levels - 1)` -- the reach
///   of a build-plus-collapse round trip for the 5-tap binomial
///   kernel, 508 px at `levels = 7`, and **not** the naive
///   `2^levels`, which is the one-tap-per-level idealization and
///   understates the true dependency of a multi-tap kernel.
///
/// `levels` above [`MAX_LEVELS`] is clamped rather than rejected here (a
/// `const fn` cannot call `Ord::min`); [`TilePlan::new`] is what actually
/// refuses an over-deep request, with [`TileError::LevelsTooDeep`].
pub const fn margin_for_levels(levels: u32) -> u32 {
    let levels = if levels > MAX_LEVELS {
        MAX_LEVELS
    } else {
        levels
    };
    4 * ((1u32 << levels) - 1)
}

/// One tile of a [`TilePlan`]: the interior rectangle this tile owns in the
/// output (no overlap with any other tile's interior), and the read
/// rectangle -- the interior grown by the plan's margin on every side and
/// clamped to the extent -- that must be read from every frame to fuse that
/// interior without a seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedTile {
    index: usize,
    interior: Rect,
    read: Rect,
}

impl PlannedTile {
    /// This tile's position in [`TilePlan::tiles`], stable for the lifetime
    /// of the plan.
    pub fn index(&self) -> usize {
        self.index
    }

    /// The rectangle this tile alone owns in the assembled output. Every
    /// tile's interior partitions the extent: no gap, no overlap.
    pub fn interior(&self) -> Rect {
        self.interior
    }

    /// The rectangle that must be read from every frame to fuse
    /// [`Self::interior`] without a seam -- the interior grown by the
    /// plan's margin and clamped to the extent, so it is always legal for
    /// the never-clamping `read_tile_into`.
    pub fn read(&self) -> Rect {
        self.read
    }
}

/// A tiling plan over one frame extent: tile size, pyramid-level overlap,
/// and the resulting interior/read rectangles, ragged at the right and
/// bottom edges rather than padded past the frame.
#[derive(Debug, Clone)]
pub struct TilePlan {
    tile_size: u32,
    levels: u32,
    margin: u32,
    tiles: Vec<PlannedTile>,
}

impl TilePlan {
    /// Build a plan over `width x height`, with `tile_size` interpreted as
    /// the size of the *read* rect (so `interior_size = tile_size - 2 *
    /// margin`), and `levels` the pyramid depth `margin_for_levels`
    /// converts to an overlap.
    ///
    /// Refuses rather than silently adjusting: a zero extent is
    /// `EmptyExtent`, `tile_size == 0` is `TileSizeZero`, `levels >
    /// MAX_LEVELS` is `LevelsTooDeep`, and a margin that leaves no interior
    /// (`tile_size <= 2 * margin`) is `NoInterior`, naming the smallest tile
    /// size that would work (`2 * margin + 1`).
    pub fn new(width: u32, height: u32, tile_size: u32, levels: u32) -> Result<Self, TileError> {
        if width == 0 || height == 0 {
            return Err(TileError::EmptyExtent { width, height });
        }
        if tile_size == 0 {
            return Err(TileError::TileSizeZero);
        }
        if levels > MAX_LEVELS {
            return Err(TileError::LevelsTooDeep {
                levels,
                max: MAX_LEVELS,
            });
        }

        let margin = margin_for_levels(levels);
        let interior_size = tile_size.saturating_sub(2 * margin);
        if interior_size == 0 {
            return Err(TileError::NoInterior {
                tile_size,
                margin,
                levels,
                min_tile_size: 2 * margin + 1,
            });
        }

        let mut tiles = Vec::new();
        let mut index = 0usize;
        let mut y = 0u32;
        while y < height {
            let h = interior_size.min(height - y);
            let mut x = 0u32;
            while x < width {
                let w = interior_size.min(width - x);
                let interior = Rect {
                    x,
                    y,
                    width: w,
                    height: h,
                };
                let read_x = x.saturating_sub(margin);
                let read_y = y.saturating_sub(margin);
                let read_x_end = x.saturating_add(w).saturating_add(margin).min(width);
                let read_y_end = y.saturating_add(h).saturating_add(margin).min(height);
                let read = Rect {
                    x: read_x,
                    y: read_y,
                    width: read_x_end - read_x,
                    height: read_y_end - read_y,
                };
                tiles.push(PlannedTile {
                    index,
                    interior,
                    read,
                });
                index += 1;
                x += w;
            }
            y += h;
        }

        Ok(TilePlan {
            tile_size,
            levels,
            margin,
            tiles,
        })
    }

    pub fn tiles(&self) -> &[PlannedTile] {
        &self.tiles
    }

    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    pub fn tile_size(&self) -> u32 {
        self.tile_size
    }

    pub fn margin(&self) -> u32 {
        self.margin
    }

    pub fn levels(&self) -> u32 {
        self.levels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interiors_partition_the_extent() {
        let plan = TilePlan::new(512, 512, 200, 3).unwrap();
        assert_eq!(plan.margin(), 28);
        assert_eq!(plan.len(), 16);

        let mut coverage = vec![0u8; 512 * 512];
        for tile in plan.tiles() {
            let r = tile.interior();
            for y in r.y..(r.y + r.height) {
                for x in r.x..(r.x + r.width) {
                    coverage[(y * 512 + x) as usize] += 1;
                }
            }
        }
        assert!(
            coverage.iter().all(|&c| c == 1),
            "every pixel must be covered by exactly one interior"
        );
    }

    #[test]
    fn edge_tiles_are_ragged() {
        let plan = TilePlan::new(512, 512, 200, 3).unwrap();
        // interior_size = 144; 512 = 144*3 + 80.
        let last_col = plan
            .tiles()
            .iter()
            .find(|t| t.interior().x == 144 * 3)
            .expect("a tile must start the last ragged column");
        assert_eq!(last_col.interior().width, 80);
        let last_row = plan
            .tiles()
            .iter()
            .find(|t| t.interior().y == 144 * 3)
            .expect("a tile must start the last ragged row");
        assert_eq!(last_row.interior().height, 80);
    }

    #[test]
    fn overlap_covers_the_measured_margin() {
        // n = 0 (no pyramid) needs no overlap at all: margin_for_levels(0)
        // == 0, correctly below the degenerate 2^0 == 1. For every n >= 1
        // the analytic margin is at least the superseded 2^n.
        assert_eq!(margin_for_levels(0), 0);
        for n in 1..=8u32 {
            let margin = margin_for_levels(n);
            assert_eq!(margin, 4 * ((1u32 << n) - 1));
            assert!(
                margin >= (1u32 << n),
                "margin must be at least the superseded 2^N: n={n}, margin={margin}"
            );
        }
        let plan = TilePlan::new(512, 512, 200, 3).unwrap();
        assert_eq!(plan.margin(), margin_for_levels(3));
    }

    #[test]
    fn no_interior_is_refused_by_name() {
        let err = TilePlan::new(512, 512, 512, 7).unwrap_err();
        match err {
            TileError::NoInterior {
                tile_size: 512,
                margin: 508,
                levels: 7,
                min_tile_size: 1017,
            } => {}
            other => panic!("expected NoInterior naming 512/508/7/1017, got {other:?}"),
        }
    }

    #[test]
    fn tile_larger_than_the_frame_is_one_tile() {
        let plan = TilePlan::new(100, 100, 200, 0).unwrap();
        assert_eq!(plan.len(), 1);
        let tile = &plan.tiles()[0];
        assert_eq!(
            tile.interior(),
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100
            }
        );
        assert_eq!(
            tile.read(),
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100
            }
        );
    }

    #[test]
    fn one_pixel_frame_yields_one_tile() {
        let plan = TilePlan::new(1, 1, 512, 0).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(
            plan.tiles()[0].interior(),
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1
            }
        );
    }

    #[test]
    fn zero_extent_is_refused() {
        match TilePlan::new(0, 10, 512, 0) {
            Err(TileError::EmptyExtent {
                width: 0,
                height: 10,
            }) => {}
            other => panic!("expected EmptyExtent, got {other:?}"),
        }
        match TilePlan::new(10, 0, 512, 0) {
            Err(TileError::EmptyExtent {
                width: 10,
                height: 0,
            }) => {}
            other => panic!("expected EmptyExtent, got {other:?}"),
        }
    }

    #[test]
    fn read_rects_stay_in_bounds() {
        let plan = TilePlan::new(512, 512, 200, 3).unwrap();
        for tile in plan.tiles() {
            let r = tile.read();
            assert!(r.width > 0 && r.height > 0);
            assert!(r.x + r.width <= 512);
            assert!(r.y + r.height <= 512);
            // The read rect must contain the interior it grows.
            let i = tile.interior();
            assert!(r.x <= i.x && r.y <= i.y);
            assert!(r.x + r.width >= i.x + i.width);
            assert!(r.y + r.height >= i.y + i.height);
        }
    }
}
