// SPDX-License-Identifier: GPL-3.0-or-later

//! The `Images` dock panel: one row per frame in fuse order, its legend
//! colour, name and capture time, and the copy for the frame-set-change
//! warnings. Plain Rust, no `use slint::` -- `crate::controller` builds
//! `ImagesModel` from a tab's own frame list and exclusion mask and hands
//! it to `ui/shell/dock/images_panel.slint`'s `for` loop; the schema/registry
//! crate is named here so `crate::controller` never has to.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use focusforge_core::io::{CaptureTime, capture_time};
use focusforge_core::timing::{TimingCounter, measurement_scope, record_timing};
use focusforge_core::zmap::{overlay_table, source_color};

/// A row's fixed height, top to top of the next row.
pub const ROW_HEIGHT: f32 = 26.0;

/// One row of the `Images` list. Carries no index of its own -- a frame is
/// a position, not an identity, and the row's position in [`ImagesModel::rows`]
/// *is* that position.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageRow {
    /// Stable source identity for background preview lookup.
    pub path: PathBuf,
    pub chip: [u8; 3],
    pub name: String,
    /// The recorded capture time in stacking mode, or the row's 1-based
    /// position in legend mode -- verbatim, or empty when a capture time is
    /// absent or the source is unresolved.
    pub trailing_text: String,
    /// The complete recorded capture timestamp, or empty when unavailable.
    pub capture_stamp: String,
    pub excluded: bool,
    /// The recorded source is no longer a file on disk.
    pub unresolved: bool,
    /// Whether this is the frame the left canvas pane draws.
    pub selected: bool,
}

/// The `Images` panel's whole state: header and rows.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImagesModel {
    pub rows: Vec<ImageRow>,
    pub header_text: String,
}

/// What the `Images` list shows about one source file, read once from disk
/// and keyed by the source's path so it follows the file through sorting
/// and reordering.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FrameMeta {
    pub time: Option<CaptureTime>,
    /// The source was not a file on disk when last read.
    pub unresolved: bool,
}

/// Reads each path's existence and capture time, one header read per
/// existing file.
pub fn read_metadata(paths: &[PathBuf]) -> HashMap<PathBuf, FrameMeta> {
    let _scope = measurement_scope("gui_image_metadata");
    paths
        .iter()
        .map(|path| {
            let unresolved = !path.is_file();
            let time = if unresolved {
                None
            } else {
                record_timing(TimingCounter::MetadataReads, 1);
                capture_time(path)
            };
            (path.clone(), FrameMeta { time, unresolved })
        })
        .collect()
}

/// Builds one row per frame, in order, from `meta` alone; a path absent from
/// `meta` renders as unresolved. `excluded` shorter than `frames` is read as
/// "not excluded" for the missing tail.
fn build_rows(
    frames: &[PathBuf],
    excluded: &[bool],
    selected: Option<usize>,
    meta: &HashMap<PathBuf, FrameMeta>,
) -> Vec<ImageRow> {
    let total = frames.len().max(1) as u32;
    frames
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let resolved = meta.get(path).filter(|m| !m.unresolved);
            ImageRow {
                path: path.clone(),
                chip: source_color(i as u16, total),
                name: file_name(path),
                trailing_text: resolved
                    .and_then(|m| m.time.as_ref())
                    .map(|t| time_of_day(&t.text))
                    .unwrap_or_default(),
                capture_stamp: resolved
                    .and_then(|m| m.time.as_ref())
                    .map(|t| t.text.clone())
                    .unwrap_or_default(),
                excluded: excluded.get(i).copied().unwrap_or(false),
                unresolved: resolved.is_none(),
                selected: selected == Some(i),
            }
        })
        .collect()
}

