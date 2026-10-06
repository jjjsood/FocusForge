// SPDX-License-Identifier: GPL-3.0-or-later

//! The checkpoint dock panel's view model: day-grouped rows built from a
//! project's saved history, in the local clock. Plain Rust, no `use
//! slint::`; `crate::controller` builds a [`CheckpointsModel`](crate::checkpoints::CheckpointsModel) and hands it
//! to `ui/shell/dock/checkpoint_panel.slint`'s `for` loop.

use std::cmp::Ordering;

use chrono::{Local, NaiveDate, TimeZone};

use focusforge_core::project::Checkpoint;
use focusforge_core::project::Op;
pub use focusforge_core::project::{InMemoryCheckpoint, InMemoryKind, ParentRef};
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
    /// The creation generation of a row a checkpoint creation just added;
    /// `None` for every other entry.
    pub fresh: Option<u64>,
}

/// A held row's own key into `ProjectTab::held`, stable until a save.
pub(crate) fn held_key(index: usize) -> String {
    format!("held:{index}")
}

/// [`held_key`]'s index back; `None` for a saved checkpoint's id.
pub(crate) fn held_index(key: &str) -> Option<usize> {
    key.strip_prefix("held:")?.parse().ok()
}

/// The ops of in-memory entry `index` and of the op-bearing entries it
/// descends from, oldest first, up to the first fused entry or saved
/// checkpoint. `None` when `index` is not an op-bearing entry.
pub(crate) fn stroke_chain(held: &[InMemoryCheckpoint], index: usize) -> Option<Vec<Op>> {
    let mut levels = Vec::new();
    let mut entry = held.get(index)?;
    while let InMemoryKind::Ops { ops, .. } = &entry.kind {
        levels.push(ops);
        match &entry.parent {
            Some(ParentRef::Entry(parent)) => entry = held.get(*parent)?,
            _ => break,
        }
    }
    if levels.is_empty() {
        return None;
    }
    Some(levels.into_iter().rev().flatten().cloned().collect())
}

/// Op kinds in the order a summary lists them; the last catches any op kind
/// the list does not name.
const OP_NAMES: [(&str, &str); 2] = [
    ("brush stroke", "brush strokes"),
    ("other change", "other changes"),
];

/// The counted phrase a question about unsaved ops opens with, in digits and
/// grouped by kind: "12 brush strokes and 2 other changes".
pub(crate) fn op_summary<'a>(ops: impl IntoIterator<Item = &'a Op>) -> String {
    let mut counts = [0usize; OP_NAMES.len()];
    for op in ops {
        counts[match op {
            Op::BrushStroke { .. } => 0,
            _ => 1,
        }] += 1;
    }
    counted_phrase(counts)
}

