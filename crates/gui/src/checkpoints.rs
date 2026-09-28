// SPDX-License-Identifier: GPL-3.0-or-later

//! The checkpoint dock panel's view model: day-grouped rows built from a
//! project's saved history, in the local clock. Plain Rust, no `use
//! slint::`; `crate::controller` builds a [`CheckpointsModel`] and hands it
//! to `ui/shell/dock/checkpoint_panel.slint`'s `for` loop.

use chrono::{Local, NaiveDate, TimeZone};

use focusforge_core::project::Checkpoint;
pub use focusforge_core::project::HeldCheckpoint;
use focusforge_core::registry;

/// A row's own height, top to top of the next row.
pub const ROW_HEIGHT: f32 = 40.0;
/// A day separator's own height.
pub const SEPARATOR_HEIGHT: f32 = 16.0;

/// One entry of the panel's flat, scrolling list: either a day separator or
/// a checkpoint row. Carries both shapes' fields so the `for` loop binds one
/// homogeneous type; the unused half is empty.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointEntry {
    /// `Some` draws a compact `FFSectionHeading`; `None` draws `FFCheckpointRow`.
    pub separator_label: Option<String>,
    pub algorithm: String,
    pub time_text: String,
    pub top: f32,
    pub selected: bool,
    /// A saved row's own id, or a held row's synthetic `"held:<index>"` key
    /// (not yet in any `.ffp`, so it has no id of its own) -- `None` only
    /// for a separator.
    pub id: Option<String>,
    /// A separator's own next separator's `top`, `None` when this is the
    /// last one or this entry is a row -- the sticky header's own cue for
    /// when the next day pushes it out, so the view need not search the
    /// list to find it.
    pub next_separator_top: Option<f32>,
}

/// A held row's own key into `ProjectTab::held`, stable until a save.
pub(crate) fn held_key(index: usize) -> String {
    format!("held:{index}")
}

/// [`held_key`]'s index back; `None` for a saved checkpoint's id.
pub(crate) fn held_index(key: &str) -> Option<usize> {
    key.strip_prefix("held:")?.parse().ok()
}

/// The checkpoint panel's whole state -- entries and the scroll offset
/// [`crate::settings_panel::clamp_panel_scroll`] already clamps for the
/// other two dock panels.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CheckpointsModel {
    pub entries: Vec<CheckpointEntry>,
    pub content_height: f32,
    pub scroll_y: f32,
    /// Checkpoints only, excluding separators -- the tab's own count.
    pub row_count: usize,
}

/// `unix_secs` as local `"HH:MM:SS"`, the row's own time-text format.
pub fn local_hhmmss(unix_secs: i64) -> String {
    local_datetime(unix_secs)
        .map(|dt| dt.format("%H:%M:%S").to_string())
        .unwrap_or_default()
}

