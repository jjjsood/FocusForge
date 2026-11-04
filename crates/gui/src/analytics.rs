// SPDX-License-Identifier: GPL-3.0-or-later

//! The Analytics overlay's rows: one per frame in fuse order, with the
//! frame's sharp-area count. Plain Rust, no `use slint::` -- `crate::controller`
//! owns the state and `crate::bridge` maps `AnalyticsRow` onto the overlay.

use std::path::PathBuf;

use focusforge_core::io::open_frame_set;
use focusforge_core::params::ParamValues;
use focusforge_core::sharp_areas::{SharpAreasError, count_sharp_areas};
use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;
use focusforge_core::tile::{HostBudget, TileControl};
use focusforge_core::zmap::source_color;

use crate::images::file_name;

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
}

/// What the overlay shows.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum AnalyticsView {
    /// The count is running (or nothing was asked yet).
    #[default]
    Computing,
    Rows(Vec<AnalyticsRow>),
    Failed(String),
}

/// Why a count produced no counts.
#[derive(Debug, PartialEq)]
pub(crate) enum CountError {
    /// The control asked the count to stop; the result is not shown.
    Stopped,
    Failed(String),
}

/// One row per frame of `frames`, with the matching entry of `counts`.
pub(crate) fn build_rows(frames: &[PathBuf], counts: &[u32]) -> Vec<AnalyticsRow> {
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
        })
        .collect()
}

/// The per-frame sharp-area counts of `frames`, by the same two core calls
/// and defaults as the command line.
pub(crate) fn count_frames(
    frames: &[PathBuf],
    control: &dyn TileControl,
) -> Result<Vec<u32>, CountError> {
    let set = open_frame_set(
        frames,
        HostBudget::host_derived(&focusforge_core::host::snapshot()),
    )
    .map_err(|err| CountError::Failed(err.to_string()))?;
    let reader = set.reader.as_ref();
    count_sharp_areas(
        reader,
        reader.len(),
        reader.dimensions(),
        reader.frame(0).samples_per_pixel,
        &ParamValues::new(),
        DEFAULT_TILE_SIZE,
        control,
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
            .map(|i| PathBuf::from(format!("/stack/{i:02}.tif")))
            .collect()
    }

    #[test]
    fn rows_carry_chip_name_and_fraction_of_the_largest_count() {
        let rows = build_rows(&frames(3), &[2, 8, 4]);
        let fractions: Vec<f32> = rows.iter().map(|r| r.fraction).collect();
        assert_eq!(fractions, [0.25, 1.0, 0.5]);
        assert_eq!(rows[1].name, "01.tif");
        assert_eq!(rows[2].chip, source_color(2, 3));
        assert!(rows.iter().all(|r| r.mm_text.is_empty()));
    }

    #[test]
    fn all_zero_counts_leave_every_bar_empty() {
        let rows = build_rows(&frames(3), &[0, 0, 0]);
        assert!(rows.iter().all(|r| r.fraction == 0.0 && r.count == 0));
    }
}