/// `counts` per [`OP_NAMES`] kind as "A", "A and B" or "A, B and C".
fn counted_phrase(counts: [usize; OP_NAMES.len()]) -> String {
    let parts: Vec<String> = counts
        .iter()
        .zip(OP_NAMES)
        .filter(|(count, _)| **count > 0)
        .map(|(count, (one, many))| format!("{count} {}", if *count == 1 { one } else { many }))
        .collect();
    match parts.as_slice() {
        [] => String::new(),
        [only] => only.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
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

/// `unix_secs` as local `"2026-09-28 14:32"` -- the export modal's own
/// full date-and-time format, unlike the list row's relative day label.
pub fn local_date_and_time(unix_secs: i64) -> String {
    local_datetime(unix_secs)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
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
/// picker. `pub(crate)`: any module naming a checkpoint's algorithm reads it
/// through this, never the registry directly.
pub(crate) fn algorithm_label(id: &str) -> String {
    registry::find_stack(id)
        .map(|a| a.display_name().to_string())
        .unwrap_or_else(|_| id.to_string())
}

/// A row's underlying checkpoint, saved or held, before it becomes a
/// [`CheckpointEntry`] -- what lets the merge below sort and label the two
/// kinds uniformly.
enum Row<'a> {
    Saved(&'a Checkpoint),
    Held {
        index: usize,
        created_at: i64,
        algorithm: String,
    },
}

impl Row<'_> {
    fn created_at(&self) -> i64 {
        match self {
            Row::Saved(c) => c.created_at,
            Row::Held { created_at, .. } => *created_at,
        }
    }

    fn id(&self) -> String {
        match self {
            Row::Saved(c) => c.id.clone(),
            Row::Held { index, .. } => held_key(*index),
        }
    }

    fn algorithm(&self) -> &str {
        match self {
            Row::Saved(c) => &c.algorithm,
            Row::Held { algorithm, .. } => algorithm,
        }
    }
}

/// The algorithm `held[index]` carries once written: a fused entry's own, an
/// op-bearing one's parent chain's (the checkpoint it is replayed onto). Empty when the chain does not resolve.
fn held_algorithm(index: usize, held: &[InMemoryCheckpoint], saved: &[&Checkpoint]) -> String {
    let Some(entry) = held.get(index) else {
        return String::new();
    };
    match &entry.kind {
        InMemoryKind::Fused(fused) => fused.algorithm.clone(),
        InMemoryKind::Ops { .. } => match &entry.parent {
            Some(ParentRef::Entry(parent)) if *parent < index => {
                held_algorithm(*parent, held, saved)
            }
            Some(ParentRef::Saved(id)) => saved
                .iter()
                .find(|c| &c.id == id)
                .map(|c| c.algorithm.clone())
                .unwrap_or_default(),
            _ => String::new(),
        },
    }
}

/// Builds the panel's flat entry list: `checkpoints` merged with `held`
/// (in memory since the last save, not yet in any `.ffp`), ordered by
/// `created_at` descending. On a tie a held row comes before a saved one, a
/// newer held row (higher index) before an older one, and saved rows keep
/// their order in `checkpoints`. A hidden saved checkpoint and every row
/// `pending` names are left out (`checkpoints` and `held` stay whole for the
/// parent lookups). Day separators appear only when the merged list spans
/// more than one local day, `today` being the caller's local date.
/// `selected` matches a row's own id (a saved row's stored id, a held row's
/// `"held:<index>"`).
pub fn build_model(
    checkpoints: &[&Checkpoint],
    held: &[InMemoryCheckpoint],
    pending: &[ParentRef],
    today: NaiveDate,
    selected: Option<&str>,
    scroll_y: f32,
) -> CheckpointsModel {
    let mut rows: Vec<Row> = checkpoints
        .iter()
        .filter(|c| !c.hidden && !pending.contains(&ParentRef::Saved(c.id.clone())))
        .map(|c| Row::Saved(c))
        .collect();
    rows.extend(
        held.iter()
            .enumerate()
            .filter(|(index, _)| !pending.contains(&ParentRef::Entry(*index)))
            .map(|(index, entry)| Row::Held {
                index,
                created_at: entry.created_at(),
                algorithm: held_algorithm(index, held, checkpoints),
            }),
    );
    // On a `created_at` tie a held row sorts before a saved one (`false <
    // true`), and a newer held row (higher index) before an older one. Saved
    // rows compare equal there, so the stable sort keeps their input order.
    rows.sort_by(|a, b| {
        b.created_at()
            .cmp(&a.created_at())
            .then_with(|| matches!(a, Row::Saved(_)).cmp(&matches!(b, Row::Saved(_))))
            .then_with(|| match (a, b) {
                (Row::Held { index: x, .. }, Row::Held { index: y, .. }) => y.cmp(x),
                _ => Ordering::Equal,
            })
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
                fresh: None,
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
            fresh: None,
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
            hidden: false,
        }
    }

    #[test]
    fn one_day_of_checkpoints_carries_no_separator() {
        let noon = 1_756_468_800; // an arbitrary local noon
        let a = checkpoint("a", noon);
        let b = checkpoint("b", noon + 60);
        let today = local_datetime(noon).unwrap().date_naive();
        let model = build_model(&[&b, &a], &[], &[], today, None, 0.0);
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
        let model = build_model(&[&today_cp, &older_cp], &[], &[], today, None, 0.0);
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
        let model = build_model(&[&c], &[], &[], today, None, 0.0);
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
        let model = build_model(&[&b, &a], &[], &[], today, Some("a"), 0.0);
        let selected: Vec<bool> = model.entries.iter().map(|e| e.selected).collect();
        assert_eq!(selected, vec![false, true]);
    }

    #[test]
    fn an_in_memory_op_row_reads_as_its_written_checkpoint() {
        use focusforge_core::project::{InMemoryCheckpoint, InMemoryKind, ParentRef};
        let noon = 1_756_468_800;
        let mut saved = checkpoint("a", noon - 2 * 86_400);
        saved.algorithm = "saved-algorithm".to_string();
        let ops_at = noon - 60;
        let map_at = noon;
        let held = [
            InMemoryCheckpoint {
                kind: InMemoryKind::Ops {
                    ops: Vec::new(),
                    created_at: ops_at,
                },
                parent: Some(ParentRef::Saved("a".to_string())),
            },
            InMemoryCheckpoint {
                kind: InMemoryKind::Ops {
                    ops: Vec::new(),
                    created_at: map_at,
                },
                parent: Some(ParentRef::Entry(0)),
            },
        ];
        let today = local_datetime(noon).unwrap().date_naive();
        let model = build_model(&[&saved], &held, &[], today, None, 0.0);

        let rows: Vec<&CheckpointEntry> = model
            .entries
            .iter()
            .filter(|e| e.separator_label.is_none())
            .collect();
        let ids: Vec<&str> = rows.iter().filter_map(|e| e.id.as_deref()).collect();
        assert_eq!(ids, ["held:1", "held:0", "a"], "newest first");
        assert_eq!(rows[0].time_text, local_hhmmss(map_at));
        assert_eq!(rows[1].time_text, local_hhmmss(ops_at));
        assert_eq!(rows[0].algorithm, "saved-algorithm");
        assert_eq!(rows[1].algorithm, "saved-algorithm");
        let labels: Vec<&str> = model
            .entries
            .iter()
            .filter_map(|e| e.separator_label.as_deref())
            .collect();
        assert_eq!(labels.len(), 2, "today's rows, then the saved row's day");
    }

    #[test]
    fn queued_and_hidden_rows_leave_the_list_and_keep_every_other_key() {
        use focusforge_core::project::{InMemoryCheckpoint, InMemoryKind, ParentRef};
        let noon = 1_756_468_800;
        let mut hidden = checkpoint("a", noon - 500);
        hidden.algorithm = "hidden-algorithm".to_string();
        hidden.hidden = true;
        let b = checkpoint("b", noon - 400);
        let c = checkpoint("c", noon - 300);
        let ops = |parent: ParentRef, created_at: i64| InMemoryCheckpoint {
            kind: InMemoryKind::Ops {
                ops: Vec::new(),
                created_at,
            },
            parent: Some(parent),
        };
        let held = [
            ops(ParentRef::Saved("a".to_string()), noon - 200),
            ops(ParentRef::Entry(0), noon - 100),
            ops(ParentRef::Saved("b".to_string()), noon),
        ];
        let pending = [ParentRef::Saved("c".to_string()), ParentRef::Entry(1)];
        let today = local_datetime(noon).unwrap().date_naive();
        let model = build_model(&[&hidden, &b, &c], &held, &pending, today, None, 0.0);

        let rows: Vec<&CheckpointEntry> = model
            .entries
            .iter()
            .filter(|e| e.separator_label.is_none())
            .collect();
        let ids: Vec<&str> = rows.iter().filter_map(|e| e.id.as_deref()).collect();
        assert_eq!(ids, ["held:2", "held:0", "b"]);
        assert_eq!(model.row_count, 3);
        assert_eq!(
            rows[1].algorithm, "hidden-algorithm",
            "an op row names its hidden parent's algorithm"
        );
    }

    #[test]
    fn same_second_in_memory_checkpoints_list_newest_first() {
        use ParentRef::Entry;
        let held = [
            fused_entry(),
            ops_entry(Vec::new(), Some(Entry(0))),
            fused_entry(),
            ops_entry(Vec::new(), Some(Entry(2))),
        ];
        let (z, y, x) = (checkpoint("z", 0), checkpoint("y", 0), checkpoint("x", 1));
        let saved = [&z, &y, &x];
        let today = local_datetime(0).unwrap().date_naive();
        let ids = |pending: &[ParentRef], selected: Option<&str>| {
            let model = build_model(&saved, &held, pending, today, selected, 0.0);
            model
                .entries
                .into_iter()
                .filter(|e| e.separator_label.is_none())
                .map(|e| (e.id.unwrap(), e.selected))
                .collect::<Vec<_>>()
        };
        let keys = |rows: &[(String, bool)]| rows.iter().map(|r| r.0.clone()).collect::<Vec<_>>();

        assert_eq!(
            keys(&ids(&[], None)),
            ["x", "held:3", "held:2", "held:1", "held:0", "z", "y"]
        );
        let pending = ids(&[Entry(2)], Some("held:1"));
        assert_eq!(
            keys(&pending),
            ["x", "held:3", "held:1", "held:0", "z", "y"]
        );
        let marked: Vec<&str> = pending
            .iter()
            .filter(|r| r.1)
            .map(|r| r.0.as_str())
            .collect();
        assert_eq!(marked, ["held:1"]);
    }

    fn stroke(x: i32) -> Op {
        Op::BrushStroke {
            source_id: 0,
            path: vec![focusforge_core::project::Point { x, y: 0 }],
            radius: 1.0,
            softness: 0.0,
        }
    }

    #[test]
    fn an_op_summary_counts_in_digits_per_kind() {
        assert_eq!(op_summary(&[stroke(0)]), "1 brush stroke");
        assert_eq!(
            op_summary(&[stroke(0), stroke(1), stroke(2)]),
            "3 brush strokes"
        );
        assert_eq!(op_summary(&[]), "");
    }

    #[test]
    fn an_op_kind_the_list_does_not_name_reads_as_other_changes() {
        assert_eq!(counted_phrase([0, 1]), "1 other change");
        assert_eq!(
            counted_phrase([2, 3]),
            "2 brush strokes and 3 other changes"
        );
    }

    fn ops_entry(ops: Vec<Op>, parent: Option<ParentRef>) -> InMemoryCheckpoint {
        InMemoryCheckpoint {
            kind: InMemoryKind::Ops { ops, created_at: 0 },
            parent,
        }
    }

    fn fused_entry() -> InMemoryCheckpoint {
        use focusforge_core::project::{HeldCheckpoint, Metrics};
        use std::path::PathBuf;
        InMemoryCheckpoint {
            kind: InMemoryKind::Fused(HeldCheckpoint {
                composite: PathBuf::new(),
                zmap: PathBuf::new(),
                width: 1,
                height: 1,
                sources: Vec::new(),
                algorithm: "a-stack-algorithm".to_string(),
                params: Default::default(),
                align: None,
                registration: Default::default(),
                composite_description: String::new(),
                metrics: Metrics::default(),
                created_at: 0,
                compressed_composite: None,
            }),
            parent: None,
        }
    }

    #[test]
    fn a_stroke_chain_is_the_ops_down_to_the_base_oldest_first() {
        let saved = Some(ParentRef::Saved("a".to_string()));
        let held = [
            fused_entry(),
            ops_entry(vec![stroke(1)], Some(ParentRef::Entry(0))),
            ops_entry(vec![stroke(2), stroke(4)], Some(ParentRef::Entry(1))),
            ops_entry(vec![stroke(3)], saved),
        ];
        assert_eq!(
            stroke_chain(&held, 1),
            Some(vec![stroke(1)]),
            "over a fused entry"
        );
        assert_eq!(
            stroke_chain(&held, 3),
            Some(vec![stroke(3)]),
            "over a saved id"
        );
        assert_eq!(
            stroke_chain(&held, 2),
            Some(vec![stroke(1), stroke(2), stroke(4)]),
            "two levels deep, oldest first"
        );
    }

    #[test]
    fn only_an_op_bearing_chain_of_entries_is_a_stroke_chain() {
        let held = [
            fused_entry(),
            ops_entry(vec![stroke(1)], Some(ParentRef::Entry(0))),
        ];
        assert_eq!(stroke_chain(&held, 0), None, "a fused entry");
        assert_eq!(
            stroke_chain(&held, 1),
            Some(vec![stroke(1)]),
            "up to the fused entry"
        );
        assert_eq!(stroke_chain(&held, 9), None, "no such entry");
    }
}
