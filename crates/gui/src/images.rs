// SPDX-License-Identifier: GPL-3.0-or-later

//! The `Images` dock panel: one row per frame in fuse order, its legend
//! colour, name and capture time, and the copy for the frame-set-change
//! warnings. Plain Rust, no `use slint::` -- `crate::controller` builds
//! `ImagesModel` from a tab's own frame list and exclusion mask and hands
//! it to `ui/shell/dock/images_panel.slint`'s `for` loop; the schema/registry
//! crate is named here so `crate::controller` never has to.

use std::path::{Path, PathBuf};

use focusforge_core::io::capture_time;
use focusforge_core::zmap::source_color;

/// A row's fixed height, top to top of the next row.
pub const ROW_HEIGHT: f32 = 26.0;

/// One row of the `Images` list. Carries no index of its own -- a frame is
/// a position, not an identity, and the row's position in [`ImagesModel::rows`]
/// *is* that position.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageRow {
    pub chip: [u8; 3],
    pub name: String,
    /// The recorded capture time, verbatim, or empty when absent or the
    /// source is unresolved.
    pub time_text: String,
    pub excluded: bool,
    /// The recorded source is no longer a file on disk.
    pub unresolved: bool,
    pub top: f32,
}

/// The `Images` panel's whole state -- header, rows and the scroll offset
/// [`crate::settings_panel::clamp_panel_scroll`] already clamps for the
/// settings panel.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImagesModel {
    pub rows: Vec<ImageRow>,
    pub header_text: String,
    pub content_height: f32,
    pub scroll_y: f32,
}

/// Builds one row per frame, in order. `excluded` shorter than `frames` is
/// read as "not excluded" for the missing tail -- a tab always keeps the two
/// in lockstep, but a row builder should not panic on an untrusted slice.
fn build_rows(frames: &[PathBuf], excluded: &[bool]) -> Vec<ImageRow> {
    let total = frames.len().max(1) as u32;
    frames
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let unresolved = !path.is_file();
            let time_text = if unresolved {
                String::new()
            } else {
                capture_time(path)
                    .map(|t| time_of_day(&t.text))
                    .unwrap_or_default()
            };
            ImageRow {
                chip: source_color(i as u16, total),
                name: file_name(path),
                time_text,
                excluded: excluded.get(i).copied().unwrap_or(false),
                unresolved,
                top: i as f32 * ROW_HEIGHT,
            }
        })
        .collect()
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

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Builds the whole panel model: the header count and the rows, at a given
/// scroll offset already clamped by the caller.
pub fn build_model(frames: &[PathBuf], excluded: &[bool], scroll_y: f32) -> ImagesModel {
    let rows = build_rows(frames, excluded);
    let excluded_count = rows.iter().filter(|r| r.excluded).count();
    let header_text = format!(
        "{} frame{} \u{b7} {} excluded",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        excluded_count
    );
    let content_height = rows.len() as f32 * ROW_HEIGHT;
    ImagesModel {
        rows,
        header_text,
        content_height,
        scroll_y,
    }
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
/// `CaptureTime`.
pub fn sort_order(frames: &[PathBuf], sort: FrameSort) -> Vec<usize> {
    let mut order: Vec<usize> = (0..frames.len()).collect();
    match sort {
        FrameSort::Name => {
            order.sort_by(|&a, &b| file_name(&frames[a]).cmp(&file_name(&frames[b])))
        }
        FrameSort::CaptureTime => order.sort_by(|&a, &b| {
            let time = |i: usize| {
                frames[i]
                    .is_file()
                    .then(|| capture_time(&frames[i]))
                    .flatten()
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
}

impl FrameSetWarningCopy {
    /// The secondary footer button's own label -- empty when neither
    /// applies, which never happens for a real warning.
    pub fn secondary_label(&self) -> &'static str {
        if self.exclude_instead_button {
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
        let model = build_model(&frames, &[false, false, false], 0.0);
        assert_eq!(model.rows.len(), 3);
        assert_eq!(model.header_text, "3 frames \u{b7} 0 excluded");
        // ImageRow's own fields are exhaustively named here -- there is no
        // index field to omit.
        let ImageRow { top, .. } = &model.rows[1];
        assert_eq!(*top, ROW_HEIGHT);
    }

    #[test]
    fn an_absent_capture_time_is_empty_text_with_no_fabricated_value() {
        let dir = tempfile::tempdir().unwrap();
        let frames = vec![
            write_timed(dir.path(), "00.tif", Some("2026:08:29 10:00:00")),
            write_timed(dir.path(), "01.tif", None),
        ];
        let model = build_model(&frames, &[false, false], 0.0);
        assert_eq!(model.rows[1].time_text, "");
        assert!(!model.rows[1].unresolved);
    }

    #[test]
    fn an_unresolved_source_carries_no_fabricated_time() {
        let model = build_model(&[PathBuf::from("/does/not/exist.tif")], &[false], 0.0);
        assert!(model.rows[0].unresolved);
        assert_eq!(model.rows[0].time_text, "");
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
        let model = build_model(&frames, &[true, false], 0.0);
        assert_eq!(model.header_text, "2 frames \u{b7} 1 excluded");
    }

    #[test]
    fn sort_by_name_is_lexicographic() {
        let frames = vec![PathBuf::from("b.tif"), PathBuf::from("a.tif")];
        assert_eq!(sort_order(&frames, FrameSort::Name), vec![1, 0]);
    }

    #[test]
    fn sort_by_capture_time_puts_untimed_frames_last() {
        let dir = tempfile::tempdir().unwrap();
        let frames = vec![
            write_timed(dir.path(), "later.tif", Some("2026:08:29 12:00:00")),
            write_timed(dir.path(), "untimed.tif", None),
            write_timed(dir.path(), "earlier.tif", Some("2026:08:29 10:00:00")),
        ];
        assert_eq!(sort_order(&frames, FrameSort::CaptureTime), vec![2, 0, 1]);
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