/// `unix_secs` as local `"HH:MM"` -- the bottom bar's own format, in the
/// machine's own zone rather than UTC.
pub fn local_hhmm(unix_secs: i64) -> String {
    local_datetime(unix_secs)
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// The machine's own local date, for a caller that needs "today" to label
/// separators against.
pub fn today() -> NaiveDate {
    Local::now().date_naive()
}

fn local_datetime(unix_secs: i64) -> Option<chrono::DateTime<Local>> {
    Local.timestamp_opt(unix_secs, 0).single()
}

/// `TODAY`/`YESTERDAY` for the last two local days, `"12 SEP 2026"` beyond
/// that -- `today` is the caller's own reading of the local date, so a test
/// can pin it instead of racing the real clock.
fn day_label(date: NaiveDate, today: NaiveDate) -> String {
    if date == today {
        "TODAY".to_string()
    } else if today.pred_opt() == Some(date) {
        "YESTERDAY".to_string()
    } else {
        date.format("%d %b %Y").to_string().to_uppercase()
    }
}

/// The algorithm registry's own display name for `id`, or `id` itself when
/// unregistered -- `settings_panel`'s own picker-label resolution, applied
/// to a checkpoint's stored algorithm id instead of the active tab's fuse
/// picker.
fn algorithm_label(id: &str) -> String {
    registry::find_stack(id)
        .map(|a| a.display_name().to_string())
        .unwrap_or_else(|_| id.to_string())
}

/// A row's underlying checkpoint, saved or held, before it becomes a
/// [`CheckpointEntry`] -- what lets the merge below sort and label the two
/// kinds uniformly.
enum Row<'a> {
    Saved(&'a Checkpoint),
    Held(usize, &'a HeldCheckpoint),
}

impl Row<'_> {
    fn created_at(&self) -> i64 {
        match self {
            Row::Saved(c) => c.created_at,
            Row::Held(_, h) => h.created_at,
        }
    }

    fn id(&self) -> String {
        match self {
            Row::Saved(c) => c.id.clone(),
            Row::Held(index, _) => held_key(*index),
        }
    }

    fn algorithm(&self) -> &str {
        match self {
            Row::Saved(c) => &c.algorithm,
            Row::Held(_, h) => &h.algorithm,
        }
    }
}

/// Builds the panel's flat entry list: `checkpoints` merged with `held`
/// (fused since the last save, not yet in any `.ffp`), ordered by
/// `created_at` descending with a held row first on a tie. Day separators
/// appear only when the merged list spans more than one local day, `today`
/// being the caller's local date. `selected` matches a row's own id (a
/// saved row's stored id, a held row's `"held:<index>"`).
pub fn build_model(
    checkpoints: &[&Checkpoint],
    held: &[HeldCheckpoint],
    today: NaiveDate,
    selected: Option<&str>,
    scroll_y: f32,
) -> CheckpointsModel {
    let mut rows: Vec<Row> = checkpoints.iter().map(|c| Row::Saved(c)).collect();
    rows.extend(held.iter().enumerate().map(|(i, h)| Row::Held(i, h)));
    // A held row sorts before a saved one at the same `created_at`: `false <
    // true`, so `Row::Held`'s `0` rank stays ahead of `Row::Saved`'s `1`.
    rows.sort_by(|a, b| {
        b.created_at()
            .cmp(&a.created_at())
            .then_with(|| matches!(a, Row::Saved(_)).cmp(&matches!(b, Row::Saved(_))))
    });

    let dates: Vec<NaiveDate> = rows
        .iter()
        .map(|r| {
            local_datetime(r.created_at())
                .map(|dt| dt.date_naive())
                .unwrap_or(today)
        })
        .collect();
    let multi_day = dates.windows(2).any(|w| w[0] != w[1]);

    let mut entries = Vec::with_capacity(rows.len());
    let mut top = 0.0f32;
    let mut last_date: Option<NaiveDate> = None;
    for (row, &date) in rows.iter().zip(&dates) {
        if multi_day && last_date != Some(date) {
            entries.push(CheckpointEntry {
                separator_label: Some(day_label(date, today)),
                algorithm: String::new(),
                time_text: String::new(),
                top,
                selected: false,
                id: None,
                next_separator_top: None,
            });
            top += SEPARATOR_HEIGHT;
            last_date = Some(date);
        }
        let id = row.id();
        entries.push(CheckpointEntry {
            separator_label: None,
            algorithm: algorithm_label(row.algorithm()),
            time_text: local_hhmmss(row.created_at()),
            top,
            selected: selected == Some(id.as_str()),
            id: Some(id),
            next_separator_top: None,
        });
        top += ROW_HEIGHT;
    }

    // Backfill each separator's own `next_separator_top` from the one after
    // it, walking back to front so a single pass suffices.
    let mut next_sep_top: Option<f32> = None;
    for entry in entries.iter_mut().rev() {
        if entry.separator_label.is_some() {
            entry.next_separator_top = next_sep_top;
            next_sep_top = Some(entry.top);
        }
    }

    CheckpointsModel {
        entries,
        content_height: top,
        scroll_y,
        row_count: rows.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(id: &str, created_at: i64) -> Checkpoint {
        Checkpoint {
            id: id.to_string(),
            parent: None,
            ops: Vec::new(),
            zmap_blob: String::new(),
            stack_fingerprint: String::new(),
            algorithm: "a-stack-algorithm".to_string(),
            params: Default::default(),
            created_at,
            label: None,
            fuse: None,
        }
    }

    #[test]
    fn one_day_of_checkpoints_carries_no_separator() {
        let noon = 1_756_468_800; // an arbitrary local noon
        let a = checkpoint("a", noon);
        let b = checkpoint("b", noon + 60);
        let today = local_datetime(noon).unwrap().date_naive();
        let model = build_model(&[&b, &a], &[], today, None, 0.0);
        assert!(
            model.entries.iter().all(|e| e.separator_label.is_none()),
            "a single-day list must draw no separator"
        );
        assert_eq!(model.row_count, 2);
    }

    #[test]
    fn a_multi_day_list_labels_today_and_an_older_date() {
        let noon = 1_756_468_800;
        let today_cp = checkpoint("a", noon);
        let older_cp = checkpoint("b", noon - 3 * 86_400);
        let today = local_datetime(noon).unwrap().date_naive();
        let model = build_model(&[&today_cp, &older_cp], &[], today, None, 0.0);
        let labels: Vec<&str> = model
            .entries
            .iter()
            .filter_map(|e| e.separator_label.as_deref())
            .collect();
        assert_eq!(labels.len(), 2, "one separator per distinct day");
        assert_eq!(labels[0], "TODAY");
        assert_ne!(labels[1], "TODAY");
        assert_ne!(labels[1], "YESTERDAY");
    }

    #[test]
    fn an_unregistered_algorithm_id_shows_verbatim() {
        let mut c = checkpoint("a", 1_756_468_800);
        c.algorithm = "not-a-real-algorithm".to_string();
        let today = local_datetime(c.created_at).unwrap().date_naive();
        let model = build_model(&[&c], &[], today, None, 0.0);
        assert_eq!(model.entries[0].algorithm, "not-a-real-algorithm");
    }

    #[test]
    fn each_separator_carries_the_next_ones_top() {
        let noon = 1_756_468_800;
        let today_cp = checkpoint("a", noon);
        let yesterday_cp = checkpoint("b", noon - 86_400);
        let older_cp = checkpoint("c", noon - 3 * 86_400);
        let today = local_datetime(noon).unwrap().date_naive();
        let model = build_model(
            &[&today_cp, &yesterday_cp, &older_cp],
            &[],
            today,
            None,
            0.0,
        );
        let separator_tops: Vec<(f32, Option<f32>)> = model
            .entries
            .iter()
            .filter(|e| e.separator_label.is_some())
            .map(|e| (e.top, e.next_separator_top))
            .collect();
        assert_eq!(separator_tops.len(), 3, "one separator per day");
        assert_eq!(separator_tops[0].1, Some(separator_tops[1].0));
        assert_eq!(separator_tops[1].1, Some(separator_tops[2].0));
        assert_eq!(separator_tops[2].1, None, "the last separator has none");
    }

    #[test]
    fn the_selected_id_marks_exactly_one_row() {
        let a = checkpoint("a", 1_756_468_800);
        let b = checkpoint("b", 1_756_468_860);
        let today = local_datetime(a.created_at).unwrap().date_naive();
        let model = build_model(&[&b, &a], &[], today, Some("a"), 0.0);
        let selected: Vec<bool> = model.entries.iter().map(|e| e.selected).collect();
        assert_eq!(selected, vec![false, true]);
    }
}