/// One legend row per frame, in position order -- editor mode only ever runs
/// over the frame set its own Z-Map was fused from (`can_edit`'s own gate),
/// so a position *is* its Z-Map source index and the exclusion mask is
/// never consulted: excluding a frame after the fuse changes no index.
/// `chip` is `overlay_table`'s colour for that index, no palette arithmetic.
fn build_legend_rows(
    frames: &[PathBuf],
    selected: Option<usize>,
    meta: &HashMap<PathBuf, FrameMeta>,
) -> Vec<ImageRow> {
    let total = frames.len().max(1) as u32;
    let table = overlay_table(total, None);
    frames
        .iter()
        .enumerate()
        .map(|(i, path)| ImageRow {
            path: path.clone(),
            chip: table[i],
            name: file_name(path),
            trailing_text: (i + 1).to_string(),
            capture_stamp: String::new(),
            excluded: false,
            unresolved: meta.get(path).is_none_or(|m| m.unresolved),
            selected: selected == Some(i),
        })
        .collect()
}

/// The position seeding lands on: the first not-excluded index, or `0` when
/// every frame is excluded -- a tab holding at least one frame always names
/// exactly one selected position, even a fully excluded one.
pub fn first_included(excluded: &[bool]) -> usize {
    excluded.iter().position(|&x| !x).unwrap_or(0)
}

/// The `HH:MM:SS` tail of a `"YYYY:MM:DD HH:MM:SS"` capture time -- the row
/// shows the time of day, not the date every frame in one stack shares.
/// Verbatim when `text` does not have that exact 19-byte shape.
fn time_of_day(text: &str) -> String {
    if text.len() == 19 {
        text[11..].to_string()
    } else {
        text.to_string()
    }
}

pub(crate) fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Builds the whole panel model, the header count and the rows, without
/// touching the disk. `legend` selects the editor's read-only legend (colour
/// and position number from `build_legend_rows`) over stacking's own row
/// builder (capture time, exclusion count).
pub fn build_model(
    frames: &[PathBuf],
    excluded: &[bool],
    selected: Option<usize>,
    legend: bool,
    meta: &HashMap<PathBuf, FrameMeta>,
) -> ImagesModel {
    let rows = if legend {
        build_legend_rows(frames, selected, meta)
    } else {
        build_rows(frames, excluded, selected, meta)
    };
    let header_text = if legend {
        format!(
            "{} frame{}",
            rows.len(),
            if rows.len() == 1 { "" } else { "s" }
        )
    } else {
        let excluded_count = rows.iter().filter(|r| r.excluded).count();
        format!(
            "{} frame{} \u{b7} {} excluded",
            rows.len(),
            if rows.len() == 1 { "" } else { "s" },
            excluded_count
        )
    };
    ImagesModel { rows, header_text }
}

/// The `Name` / `Capture time` sort menu's two entries -- and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSort {
    Name,
    CaptureTime,
}

/// The permutation `sort` produces over `frames`, each entry the original
/// index now at that position. A frame with no readable capture time sorts
/// after every timed frame, in its original relative order, when sorting by
/// `CaptureTime`. Capture times come from `meta`, never from the disk.
pub fn sort_order(
    frames: &[PathBuf],
    sort: FrameSort,
    meta: &HashMap<PathBuf, FrameMeta>,
) -> Vec<usize> {
    let mut order: Vec<usize> = (0..frames.len()).collect();
    match sort {
        FrameSort::Name => {
            order.sort_by(|&a, &b| file_name(&frames[a]).cmp(&file_name(&frames[b])))
        }
        FrameSort::CaptureTime => order.sort_by(|&a, &b| {
            let time = |i: usize| {
                meta.get(&frames[i])
                    .filter(|m| !m.unresolved)
                    .and_then(|m| m.time.as_ref())
            };
            match (time(a), time(b)) {
                (Some(x), Some(y)) => x.epoch_seconds.cmp(&y.epoch_seconds),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            }
        }),
    }
    order
}

