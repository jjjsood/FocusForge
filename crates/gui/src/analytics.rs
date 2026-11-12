// SPDX-License-Identifier: GPL-3.0-or-later

//! The Analytics overlay's rows: one per frame in fuse order, with the
//! frame's sharp-area count. Plain Rust, no `use slint::` -- `crate::controller`
//! owns the state and `crate::bridge` maps `AnalyticsRow` onto the overlay.

use std::path::PathBuf;

use focusforge_core::gpu::SharpnessBackend;
use focusforge_core::io::open_frame_set;
use focusforge_core::params::ParamValues;
pub(crate) use focusforge_core::sharp_areas::SharpAreas;
use focusforge_core::sharp_areas::{
    PairOverlap, SharpAreasError, count_sharp_areas_reporting_with_backend,
};
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

/// One segment of the overlap bar: a neighbour pair of frames.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlapSegment {
    /// Position on the colour scale: 0 red, 1 yellow, 2 green.
    pub scale: f32,
    /// False when neither frame has a sharp area.
    pub has_data: bool,
    /// Fewer than `SMALL_UNION` sharp areas in either frame.
    pub dimmed: bool,
    /// The whole-percent share with its unit; empty without data.
    pub percent: String,
    /// Two lines: the pair, then what its share says.
    pub tooltip: String,
}

/// What the overlay shows.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum AnalyticsView {
    /// No stack is open to count.
    #[default]
    Waiting,
    /// The stack's rows while its count runs, each pending or partial.
    Counting(Vec<AnalyticsRow>),
    /// The finished count: the rows and one overlap segment per neighbour pair.
    Rows(Vec<AnalyticsRow>, Vec<OverlapSegment>),
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

/// Sharp areas in either frame below which a pair's share is not trusted.
const SMALL_UNION: u32 = 10;
/// Shares below this read as a likely gap in focus.
const GAP_SHARE: f64 = 0.05;
/// The share at which the scale reaches green.
const GREEN_SHARE: f64 = 0.2;

/// A share's position on the colour scale: 0 at 0 %, 1 at 5 %, 2 from 20 %,
/// linear between and clamped.
pub(crate) fn overlap_scale(share: f64) -> f32 {
    let position = if share < GAP_SHARE {
        share / GAP_SHARE
    } else {
        1.0 + (share - GAP_SHARE) / (GREEN_SHARE - GAP_SHARE)
    };
    position.clamp(0.0, 2.0) as f32
}

/// One segment per neighbour pair of `frames`, from `overlaps`.
pub(crate) fn build_segments(frames: &[PathBuf], overlaps: &[PairOverlap]) -> Vec<OverlapSegment> {
    overlaps
        .iter()
        .zip(frames.windows(2))
        .enumerate()
        .map(|(k, (overlap, pair))| {
            let share = overlap.share();
            let percent = share.map_or_else(String::new, |s| format!("{} %", (s * 100.0).round()));
            let dimmed = overlap.either < SMALL_UNION;
            let verdict = match share {
                None => "No sharp areas in either image".to_owned(),
                Some(_) if dimmed => format!("{percent} \u{2013} too few sharp areas to be sure"),
                Some(s) if s < GAP_SHARE => {
                    format!("{percent} \u{2013} likely a gap in focus here")
                }
                Some(_) => format!("{percent} of their sharp areas overlap"),
            };
            OverlapSegment {
                scale: share.map_or(0.0, overlap_scale),
                has_data: share.is_some(),
                dimmed,
                percent,
                tooltip: format!(
                    "Images {} and {} \u{b7} {}, {}\n{verdict}",
                    k + 1,
                    k + 2,
                    file_name(&pair[0]),
                    file_name(&pair[1]),
                ),
            }
        })
        .collect()
}

/// The finished view of `frames` over their `areas`.
pub(crate) fn finished_view(frames: &[PathBuf], areas: &SharpAreas) -> AnalyticsView {
    AnalyticsView::Rows(
        build_rows(frames, &areas.counts, RowValue::Final),
        build_segments(frames, &areas.overlaps),
    )
}

/// One pending row per frame of `frames`.
pub(crate) fn pending_rows(frames: &[PathBuf]) -> Vec<AnalyticsRow> {
    build_rows(frames, &vec![0; frames.len()], RowValue::Pending)
}

