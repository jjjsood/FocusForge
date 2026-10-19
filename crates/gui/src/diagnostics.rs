// SPDX-License-Identifier: GPL-3.0-or-later

//! The diagnostics panel's own state, as plain Rust -- no `use slint::`
//! anywhere in this module, the same seam every other view type in this
//! crate keeps (`crate::input_map`, `crate::viewport`). The panel binds a
//! [`DiagnosticsView`](crate::diagnostics::DiagnosticsView); the controller
//! is what builds one, from `focusforge_diagnostics::bundle`'s
//! manifest-then-write API.

use std::cell::RefCell;

use focusforge_core::timing::timing_enabled;
/// The one re-export of `focusforge_core::timing` outside this module --
/// every other GUI file reaches it through `crate::diagnostics`, never
/// `focusforge_core` directly (`crates/gui/tests/shell.rs`'s
/// `checkpoint_panel_touches_no_storage`).
pub(crate) use focusforge_core::timing::{
    MeasurementScope, TimingCounter, measurement_scope, measurement_scope_with_fields,
    record_timing,
};
#[cfg(test)]
pub(crate) use focusforge_core::timing::{enable_trace_accounting, timing_snapshot};

thread_local! {
    static INPUT_TRACE: RefCell<InputTrace> = RefCell::new(InputTrace::default());
}

/// One row of the file list: a name, its value column already rendered as
/// text, and whether it carries unredacted binary content.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiagnosticsEntryView {
    pub name: String,
    pub value: String,
    pub not_redacted: bool,
}

/// One entry of a completed manifest scan, before it is formatted for
/// display: the value column's byte count and redacted mark are rendered
/// from this by [`crate::controller::AppController::diagnostics_view`], not
/// computed again on the scan thread.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DiagnosticsScanEntry {
    pub name: String,
    pub bytes: u64,
    pub redacted: bool,
}

/// A background manifest scan's result: which entries the bundle would
/// hold and whether it found any log or crash evidence at all, independent
/// of `bundle::Manifest` so the scan thread never hands `fs::Metadata` or a
/// formatted string back to the controller.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DiagnosticsScan {
    pub entries: Vec<DiagnosticsScanEntry>,
    pub has_logs: bool,
    pub has_crash_evidence: bool,
}

/// Where a write attempt stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiagnosticsStatus {
    #[default]
    Idle,
    ExistingFile,
    Writing,
    Written,
    Failed,
}

/// Everything the diagnostics panel shows, built fresh on every change so
/// the panel never carries state of its own.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DiagnosticsView {
    pub entries: Vec<DiagnosticsEntryView>,
    /// Present exactly when the manifest found neither a log nor crash
    /// evidence to collect.
    pub note: Option<String>,
    pub include_minidumps: bool,
    pub destination: String,
    pub status: DiagnosticsStatus,
    pub message: String,
    pub primary_label: String,
    /// Set while a write is in flight, disabling every control.
    pub busy: bool,
    /// Set while no scan has completed yet, or a rescan is in flight,
    /// disabling only the footer's primary action.
    pub loading: bool,
    /// The manifest list's current scroll offset, in its own track-pixel
    /// space.
    pub manifest_scroll_offset: f32,
    /// The manifest list's total content height for its current row count.
    pub manifest_content_height: f32,
    /// The destination field's current horizontal scroll offset.
    pub destination_scroll_offset: f32,
}

/// One manifest row's fixed height and the gap between rows, in the units
/// the manifest list's own layout uses.
const MANIFEST_ROW_HEIGHT: f32 = 18.0;
const MANIFEST_ROW_SPACING: f32 = 8.0;

/// The manifest list's total content height for `entry_count` rows plus one
/// optional note row -- the same sum its layout arrives at, needed to size
/// its scrollbar without measuring the rendered layout itself.
pub fn manifest_content_height(entry_count: usize, has_note: bool) -> f32 {
    let rows = entry_count + usize::from(has_note);
    if rows == 0 {
        return 0.0;
    }
    rows as f32 * MANIFEST_ROW_HEIGHT + (rows - 1) as f32 * MANIFEST_ROW_SPACING
}

/// Clamps a scroll offset to `[0, max(content_height - viewport_height, 0)]`.
pub fn clamp_manifest_scroll(offset: f32, content_height: f32, viewport_height: f32) -> f32 {
    offset.clamp(0.0, (content_height - viewport_height).max(0.0))
}

/// Renders a byte count the way the panel's value column does: whole
/// kilobytes below a megabyte, one decimal place above.
pub fn size_text(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    if bytes < MB {
        format!("{} KB", (bytes as f64 / 1024.0).round() as u64)
    } else {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    }
}

/// One bounded batch of input received before a publication attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InputBatch {
    pub(crate) first: u64,
    pub(crate) last: u64,
    pub(crate) count: u64,
    pub(crate) coalesced: u64,
    pub(crate) incomplete: bool,
    pub(crate) publication: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputSurface {
    Canvas,
    Settings,
    Images,
    Gallery,
    Other,
}