/// The permutation a manual drag of the row at `from` to `to` produces:
/// remove-then-insert, the ordinary drag-list semantics. Out-of-range
/// indices leave the order unchanged.
pub fn move_order(len: usize, from: usize, to: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..len).collect();
    if from >= len || to >= len {
        return order;
    }
    let item = order.remove(from);
    order.insert(to, item);
    order
}

/// Which frame-set-changing action a warning is gating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSetChangeKind {
    Add,
    Move,
    Sort,
    Remove,
}

/// Which button, if any, a warning's `Cancel`/`Confirm` pair defaults to --
/// always `Cancel`: confirming appends a checkpoint and is never the
/// keyboard-default action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultButton {
    Cancel,
}

/// A frame-set-change warning's copy: the counted body line, the info line
/// naming the non-destructive alternative, and which buttons the footer
/// shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameSetWarningCopy {
    pub title: String,
    pub body: String,
    pub info: String,
    pub confirm_label: String,
    pub default_button: DefaultButton,
    /// `Add`, `Move` and `Sort` offer this as a literal footer button;
    /// `Remove` mentions it on the info line instead.
    pub new_project_button: bool,
    /// `Remove` only.
    pub exclude_instead_button: bool,
    /// The duplicate question only.
    pub keep_existing_button: bool,
}

impl FrameSetWarningCopy {
    /// The secondary footer button's label -- empty when none applies,
    /// which never happens for a real warning.
    pub fn secondary_label(&self) -> &'static str {
        if self.keep_existing_button {
            "Keep existing"
        } else if self.exclude_instead_button {
            "Exclude instead"
        } else if self.new_project_button {
            "New project instead"
        } else {
            ""
        }
    }
}

/// The copy a warning shows once a project already carries `checkpoint_count`
/// checkpoints -- always at least one, since the warning is unreachable
/// before the first checkpoint.
pub fn frame_set_warning_copy(
    kind: FrameSetChangeKind,
    checkpoint_count: usize,
) -> FrameSetWarningCopy {
    let body = format!(
        "The whole stack is fused from scratch. This project already holds {checkpoint_count} checkpoint{}, and confirming keeps every one of them.",
        if checkpoint_count == 1 { "" } else { "s" }
    );
    let (title, confirm_label, new_project_button, exclude_instead_button, info) = match kind {
        FrameSetChangeKind::Add => (
            "Add frames and fuse again?".to_string(),
            "Add and re-fuse".to_string(),
            true,
            false,
            "Or start a new project with the same settings instead.".to_string(),
        ),
        FrameSetChangeKind::Move => (
            "Reorder and fuse again?".to_string(),
            "Move and re-fuse".to_string(),
            true,
            false,
            "Or start a new project with the same settings instead.".to_string(),
        ),
        FrameSetChangeKind::Sort => (
            "Sort by capture time and fuse again?".to_string(),
            "Sort and re-fuse".to_string(),
            true,
            false,
            "Or start a new project with the same settings instead.".to_string(),
        ),
        FrameSetChangeKind::Remove => (
            "Remove and fuse again?".to_string(),
            "Remove and re-fuse".to_string(),
            false,
            true,
            "You can also start a new project with the same settings instead.".to_string(),
        ),
    };
    FrameSetWarningCopy {
        title,
        body,
        info,
        confirm_label,
        default_button: DefaultButton::Cancel,
        new_project_button,
        exclude_instead_button,
        keep_existing_button: false,
    }
}

