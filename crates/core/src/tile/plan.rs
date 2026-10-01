// SPDX-License-Identifier: GPL-3.0-or-later

//! The pure tiling arithmetic over a frame extent: no I/O, unit-testable on
//! its own. [`TilePlan::new`] partitions `width x height` into interior
//! rectangles that tile the extent exactly once, each grown by a margin on
//! every side (clamped to the extent) to form the *read* rectangle a
//! [`crate::algorithm::TileReader`] is asked for.
//!
//! Memory formula (restated at the scheduler, which is what actually bounds
//! it): `tile_w x tile_h x (spp x 2 B + 1 B + working) x in_flight` -- the
//! bytes held by `in_flight` simultaneously-live tiles, each `tile_w x tile_h`
//! (the *read* rect, not the interior): one `u16` frame tile, one coverage
//! mask and the method's working bytes. No term scales with the frame count.

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
/// Returns the **measured** margin -- the pixel count at which a tile's
/// interior is independent of everything outside its read rect to within
/// 1 `u16` LSB -- rounded up to a multiple of `2^levels`, so a tile's read
/// origin lands on the pyramid's own decimation grid.
///
/// [`crate::fuse::pyramid::MEASURED_MARGIN`] is the measured table, pinned by
/// `fuse::pyramid::tests::stated_margin_matches_the_measurement`; past its range the margin
/// falls back to `4 * (2^levels - 1)`, the reach of a build-plus-collapse round trip for a 5-tap
/// binomial kernel. The measured entry is **not** capped by that analytic reach -- at `levels =
/// 7` it exceeds it (512 against 508): the analytic figure is the reach in an arbitrarily large
/// buffer, while the measured one is taken where the tails of two opposite margins overlap.
///
/// **This is the one place the overlap constant lives.** `levels` above
/// [`MAX_LEVELS`] is clamped rather than rejected here (a `const fn` cannot
/// call `Ord::min`, hence the `if` below rather than the trait method);
/// [`TilePlan::new`] is what actually refuses an over-deep request, with
/// [`TileError::LevelsTooDeep`].
pub const fn margin_for_levels(levels: u32) -> u32 {
    let levels = if levels > MAX_LEVELS {
        MAX_LEVELS
    } else {
        levels
    };
    let measured_table = crate::fuse::pyramid::MEASURED_MARGIN;
    let bound = if (levels as usize) < measured_table.len() {
        measured_table[levels as usize]
    } else {
        // Past the measured range, fall back to the analytic reach.
        4 * ((1u32 << levels) - 1)
    };
    // Round up to the decimation grid. `levels == 0` aligns to 1, i.e. no
    // change; `bound` is at most `4 * (2^MAX_LEVELS - 1)`, so neither the
    // addition nor the product can overflow `u32`.
    let alignment = 1u32 << levels;
    bound.div_ceil(alignment) * alignment
}

/// The smallest `tile_size` that plans at `levels` levels with no reach beyond
/// the pyramid margin -- [`TileSupport::min_tile_size`] of `levels.into()`.
pub const fn min_tile_size_for_levels(levels: u32) -> u32 {
    TileSupport {
        reach_px: 0,
        decimation_levels: levels,
    }
    .min_tile_size()
}

/// The largest level count `n <= MAX_LEVELS` for which
/// [`min_tile_size_for_levels`]`(n) <= tile_size`, i.e. the deepest pyramid
/// a tile of this size can plan. A `while` loop rather than
/// `(0..=MAX_LEVELS).rev().find(...)` because a `const fn` cannot call an
/// iterator adaptor: `max_levels_for_tile(512) == 5` and
/// `max_levels_for_tile(832) == 6`.
pub const fn max_levels_for_tile(tile_size: u32) -> u32 {
    let mut n = 0u32;
    let mut best = 0u32;
    while n <= MAX_LEVELS {
        if min_tile_size_for_levels(n) <= tile_size {
            best = n;
        }
        n += 1;
    }
    best
}

/// What a method needs around a tile: the pixel distance its output at a
/// pixel depends on (`reach_px`) and the depth of its decimation grid
/// (`decimation_levels`), the grid every read origin must sit on.
///
/// `From<u32>` takes a pyramid depth alone, with no reach beyond the
/// measured pyramid margin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileSupport {
    pub reach_px: u32,
    pub decimation_levels: u32,
}