#[derive(Debug, Default)]
struct PendingInput {
    first: Option<u64>,
    last: Option<u64>,
    count: u64,
    coalesced: u64,
}

impl PendingInput {
    fn record(&mut self, sequence: u64) {
        self.first.get_or_insert(sequence);
        self.last = Some(sequence);
        self.count = self.count.saturating_add(1);
    }

    fn drain(&mut self, incomplete: bool, publication: u64) -> Option<InputBatch> {
        let batch = InputBatch {
            first: self.first?,
            last: self.last.expect("pending input has a last sequence"),
            count: self.count,
            coalesced: self.coalesced,
            incomplete,
            publication,
        };
        *self = Self::default();
        Some(batch)
    }
}

/// Scalar input accounting kept independently of the rendering toolkit.
#[derive(Debug, Default)]
pub(crate) struct InputTrace {
    next: u64,
    publication: u64,
    canvas: PendingInput,
    settings: PendingInput,
    images: PendingInput,
    gallery: PendingInput,
    other: PendingInput,
    destroyed_windows: u64,
    incomplete: bool,
}

impl InputTrace {
    pub(crate) fn record_input(&mut self, surface: InputSurface) -> u64 {
        if self.next == u64::MAX {
            self.incomplete = true;
            return self.next;
        }
        self.next = self.next.saturating_add(1);
        let sequence = self.next;
        self.pending_mut(surface).record(sequence);
        sequence
    }

    pub(crate) fn record_shared_input(
        &mut self,
        primary: InputSurface,
        secondary: InputSurface,
    ) -> u64 {
        if self.next == u64::MAX {
            self.incomplete = true;
            return self.next;
        }
        let sequence = self.record_input(primary);
        if primary != secondary {
            self.pending_mut(secondary).record(sequence);
        }
        sequence
    }

    pub(crate) fn record_coalesced(&mut self, surface: InputSurface) {
        let pending = self.pending_mut(surface);
        pending.coalesced = pending.coalesced.saturating_add(1);
    }

    pub(crate) fn record_destroyed_window(&mut self) -> [Option<InputBatch>; 5] {
        self.destroyed_windows = self.destroyed_windows.saturating_add(1);
        self.finish()
    }

    pub(crate) fn publish(&mut self, surface: InputSurface) -> Option<InputBatch> {
        self.pending(surface).first?;
        self.publication = self.publication.saturating_add(1);
        let incomplete = self.incomplete;
        let publication = self.publication;
        self.pending_mut(surface).drain(incomplete, publication)
    }

    pub(crate) fn finish(&mut self) -> [Option<InputBatch>; 5] {
        [
            self.canvas.drain(self.incomplete, 0),
            self.settings.drain(self.incomplete, 0),
            self.images.drain(self.incomplete, 0),
            self.gallery.drain(self.incomplete, 0),
            self.other.drain(self.incomplete, 0),
        ]
    }

    fn pending(&self, surface: InputSurface) -> &PendingInput {
        match surface {
            InputSurface::Canvas => &self.canvas,
            InputSurface::Settings => &self.settings,
            InputSurface::Images => &self.images,
            InputSurface::Gallery => &self.gallery,
            InputSurface::Other => &self.other,
        }
    }

    fn pending_mut(&mut self, surface: InputSurface) -> &mut PendingInput {
        match surface {
            InputSurface::Canvas => &mut self.canvas,
            InputSurface::Settings => &mut self.settings,
            InputSurface::Images => &mut self.images,
            InputSurface::Gallery => &mut self.gallery,
            InputSurface::Other => &mut self.other,
        }
    }

    #[cfg(test)]
    pub(crate) fn destroyed_windows(&self) -> u64 {
        self.destroyed_windows
    }
}

pub(crate) fn record_input(surface: InputSurface) -> u64 {
    if timing_enabled() {
        INPUT_TRACE.with(|trace| trace.borrow_mut().record_input(surface))
    } else {
        0
    }
}

pub(crate) fn record_shared_input(primary: InputSurface, secondary: InputSurface) -> u64 {
    if timing_enabled() {
        INPUT_TRACE.with(|trace| trace.borrow_mut().record_shared_input(primary, secondary))
    } else {
        0
    }
}

pub(crate) fn input_trace_incomplete() -> bool {
    timing_enabled().then(|| INPUT_TRACE.with(|trace| trace.borrow().incomplete)) == Some(true)
}

pub(crate) fn record_coalesced_input(surface: InputSurface) {
    if timing_enabled() {
        INPUT_TRACE.with(|trace| trace.borrow_mut().record_coalesced(surface));
    }
}

pub(crate) fn record_destroyed_window() -> [Option<InputBatch>; 5] {
    if timing_enabled() {
        return INPUT_TRACE.with(|trace| trace.borrow_mut().record_destroyed_window());
    }
    [None, None, None, None, None]
}