/// The sharp-area counts and neighbour overlaps of `frames`, by the same two core calls
/// and defaults as the command line, read in tiles of `tile_size` (the
/// command line's default when `None`);
/// `on_tile` is given the counts so far after each tile. `backend` scores the
/// frame tiles when the stack is large enough for it.
pub(crate) fn count_frames<B: SharpnessBackend>(
    frames: &[PathBuf],
    tile_size: Option<u32>,
    control: &dyn TileControl,
    on_tile: &mut dyn FnMut(&[u32]),
    backend: Option<&B>,
) -> Result<SharpAreas, CountError> {
    let set = open_frame_set(
        frames,
        HostBudget::host_derived(&focusforge_core::host::snapshot()),
    )
    .map_err(|err| CountError::Failed(err.to_string()))?;
    let reader = set.reader.as_ref();
    count_sharp_areas_reporting_with_backend(
        reader,
        reader.len(),
        reader.dimensions(),
        reader.frame(0).samples_per_pixel,
        &ParamValues::new(),
        tile_size.unwrap_or(DEFAULT_TILE_SIZE),
        control,
        on_tile,
        backend.map(|b| b as &dyn SharpnessBackend),
    )
    .map_err(|err| match err {
        SharpAreasError::Stopped => CountError::Stopped,
        other => CountError::Failed(other.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    /// Declines every tile after counting the offer, so the count runs on the CPU.
    #[derive(Default)]
    struct Offers {
        min_pixel_frames: Option<u64>,
        calls: AtomicUsize,
    }

    impl SharpnessBackend for Offers {
        fn min_pixel_frames(&self) -> u64 {
            self.min_pixel_frames
                .unwrap_or(focusforge_core::sharp_areas::SHARPNESS_BACKEND_MIN_PIXEL_FRAMES)
        }

        fn score_tile(
            &self,
            _: &dyn focusforge_core::algorithm::TileReader,
            _: usize,
            _: focusforge_core::tile::Rect,
            _: u32,
        ) -> Result<Option<Vec<f32>>, focusforge_core::algorithm::AlgorithmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }
    }

    fn mantis_frames(n: usize) -> Vec<PathBuf> {
        (0..n)
            .map(|i| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/mantis-512/frames")
                    .join(format!("{i:02}.tif"))
            })
            .collect()
    }

    fn count_with(backend: Option<&Offers>) -> Vec<u32> {
        count_frames(
            &mantis_frames(3),
            Some(200),
            &focusforge_core::tile::NoControl,
            &mut |_| {},
            backend,
        )
        .unwrap()
        .counts
    }

    #[test]
    fn analytics_gpu_offers_every_frame_tile_at_a_zero_threshold() {
        let offers = Offers {
            min_pixel_frames: Some(0),
            ..Default::default()
        };
        let counts = count_with(Some(&offers));
        assert_eq!(counts, count_with(None));
        let tiles = focusforge_core::sharp_areas::sharpness_plan((512, 512), 200)
            .unwrap()
            .tiles()
            .len();
        assert_eq!(offers.calls.load(Ordering::SeqCst), 3 * tiles);
    }

    #[test]
    fn analytics_gpu_offers_nothing_under_the_default_threshold() {
        let offers = Offers::default();
        assert_eq!(count_with(Some(&offers)), count_with(None));
        assert_eq!(offers.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn all_zero_counts_leave_every_bar_empty() {
        let rows = build_rows(&frames(3), &[0, 0, 0], RowValue::Final);
        assert!(rows.iter().all(|r| r.fraction == 0.0 && r.count == 0));
    }

    /// `{both, either}` per neighbour pair.
    fn overlaps(pairs: &[(u32, u32)]) -> Vec<PairOverlap> {
        pairs
            .iter()
            .map(|&(both, either)| PairOverlap { both, either })
            .collect()
    }

    #[test]
    fn overlap_scale_follows_the_stops_and_clamps() {
        for (share, at) in [
            (0.0, 0.0),
            (0.025, 0.5),
            (0.05, 1.0),
            (0.125, 1.5),
            (0.2, 2.0),
            (1.0, 2.0),
            (-0.3, 0.0),
            (7.0, 2.0),
        ] {
            let got = overlap_scale(share);
            assert!((got - at).abs() < 1e-6, "share {share}: {got}, wanted {at}");
        }
    }

    #[test]
    fn overlap_scale_has_no_jump_over_a_one_tenth_percent_sweep() {
        // Inputs sit off the stops; the steepest piece rises 20 per unit share.
        let samples: Vec<f32> = (0..=1020)
            .map(|i| overlap_scale(-0.01 + 0.00037 + f64::from(i) * 0.001))
            .collect();
        for pair in samples.windows(2) {
            let step = pair[1] - pair[0];
            assert!(
                (0.0..=0.02 + 1e-5).contains(&step),
                "{} to {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn overlap_tooltip_words_line_two_by_priority() {
        let frames = frames(7);
        let segments = build_segments(
            &frames,
            &overlaps(&[(0, 0), (2, 2), (0, 16), (1, 40), (1, 30), (30, 40)]),
        );
        let lines: Vec<(&str, &str)> = segments
            .iter()
            .map(|s| s.tooltip.split_once('\n').expect("two lines"))
            .collect();
        assert_eq!(
            lines.iter().map(|l| l.1).collect::<Vec<_>>(),
            [
                "No sharp areas in either image",
                "100 % \u{2013} too few sharp areas to be sure",
                "0 % \u{2013} likely a gap in focus here",
                "5 % \u{2013} likely a gap in focus here",
                "6 % of their sharp areas overlap",
                "86 % of their sharp areas overlap",
            ]
        );
        assert_eq!(lines[3].0, "Images 4 and 5 \u{b7} 03.tif, 04.tif");
        assert_eq!(
            segments
                .iter()
                .map(|s| s.percent.as_str())
                .collect::<Vec<_>>(),
            ["", "100 %", "0 %", "5 %", "6 %", "86 %"]
        );
        assert_eq!(
            segments.iter().map(|s| s.dimmed).collect::<Vec<_>>(),
            [true, true, false, false, false, false]
        );
        assert_eq!(
            segments.iter().map(|s| s.has_data).collect::<Vec<_>>(),
            [false, true, true, true, true, true]
        );
        // The no-data line is the one copy allowed to say "either".
        for (i, segment) in segments.iter().enumerate() {
            let text = segment.tooltip.to_lowercase();
            for word in ["cell", "union", "dice", "both"] {
                assert!(!text.contains(word), "{word:?} in {text:?}");
            }
            assert!(i == 0 || !text.contains("either"), "{text:?}");
        }
    }
}
