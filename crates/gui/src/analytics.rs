// SPDX-License-Identifier: GPL-3.0-or-later

//! The Analytics overlay's rows: one per frame in fuse order, with the
//! frame's sharp-area count. Plain Rust, no `use slint::` -- `crate::controller`
//! owns the state and `crate::bridge` maps `AnalyticsRow` onto the overlay.

use std::path::PathBuf;

use focusforge_core::io::open_frame_set;
use focusforge_core::params::ParamValues;
use focusforge_core::sharp_areas::{SharpAreasError, count_sharp_areas_reporting};
use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;
use focusforge_core::tile::{HostBudget, TileControl};
use focusforge_core::zmap::source_color;

use crate::images::file_name;

/// How far a row's count is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowValue {
    /// No tile has reported yet.
    Pending,
    /// Counted over the tiles read so far.
    Partial,
    /// The count is done.
    Final,
}

/// One overlay row.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyticsRow {
    pub chip: [u8; 3],
    pub name: String,
    /// The rail position in mm; empty when the frame has none.
    pub mm_text: String,
    /// `count` over the largest count, and 0 for every row when all counts are 0.
    pub fraction: f32,
    pub count: u32,
    pub value: RowValue,
}

/// What the overlay shows.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum AnalyticsView {
    /// No stack is open to count.
    #[default]
    Waiting,
    /// The stack's rows while its count runs, each pending or partial.
    Counting(Vec<AnalyticsRow>),
    /// The finished count.
    Rows(Vec<AnalyticsRow>),
    /// The count failed; no rows are shown.
    Failed(String),
}

/// How hard the live count works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountPriority {
    /// All the host's workers.
    Normal,
    /// One worker.
    Low,
    /// Waiting for a stacking or export job to end.
    Held,
}

/// Why a count produced no counts.
#[derive(Debug, PartialEq)]
pub(crate) enum CountError {
    /// The control asked the count to stop; the result is not shown.
    Stopped,
    Failed(String),
}

/// One row per frame of `frames`, with the matching entry of `counts`, all
/// in the same `value` state.
pub(crate) fn build_rows(frames: &[PathBuf], counts: &[u32], value: RowValue) -> Vec<AnalyticsRow> {
    let total = frames.len().max(1) as u32;
    let largest = counts.iter().copied().max().unwrap_or(0);
    frames
        .iter()
        .zip(counts)
        .enumerate()
        .map(|(i, (path, &count))| AnalyticsRow {
            chip: source_color(i as u16, total),
            name: file_name(path),
            mm_text: String::new(),
            fraction: if largest == 0 {
                0.0
            } else {
                count as f32 / largest as f32
            },
            count,
            value,
        })
        .collect()
}

/// One pending row per frame of `frames`.
pub(crate) fn pending_rows(frames: &[PathBuf]) -> Vec<AnalyticsRow> {
    build_rows(frames, &vec![0; frames.len()], RowValue::Pending)
}

/// The per-frame sharp-area counts of `frames`, by the same two core calls
/// and defaults as the command line, read in tiles of `tile_size` (the
/// command line's default when `None`);
/// `on_tile` is given the counts so far after each tile.
pub(crate) fn count_frames(
    frames: &[PathBuf],
    tile_size: Option<u32>,
    control: &dyn TileControl,
    on_tile: &mut dyn FnMut(&[u32]),
) -> Result<Vec<u32>, CountError> {
    let set = open_frame_set(
        frames,
        HostBudget::host_derived(&focusforge_core::host::snapshot()),
    )
    .map_err(|err| CountError::Failed(err.to_string()))?;
    let reader = set.reader.as_ref();
    count_sharp_areas_reporting(
        reader,
        reader.len(),
        reader.dimensions(),
        reader.frame(0).samples_per_pixel,
        &ParamValues::new(),
        tile_size.unwrap_or(DEFAULT_TILE_SIZE),
        control,
        on_tile,
    )
    .map(|areas| areas.counts)
    .map_err(|err| match err {
        SharpAreasError::Stopped => CountError::Stopped,
        other => CountError::Failed(other.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(n: usize) -> Vec<PathBuf> {
        (0..n)
            .map(|i| PathBuf::from(format!("/frames/{i:02}.tif")))
            .collect()
    }

    #[test]
    fn rows_carry_chip_name_and_fraction_of_the_largest_count() {
        let rows = build_rows(&frames(3), &[2, 8, 4], RowValue::Final);
        let fractions: Vec<f32> = rows.iter().map(|r| r.fraction).collect();
        assert_eq!(fractions, [0.25, 1.0, 0.5]);
        assert_eq!(rows[1].name, "01.tif");
        assert_eq!(rows[2].chip, source_color(2, 3));
        assert!(rows.iter().all(|r| r.mm_text.is_empty()));
    }

    #[test]
    fn all_zero_counts_leave_every_bar_empty() {
        let rows = build_rows(&frames(3), &[0, 0, 0], RowValue::Final);
        assert!(rows.iter().all(|r| r.fraction == 0.0 && r.count == 0));
    }
}