pub(crate) fn take_published_input(surface: InputSurface) -> Option<InputBatch> {
    timing_enabled().then(|| INPUT_TRACE.with(|trace| trace.borrow_mut().publish(surface)))?
}

pub(crate) fn finish_input_batches() -> [Option<InputBatch>; 5] {
    if timing_enabled() {
        INPUT_TRACE.with(|trace| trace.borrow_mut().finish())
    } else {
        [None, None, None, None, None]
    }
}

#[cfg(test)]
mod tests {
    use super::{InputBatch, InputSurface, InputTrace};

    #[test]
    fn input_trace_keeps_bursts_and_terminal_drains_disjoint() {
        let mut trace = InputTrace::default();

        trace.record_input(InputSurface::Settings);
        trace.record_input(InputSurface::Images);
        trace.record_input(InputSurface::Gallery);
        assert_eq!(
            trace.publish(InputSurface::Canvas),
            None,
            "a canvas repaint cannot publish input that belongs to another surface"
        );
        assert_eq!(
            trace.publish(InputSurface::Settings),
            Some(InputBatch {
                first: 1,
                last: 1,
                count: 1,
                coalesced: 0,
                incomplete: false,
                publication: 1,
            })
        );
        assert_eq!(
            trace.publish(InputSurface::Images),
            Some(InputBatch {
                first: 2,
                last: 2,
                count: 1,
                coalesced: 0,
                incomplete: false,
                publication: 2,
            })
        );
        assert_eq!(
            trace.publish(InputSurface::Gallery),
            Some(InputBatch {
                first: 3,
                last: 3,
                count: 1,
                coalesced: 0,
                incomplete: false,
                publication: 3,
            })
        );

        trace.record_input(InputSurface::Canvas);
        assert_eq!(
            trace.record_destroyed_window(),
            [
                Some(InputBatch {
                    first: 4,
                    last: 4,
                    count: 1,
                    coalesced: 0,
                    incomplete: false,
                    publication: 0,
                }),
                None,
                None,
                None,
                None
            ]
        );
        assert_eq!(trace.finish(), [None, None, None, None, None]);
        assert_eq!(trace.destroyed_windows(), 1);

        trace.record_input(InputSurface::Canvas);
        assert_eq!(
            trace.publish(InputSurface::Canvas),
            Some(InputBatch {
                first: 5,
                last: 5,
                count: 1,
                coalesced: 0,
                incomplete: false,
                publication: 4,
            })
        );
        trace.record_input(InputSurface::Settings);
        assert_eq!(
            trace.finish(),
            [
                None,
                Some(InputBatch {
                    first: 6,
                    last: 6,
                    count: 1,
                    coalesced: 0,
                    incomplete: false,
                    publication: 0,
                }),
                None,
                None,
                None
            ]
        );

        trace.next = u64::MAX - 1;
        assert_eq!(trace.record_input(InputSurface::Settings), u64::MAX);
        assert_eq!(trace.record_input(InputSurface::Settings), u64::MAX);
        assert_eq!(
            trace.finish(),
            [
                None,
                Some(InputBatch {
                    first: u64::MAX,
                    last: u64::MAX,
                    count: 1,
                    coalesced: 0,
                    incomplete: true,
                    publication: 0,
                }),
                None,
                None,
                None
            ]
        );
    }

    #[test]
    fn input_trace_shares_one_callback_sequence_across_two_surfaces() {
        let mut trace = InputTrace::default();

        assert_eq!(
            trace.record_shared_input(InputSurface::Gallery, InputSurface::Canvas),
            1
        );
        assert_eq!(
            trace.publish(InputSurface::Gallery),
            Some(InputBatch {
                first: 1,
                last: 1,
                count: 1,
                coalesced: 0,
                incomplete: false,
                publication: 1,
            })
        );
        assert_eq!(
            trace.publish(InputSurface::Canvas),
            Some(InputBatch {
                first: 1,
                last: 1,
                count: 1,
                coalesced: 0,
                incomplete: false,
                publication: 2,
            })
        );
    }

    #[test]
    fn input_trace_keeps_coalesced_bursts_across_two_drains() {
        let mut trace = InputTrace::default();

        trace.record_input(InputSurface::Canvas);
        trace.record_input(InputSurface::Canvas);
        trace.record_coalesced(InputSurface::Canvas);
        trace.record_coalesced(InputSurface::Canvas);
        assert_eq!(
            trace.publish(InputSurface::Canvas),
            Some(InputBatch {
                first: 1,
                last: 2,
                count: 2,
                coalesced: 2,
                incomplete: false,
                publication: 1,
            })
        );

        trace.record_input(InputSurface::Canvas);
        trace.record_coalesced(InputSurface::Canvas);
        assert_eq!(
            trace.finish(),
            [
                Some(InputBatch {
                    first: 3,
                    last: 3,
                    count: 1,
                    coalesced: 1,
                    incomplete: false,
                    publication: 0,
                }),
                None,
                None,
                None,
                None,
            ]
        );
    }
}
