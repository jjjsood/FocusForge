// SPDX-License-Identifier: GPL-3.0-or-later

//! The diagnostics panel's own state, as plain Rust -- no `use slint::`
//! anywhere in this module, the same seam every other view type in this
//! crate keeps (`crate::input_map`, `crate::viewport`). The panel binds a
//! [`DiagnosticsView`](crate::diagnostics::DiagnosticsView); the controller
//! is what builds one, from `focusforge_diagnostics::bundle`'s
//! manifest-then-write API.

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