impl From<u32> for TileSupport {
    fn from(decimation_levels: u32) -> Self {
        TileSupport {
            reach_px: 0,
            decimation_levels,
        }
    }
}

impl TileSupport {
    /// The overlap on every side that has a neighbour:
    /// `max(margin_for_levels(levels), reach_px)` rounded up to `2^levels`,
    /// so a read origin stays on the decimation grid.
    pub const fn margin(self) -> u32 {
        let pyramid = margin_for_levels(self.decimation_levels);
        let bound = if self.reach_px > pyramid {
            self.reach_px
        } else {
            pyramid
        };
        let alignment = 1u32 << clamp_levels(self.decimation_levels);
        bound.div_ceil(alignment) * alignment
    }

    /// The smallest `tile_size` that plans: `max(2 * margin + 2^levels,
    /// 4 * margin_for_levels(levels) + 64)`. The second term is the geometry
    /// the pyramid margin was measured in, so it stays on the measured
    /// margin whatever the reach; it does not apply at `levels == 0`.
    pub const fn min_tile_size(self) -> u32 {
        let levels = clamp_levels(self.decimation_levels);
        let with_alignment = 2 * self.margin() + (1u32 << levels);
        let measured = margin_for_levels(levels);
        if measured == 0 {
            return with_alignment;
        }
        let validity_range = 4 * measured + 64;
        if with_alignment > validity_range {
            with_alignment
        } else {
            validity_range
        }
    }

    /// The tile size used when a caller states none:
    /// `max(DEFAULT_TILE_SIZE, 4 * margin + 64)`.
    pub const fn default_tile_size(self) -> u32 {
        let wide = 4 * self.margin() + 64;
        if wide > DEFAULT_TILE_SIZE {
            wide
        } else {
            DEFAULT_TILE_SIZE
        }
    }
}