/// The duplicate question's copy: up to five conflicting frame names, then
/// "and N more".
pub fn duplicate_question_copy(names: &[String]) -> FrameSetWarningCopy {
    let title = format!(
        "Replace {} existing image{}?",
        names.len(),
        if names.len() == 1 { "" } else { "s" }
    );
    let body = if names.len() <= 5 {
        names.join(", ")
    } else {
        format!("{} and {} more", names[..5].join(", "), names.len() - 5)
    };
    FrameSetWarningCopy {
        title,
        body,
        info: "New images are added either way.".to_string(),
        confirm_label: "Replace".to_string(),
        default_button: DefaultButton::Cancel,
        new_project_button: false,
        exclude_instead_button: false,
        keep_existing_button: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_timed(dir: &Path, name: &str, when: Option<&str>) -> PathBuf {
        let path = dir.join(name);
        focusforge_core::io::test_frames::write_frame_with_capture_time(&path, 4, 4, when)
            .expect("a synthetic frame must write");
        path
    }

    #[test]
    fn every_frame_gets_its_own_colour_and_no_row_carries_an_index() {
        let dir = tempfile::tempdir().unwrap();
        let frames: Vec<PathBuf> = (0..3)
            .map(|i| {
                write_timed(
                    dir.path(),
                    &format!("{i:02}.tif"),
                    Some("2026:08:29 10:00:00"),
                )
            })
            .collect();
        let model = build_model(
            &frames,
            &[false, false, false],
            None,
            false,
            &read_metadata(&frames),
        );
        assert_eq!(model.rows.len(), 3);
        assert_eq!(model.header_text, "3 frames \u{b7} 0 excluded");
        assert_ne!(model.rows[0].chip, model.rows[1].chip);
    }

    #[test]
    fn rows_come_from_the_metadata_map_not_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let frames = vec![
            write_timed(dir.path(), "00.tif", Some("2026:08:29 10:00:00")),
            write_timed(dir.path(), "01.tif", Some("2026:08:29 11:30:00")),
        ];
        let mut meta = read_metadata(&frames);
        std::fs::remove_file(&frames[0]).unwrap();
        std::fs::remove_file(&frames[1]).unwrap();
        meta.remove(&frames[1]);
        let model = build_model(&frames, &[false, false], None, false, &meta);
        assert!(!model.rows[0].unresolved);
        assert_eq!(model.rows[0].trailing_text, "10:00:00");
        assert!(model.rows[1].unresolved);
        assert_eq!(model.rows[1].trailing_text, "");
    }

    #[test]
    fn an_absent_capture_time_is_empty_text_with_no_fabricated_value() {
        let dir = tempfile::tempdir().unwrap();
        let frames = vec![
            write_timed(dir.path(), "00.tif", Some("2026:08:29 10:00:00")),
            write_timed(dir.path(), "01.tif", None),
        ];
        let model = build_model(
            &frames,
            &[false, false],
            None,
            false,
            &read_metadata(&frames),
        );
        assert_eq!(model.rows[1].trailing_text, "");
        assert!(!model.rows[1].unresolved);
    }

    #[test]
    fn an_unresolved_source_carries_no_fabricated_time() {
        let frames = [PathBuf::from("/does/not/exist.tif")];
        let model = build_model(&frames, &[false], None, false, &read_metadata(&frames));
        assert!(model.rows[0].unresolved);
        assert_eq!(model.rows[0].trailing_text, "");
    }

    #[test]
    fn excluding_is_counted_in_the_header() {
        let dir = tempfile::tempdir().unwrap();
        let frames: Vec<PathBuf> = (0..2)
            .map(|i| {
                write_timed(
                    dir.path(),
                    &format!("{i:02}.tif"),
                    Some("2026:08:29 10:00:00"),
                )
            })
            .collect();
        let model = build_model(
            &frames,
            &[true, false],
            None,
            false,
            &read_metadata(&frames),
        );
        assert_eq!(model.header_text, "2 frames \u{b7} 1 excluded");
    }

    #[test]
    fn sort_by_name_is_lexicographic() {
        let frames = vec![PathBuf::from("b.tif"), PathBuf::from("a.tif")];
        assert_eq!(
            sort_order(&frames, FrameSort::Name, &HashMap::new()),
            vec![1, 0]
        );
    }

    #[test]
    fn sort_by_capture_time_puts_untimed_frames_last() {
        let dir = tempfile::tempdir().unwrap();
        let frames = vec![
            write_timed(dir.path(), "later.tif", Some("2026:08:29 12:00:00")),
            write_timed(dir.path(), "untimed.tif", None),
            write_timed(dir.path(), "earlier.tif", Some("2026:08:29 10:00:00")),
        ];
        let meta = read_metadata(&frames);
        assert_eq!(
            sort_order(&frames, FrameSort::CaptureTime, &meta),
            vec![2, 0, 1]
        );
    }

    #[test]
    fn sort_by_capture_time_reads_the_cached_times() {
        let dir = tempfile::tempdir().unwrap();
        let frames = vec![
            write_timed(dir.path(), "a.tif", Some("2026:08:29 12:00:00")),
            write_timed(dir.path(), "b.tif", Some("2026:08:29 09:00:00")),
            write_timed(dir.path(), "c.tif", Some("2026:08:29 10:00:00")),
        ];
        let meta = read_metadata(&frames);
        for path in &frames {
            std::fs::remove_file(path).unwrap();
        }
        assert_eq!(
            sort_order(&frames, FrameSort::CaptureTime, &meta),
            vec![1, 2, 0]
        );
    }

    #[test]
    fn move_order_is_remove_then_insert() {
        assert_eq!(move_order(4, 0, 2), vec![1, 2, 0, 3]);
        assert_eq!(move_order(4, 3, 0), vec![3, 0, 1, 2]);
    }

    #[test]
    fn remove_offers_exclude_instead_and_mentions_a_new_project() {
        let copy = frame_set_warning_copy(FrameSetChangeKind::Remove, 2);
        assert!(copy.exclude_instead_button);
        assert!(!copy.new_project_button);
        assert!(copy.info.contains("new project"));
        assert!(copy.body.contains('2'));
        assert_eq!(copy.default_button, DefaultButton::Cancel);
        assert_eq!(copy.secondary_label(), "Exclude instead");
    }

    #[test]
    fn add_and_reorder_secondary_label_offers_a_new_project() {
        let copy = frame_set_warning_copy(FrameSetChangeKind::Add, 1);
        assert_eq!(copy.secondary_label(), "New project instead");
    }

    #[test]
    fn duplicate_question_copy_names_every_conflict_up_to_five() {
        let names: Vec<String> = (0..5).map(|i| format!("{i:02}.tif")).collect();
        let copy = duplicate_question_copy(&names);
        assert_eq!(copy.title, "Replace 5 existing images?");
        assert_eq!(copy.body, "00.tif, 01.tif, 02.tif, 03.tif, 04.tif");
        assert!(copy.keep_existing_button);
        assert_eq!(copy.secondary_label(), "Keep existing");
    }

    #[test]
    fn duplicate_question_copy_caps_at_five_with_an_and_more_tail() {
        let names: Vec<String> = (0..7).map(|i| format!("{i:02}.tif")).collect();
        let copy = duplicate_question_copy(&names);
        assert_eq!(copy.title, "Replace 7 existing images?");
        assert_eq!(
            copy.body,
            "00.tif, 01.tif, 02.tif, 03.tif, 04.tif and 2 more"
        );
    }

    #[test]
    fn duplicate_question_copy_singular_wording_names_one() {
        let copy = duplicate_question_copy(&["00.tif".to_string()]);
        assert_eq!(copy.title, "Replace 1 existing image?");
        assert_eq!(copy.body, "00.tif");
    }

    #[test]
    fn add_and_reorder_offer_a_new_project_button_not_exclude_instead() {
        for kind in [
            FrameSetChangeKind::Add,
            FrameSetChangeKind::Move,
            FrameSetChangeKind::Sort,
        ] {
            let copy = frame_set_warning_copy(kind, 1);
            assert!(copy.new_project_button);
            assert!(!copy.exclude_instead_button);
            assert_eq!(copy.default_button, DefaultButton::Cancel);
        }
    }
}