const fn clamp_levels(levels: u32) -> u32 {
    if levels > MAX_LEVELS {
        MAX_LEVELS
    } else {
        levels
    }
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

/// Partitions one axis of length `extent` into strides of `interior_size`,
/// returning `(start, size)` pairs that cover `0..extent` with no gap and
/// no overlap.
///
/// A trailing remainder smaller than `margin` is **absorbed into the
/// previous stride**, so a read rect never exceeds `tile_size` and an
/// unabsorbed trailing read rect is at least `2 * margin` long (enough for
/// every level count the margin serves). At `8192x5464`, tile 512, 5 levels
/// the last interior row would be 24 px, whose 120 px read rect carries only
/// 4 levels. `margin == 0` never merges.
fn axis_partition(extent: u32, interior_size: u32, margin: u32) -> Vec<(u32, u32)> {
    let mut strides = Vec::new();
    let mut pos = 0u32;
    while pos < extent {
        let size = interior_size.min(extent - pos);
        strides.push((pos, size));
        pos += size;
    }
    if strides.len() > 1 {
        let last = strides.len() - 1;
        if strides[last].1 < margin {
            let (_, remainder) = strides.remove(last);
            strides[last - 1].1 += remainder;
        }
    }
    strides
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
    /// margin`), and `support` the method's reach and decimation depth
    /// ([`TileSupport::margin`] converts them to an overlap; a bare `u32`
    /// is a pyramid depth with no extra reach).
    ///
    /// Refuses rather than silently adjusting: a zero extent is
    /// `EmptyExtent`, `tile_size == 0` is `TileSizeZero`, `levels >
    /// MAX_LEVELS` is `LevelsTooDeep`, and a tile too small for the margin's
    /// own validity range is `NoInterior`, naming
    /// [`TileSupport::min_tile_size`] -- never `2 * margin + 2^levels` alone.
    /// `crate::fuse::pyramid::MEASURED_MARGIN` is only established in the
    /// geometry it was measured in, a buffer of `4 * margin + 64` per side;
    /// a smaller buffer is, by the same measurement's tails-overlap
    /// argument, the direction that needs *more* margin, not less, so a
    /// plan may not be smaller than that geometry either. The `4 * margin +
    /// 64` term only applies where a margin exists: at `levels == 0` there
    /// is no pyramid and no measurement geometry to respect, so the floor
    /// is skipped and `min_tile_size` is just `1`.
    ///
    /// **This refuses 512 px at 6 levels**: the margin table is
    /// established only up to what `4 * margin + 64` covers, and 512 px
    /// falls short of it at 6 levels
    /// (`tests::a_512_tile_at_6_levels_is_refused`). **The shipped default
    /// is unaffected**: 512 px at 5 levels needs 448 px (`4 * 96 + 64`) and
    /// has 512.
    ///
    /// # The decimation grid
    ///
    /// Every tile's read origin is a multiple of `2^levels`, so all tiles
    /// decimate in the same dyadic phase. Otherwise adjacent tiles
    /// decimate on different phases -- a shift-variance seam that does not
    /// shrink as the margin grows and that no border rule can remove,
    /// because it is a property of *where* the pyramid samples, not of what
    /// it does at an edge.
    ///
    /// Two roundings together make that true, and both are needed:
    ///
    /// - the margin is rounded **up** to a multiple of `2^levels` (in
    ///   [`margin_for_levels`], so `TileError::NoInterior::margin` and
    ///   [`TilePlan::margin`] both report the effective overlap), and
    /// - the interior stride is rounded **down** to a multiple of
    ///   `2^levels`.
    ///
    /// A read origin is then `k * stride - margin`, a difference of two
    /// multiples of `2^levels`; the first row and column, whose read origin
    /// clamps to `0`, sit in that same dyadic phase rather than in one of
    /// their own. `levels == 0` aligns to `1`, i.e. no change. If the un-aligned
    /// interior is nonzero but rounds down to zero, that is `NoInterior`
    /// too. This is what makes a tiled-vs-single-tile comparison an
    /// exact equality rather than a tolerance nobody could otherwise
    /// derive, and
    /// `tests::read_origins_land_on_the_decimation_grid` is what holds it
    /// to that.
    pub fn new(
        width: u32,
        height: u32,
        tile_size: u32,
        support: impl Into<TileSupport>,
    ) -> Result<Self, TileError> {
        let support = support.into();
        let levels = support.decimation_levels;
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

        let margin = support.margin();
        // The smallest tile that actually plans. `2 * margin + alignment`
        // alone is not enough: the margin table
        // (`crate::fuse::pyramid::MEASURED_MARGIN`) is only established in
        // the geometry it was measured in, a buffer of `4 * margin + 64`
        // per side (`crate::fuse::pyramid::tests::leak_plane`), and a
        // smaller buffer is, by the same measurement's tails-overlap
        // argument, the direction that needs *more* margin, not less. The
        // `4 * margin + 64` floor only makes sense where a margin exists:
        // at `levels == 0` there is no pyramid and no measurement geometry
        // to respect, so it is skipped rather than forcing every 0-level
        // plan through a 64 px minimum. Both `NoInterior` arms name this
        // same number, so the advice is never a size that the next guard
        // refuses in turn.
        let alignment: u32 = 1u32 << levels;
        let min_tile_size = support.min_tile_size();
        if tile_size < min_tile_size {
            return Err(TileError::NoInterior {
                tile_size,
                margin,
                levels,
                min_tile_size,
            });
        }
        let raw_interior = tile_size.saturating_sub(2 * margin);
        // Round the interior stride down to the pyramid's own decimation
        // grid -- see this function's rustdoc. Unreachable given the
        // guard above (`tile_size >= min_tile_size >= 2 * margin +
        // alignment` forces `raw_interior >= alignment`, and therefore
        // `interior_size >= alignment > 0`), kept as a named guard rather
        // than an assertion so a future change to `min_tile_size` cannot
        // silently reopen a zero-interior plan.
        let interior_size = (raw_interior / alignment) * alignment;
        if interior_size == 0 {
            return Err(TileError::NoInterior {
                tile_size,
                margin,
                levels,
                min_tile_size,
            });
        }

        let xs = axis_partition(width, interior_size, margin);
        let ys = axis_partition(height, interior_size, margin);

        let mut tiles = Vec::new();
        let mut index = 0usize;
        for &(y, h) in &ys {
            for &(x, w) in &xs {
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
            }
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

    /// The floor `TilePlan::new` refuses below: the margin table is only
    /// established in the geometry it was measured in (`4 * margin + 64`
    /// per side), so a tile smaller than that -- even if `2 * margin +
    /// 2^levels` alone would leave a nonempty interior -- is refused.
    /// Recomputed from `margin_for_levels` so a re-measured margin cannot
    /// silently invalidate a test that pins a literal.
    fn min_tile_size_for(levels: u32) -> u32 {
        min_tile_size_for_levels(levels)
    }

    /// The interior stride `TilePlan::new` derives for a tile size and a
    /// level count, recomputed from the published margin so that a test
    /// pins the *arithmetic* rather than a literal that a re-measured
    /// margin would silently invalidate.
    fn interior_size_for(tile_size: u32, levels: u32) -> u32 {
        let alignment = 1u32 << levels;
        ((tile_size - 2 * margin_for_levels(levels)) / alignment) * alignment
    }

    #[test]
    fn interiors_partition_the_extent() {
        let plan = TilePlan::new(512, 512, 200, 3).unwrap();
        assert_eq!(plan.margin(), margin_for_levels(3));
        let interior = interior_size_for(200, 3);
        assert_eq!(plan.len(), (512u32.div_ceil(interior) as usize).pow(2));

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
        // Derived, never pinned: the interior stride is the aligned
        // `200 - 2 * margin_for_levels(3)`, and the frame's last row and
        // column are whatever is left of 512 after a whole number of those
        // strides -- ragged, not padded past the frame.
        let interior = interior_size_for(200, 3);
        let last_start = (512 / interior) * interior;
        let ragged = 512 - last_start;
        assert!(
            ragged > 0 && ragged < interior,
            "this fixture must produce a genuinely ragged edge: \
             interior={interior}, last_start={last_start}, ragged={ragged}"
        );
        let last_col = plan
            .tiles()
            .iter()
            .find(|t| t.interior().x == last_start)
            .expect("a tile must start the last ragged column");
        assert_eq!(last_col.interior().width, ragged);
        let last_row = plan
            .tiles()
            .iter()
            .find(|t| t.interior().y == last_start)
            .expect("a tile must start the last ragged row");
        assert_eq!(last_row.interior().height, ragged);
    }

    #[test]
    fn overlap_covers_the_measured_margin() {
        // n = 0 (no pyramid) needs no overlap at all: margin_for_levels(0)
        // == 0, correctly below the degenerate 2^0 == 1. For every n >= 1
        // the margin is the measured requirement
        // (`crate::fuse::pyramid::MEASURED_MARGIN`, or the analytic reach
        // past that table) rounded **up** to the decimation grid, so it
        // never falls below the measured number, it is itself a multiple of
        // 2^n, and it exceeds what it rounds by less than one alignment
        // step. It is deliberately *not* capped at the analytic reach: at
        // n = 7 the measured requirement is larger, because the analytic
        // figure describes an arbitrarily large buffer and a tile is not
        // one.
        assert_eq!(margin_for_levels(0), 0);
        for n in 1..=8u32 {
            let margin = margin_for_levels(n);
            let alignment = 1u32 << n;
            let bound = crate::fuse::pyramid::MEASURED_MARGIN
                .get(n as usize)
                .copied()
                .unwrap_or(4 * ((1u32 << n) - 1));
            assert!(
                margin >= bound,
                "margin must never fall below the measured requirement: \
                 n={n}, margin={margin}, bound={bound}"
            );
            assert!(
                margin < bound + alignment,
                "rounding up to the decimation grid must cost less than one \
                 alignment step: n={n}, margin={margin}, bound={bound}, alignment={alignment}"
            );
            assert_eq!(
                margin % alignment,
                0,
                "the margin must itself be on the decimation grid: n={n}, margin={margin}"
            );
            assert!(
                margin >= alignment,
                "margin must be at least the superseded 2^N: n={n}, margin={margin}"
            );
        }
        let plan = TilePlan::new(512, 512, 200, 3).unwrap();
        assert_eq!(plan.margin(), margin_for_levels(3));
    }

    #[test]
    fn no_interior_is_refused_by_name() {
        // levels = 7's measured margin (512) already exceeds a 512 px
        // tile on its own (2 * 512 > 512), and the validity-range floor
        // (`4 * margin + 64` = 2112) is stricter still. This is the whole
        // point of the test: read the margin through `margin_for_levels`
        // and the floor through `min_tile_size_for` (neither is a literal
        // that a re-measurement could silently invalidate) and confirm the
        // refusal still fires with the smallest workable tile size it
        // should name.
        let margin = margin_for_levels(7);
        let min_tile_size = min_tile_size_for(7);
        let err = TilePlan::new(512, 512, 512, 7).unwrap_err();
        match err {
            TileError::NoInterior {
                tile_size: 512,
                margin: m,
                levels: 7,
                min_tile_size: mts,
            } if m == margin && mts == min_tile_size => {}
            other => {
                panic!("expected NoInterior naming 512/{margin}/7/{min_tile_size}, got {other:?}")
            }
        }
        // The rendered message is the only thing a user of the 512/7 case
        // ever sees, so it is pinned here -- derived from the same numbers,
        // so a re-measured margin moves the text with it.
        assert_eq!(
            err.to_string(),
            format!(
                "tile_size 512 leaves no interior at margin {margin} for levels=7; \
                 use tile_size >= {min_tile_size}, or request fewer levels"
            )
        );
    }

    #[test]
    fn a_512_tile_at_6_levels_is_refused() {
        // The margin table is established in a buffer of `4 * margin + 64`
        // per side, which at levels=6 (margin 192) is 832 -- larger than
        // the 512 px tile `2 * margin + 2^levels` alone would allow (448);
        // 512 px is refused because it falls inside that gap, not inside
        // the established geometry.
        let margin = margin_for_levels(6);
        let min_tile_size = min_tile_size_for(6);
        let err = TilePlan::new(512, 512, 512, 6).unwrap_err();
        match err {
            TileError::NoInterior {
                tile_size: 512,
                margin: m,
                levels: 6,
                min_tile_size: mts,
            } if m == margin && mts == min_tile_size => {}
            other => {
                panic!("expected NoInterior naming 512/{margin}/6/{min_tile_size}, got {other:?}")
            }
        }
        assert!(
            TilePlan::new(512, 512, min_tile_size, 6).is_ok(),
            "the size the error names must actually plan"
        );
        // The rendered message, since it is the only thing a user of the
        // 512/6 case ever sees.
        assert_eq!(
            err.to_string(),
            format!(
                "tile_size 512 leaves no interior at margin {margin} for levels=6; \
                 use tile_size >= {min_tile_size}, or request fewer levels"
            )
        );
    }

    #[test]
    fn the_size_no_interior_names_actually_plans() {
        // Five is a default, never a cap, and the only thing
        // telling a user how to get 7 levels is `NoInterior`'s
        // `min_tile_size`. So: take that size from the error itself --
        // never a literal, or the advice and the guard can drift apart --
        // and plan at exactly it.
        let err = TilePlan::new(4096, 4096, 512, 7).unwrap_err();
        let TileError::NoInterior { min_tile_size, .. } = err else {
            panic!("512 px at levels=7 must be refused with NoInterior, got {err:?}");
        };
        let plan = TilePlan::new(4096, 4096, min_tile_size, 7).unwrap_or_else(|e| {
            panic!("the tile size NoInterior names ({min_tile_size}) must plan, got {e}")
        });
        assert_eq!(plan.levels(), 7);
        assert!(!plan.is_empty());
        let alignment = 1u32 << 7;
        for tile in plan.tiles() {
            assert!(tile.interior().width > 0 && tile.interior().height > 0);
            assert_eq!(tile.read().x % alignment, 0);
            assert_eq!(tile.read().y % alignment, 0);
        }
    }

    #[test]
    fn a_tile_the_old_rounding_guard_alone_caught_is_refused_by_name() {
        // `tile_size = 2 * margin + 1` gives the smallest nonzero raw
        // interior (`1` px), which rounds down to `0` on the decimation
        // grid -- the `interior_size == 0` case `TilePlan::new` guards
        // against.

        // At levels=5 this size is `193`, below the validity-range floor
        // `4 * margin + 64 = 448`, so `TilePlan::new` refuses it by
        // `NoInterior` before the alignment-rounds-to-zero guard is
        // reached; the between-448-and-832 case is
        // `tests::a_512_tile_at_6_levels_is_refused`.
        let levels = 5u32;
        let margin = margin_for_levels(levels);
        let alignment = 1u32 << levels;
        let tile_size = 2 * margin + 1;
        let min_tile_size = min_tile_size_for(levels);
        assert!(
            min_tile_size > 2 * margin + alignment,
            "this fixture assumes the floor is the binding constraint at levels={levels}"
        );
        match TilePlan::new(4096, 4096, tile_size, levels) {
            Err(TileError::NoInterior {
                tile_size: t,
                margin: m,
                levels: l,
                min_tile_size: mts,
            }) if t == tile_size && m == margin && l == levels && mts == min_tile_size => {
                assert!(TilePlan::new(4096, 4096, min_tile_size, levels).is_ok());
            }
            other => {
                panic!(
                    "expected NoInterior naming {tile_size}/{margin}/{levels}/{min_tile_size}, \
                     got {other:?}"
                )
            }
        }
    }

    #[test]
    fn read_origins_land_on_the_decimation_grid() {
        // The property holds so that *every* tile's read origin -- the
        // first one included -- is a multiple of `2^levels`; no two tiles
        // decimate in different dyadic phases, and a tiled-vs-single-tile
        // comparison is an equality. Aligning the stride alone is not
        // enough; the margin subtracted to derive the read origin has to
        // be on the grid too.
        for levels in 0..=7u32 {
            let margin = margin_for_levels(levels);
            let alignment = if levels == 0 { 1 } else { 1u32 << levels };
            // Four aligned interiors' worth of tile above the
            // validity-range floor, and a frame wide enough that several
            // tiles (plus a ragged edge) exist.
            let tile_size = min_tile_size_for(levels) + 4 * alignment;
            let interior = ((tile_size - 2 * margin) / alignment) * alignment;
            let width = 3 * interior + interior / 2 + 7;
            // `3 * interior + 3`, not `2 * interior + 3`: the ragged-last-row
            // absorption can merge a small trailing remainder into the
            // previous stride, which on
            // the original `2 * interior + 3` shrunk this axis to as few as
            // 2 strides at high `levels` -- below the `>= 8` this test needs.
            // Three full strides plus a small, mergeable remainder keeps at
            // least 3 strides on this axis (merged or not) at every level.
            let height = 3 * interior + 3;
            let plan = TilePlan::new(width, height, tile_size, levels)
                .unwrap_or_else(|e| panic!("plan must build at levels={levels}: {e}"));
            assert!(
                plan.len() >= 8,
                "levels={levels} must produce several tiles, got {}",
                plan.len()
            );
            for tile in plan.tiles() {
                let r = tile.read();
                assert_eq!(
                    r.x % alignment,
                    0,
                    "read origin x={} is off the 2^{levels}={alignment} decimation grid \
                     (tile {}, interior x={})",
                    r.x,
                    tile.index(),
                    tile.interior().x
                );
                assert_eq!(
                    r.y % alignment,
                    0,
                    "read origin y={} is off the 2^{levels}={alignment} decimation grid \
                     (tile {}, interior y={})",
                    r.y,
                    tile.index(),
                    tile.interior().y
                );
            }
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
    fn ragged_last_row_is_absorbed_into_previous_tile() {
        // The reference stack's own extent, at the shipped default tile
        // size and level count.
        // Before the absorption fix, the last interior row was 24 px tall
        // (5464 - 17*320) and its 120 px read rect gave
        // `max_levels(_, 120) == 4 < 5`, so `fuse::build` refused with
        // `LevelsTooDeep` -- unfusable by the default geometry, with no
        // tile size dodging it (`5464 == 2^3 * 683` has no multiple of 32
        // as a divisor). This test pins the fix's own verified numbers.
        let plan = TilePlan::new(8192, 5464, 512, 5)
            .expect("the reference stack's default geometry must now plan");
        let last_row = plan
            .tiles()
            .iter()
            .filter(|t| t.interior().x == 0)
            .max_by_key(|t| t.interior().y)
            .expect("at least one tile must start the last row");
        assert_eq!(
            last_row.interior().y,
            5120,
            "the absorbed last interior must start at y=5120"
        );
        assert_eq!(
            last_row.interior().y + last_row.interior().height,
            5464,
            "the last interior must still reach the frame's bottom edge"
        );
        assert_eq!(
            last_row.read().y,
            5024,
            "the read origin must be 5024 -- still a multiple of 32"
        );
        assert_eq!(last_row.read().y % 32, 0);
        assert_eq!(
            last_row.read().height,
            440,
            "the absorbed row's read rect must be 440 px tall"
        );
        assert!(
            crate::fuse::pyramid::max_levels(last_row.read().width, last_row.read().height) >= 5,
            "the absorbed geometry must support the shipped default level count"
        );

        // The x axis divides differently (8192 = 25*320 + 192, and 192 is
        // not < margin=96) so it is never absorbed -- both behaviours
        // covered by one plan.
        let last_col = plan
            .tiles()
            .iter()
            .filter(|t| t.interior().y == 0)
            .max_by_key(|t| t.interior().x)
            .expect("at least one tile must start the last column");
        assert_eq!(last_col.interior().x, 8000);
        assert_eq!(last_col.interior().width, 192);
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
    fn support(reach_px: u32, decimation_levels: u32) -> TileSupport {
        TileSupport {
            reach_px,
            decimation_levels,
        }
    }

    #[test]
    fn support_margin_min_and_default_follow_the_reach() {
        // Laplacian at 5 levels: reach 140 -> margin 160, minimum tile on
        // the measured margin (448), default 4 * 160 + 64.
        let laplacian = support(140, 5);
        assert_eq!(laplacian.margin(), 160);
        assert_eq!(laplacian.min_tile_size(), 448);
        assert_eq!(laplacian.default_tile_size(), 704);
        // Depth 3 and 4: 36 -> 40 and 68 -> 80.
        assert_eq!(support(36, 3).margin(), 40);
        assert_eq!(support(68, 4).margin(), 80);
        // A two-pixel grid: margin is the reach itself.
        assert_eq!(support(10, 1).margin(), 10);
        assert_eq!(support(14, 1).margin(), 14);
        assert_eq!(support(10, 1).default_tile_size(), DEFAULT_TILE_SIZE);
        // No reach: the measured pyramid margin and its minimum tile.
        for levels in 0..=7 {
            let plain = TileSupport::from(levels);
            assert_eq!(plain.margin(), margin_for_levels(levels));
            assert_eq!(plain.min_tile_size(), min_tile_size_for_levels(levels));
        }
        assert_eq!(TileSupport::from(5).default_tile_size(), DEFAULT_TILE_SIZE);
    }

    #[test]
    fn support_margin_covers_the_reach_on_the_decimation_grid() {
        for levels in 0..=8u32 {
            for reach in 0..=700u32 {
                let s = support(reach, levels);
                let margin = s.margin();
                assert!(
                    margin >= reach,
                    "levels={levels} reach={reach} margin={margin}"
                );
                assert!(margin >= margin_for_levels(levels));
                assert_eq!(margin % (1u32 << levels), 0);
                assert!(s.min_tile_size() > 2 * margin);
                assert!(s.default_tile_size() >= s.min_tile_size());
            }
        }
    }

    #[test]
    fn a_tile_below_the_support_minimum_is_refused_naming_it() {
        let s = support(140, 5);
        match TilePlan::new(2000, 2000, s.min_tile_size() - 1, s) {
            Err(TileError::NoInterior {
                margin: 160,
                min_tile_size: 448,
                ..
            }) => {}
            other => panic!("expected NoInterior naming margin 160 / minimum 448, got {other:?}"),
        }
        assert!(TilePlan::new(2000, 2000, 448, s).is_ok());
    }

    #[test]
    fn read_rects_never_exceed_the_tile_size() {
        for (reach, levels) in [(0, 0), (0, 3), (0, 5), (140, 5), (36, 3), (10, 1)] {
            let s = support(reach, levels);
            let tile_sizes = [
                s.min_tile_size(),
                s.min_tile_size() + 37,
                s.default_tile_size(),
            ];
            for tile_size in tile_sizes {
                for extent in 1..=1300u32 {
                    let plan = TilePlan::new(extent, extent, tile_size, s)
                        .unwrap_or_else(|e| panic!("extent {extent}: {e}"));
                    for tile in plan.tiles() {
                        let r = tile.read();
                        assert!(
                            r.width <= tile_size && r.height <= tile_size,
                            "reach={reach} levels={levels} tile={tile_size} extent={extent}: \
                             read {r:?}"
                        );
                        // Every read rect is deep enough for the pyramid the
                        // support's levels ask for.
                        if extent >= 4 << levels {
                            assert!(
                                crate::fuse::pyramid::max_levels(r.width, r.height) >= levels,
                                "reach={reach} levels={levels} extent={extent}: read {r:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}
