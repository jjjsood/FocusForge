// SPDX-License-Identifier: GPL-3.0-or-later

//! The session's retained notifications and the rules that decide which of
//! them reach the user as a toast.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long an ordinary toast stays up.
pub const TOAST_DURATION: Duration = Duration::from_secs(6);
/// How long a crash toast stays up.
pub const CRASH_TOAST_DURATION: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Info,
    Warning,
    Failure,
    Crash,
}

impl Kind {
    fn toast_duration(self) -> Duration {
        match self {
            Kind::Crash => CRASH_TOAST_DURATION,
            _ => TOAST_DURATION,
        }
    }
}

/// Where in the app a notification arose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Run,
    Save,
    Import,
    Startup,
}

/// Unreported crash evidence a [`Kind::Crash`] record stands for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrashEvidence {
    /// The crash ids still to be marked reported.
    pub ids: Vec<String>,
    /// How many crash reports the record stands for; kept when
    /// acknowledgement clears `ids`.
    pub reports: usize,
    pub acknowledged: bool,
    /// Why the last attempt to mark the evidence reported failed.
    pub error: Option<String>,
    /// The crash reports could not be located or read.
    pub unavailable: bool,
}

/// One user-facing event, kept for the session. Appended, never edited or
/// removed, so a later event, a retry or a tab switch never drops what an
/// earlier one recorded.
#[derive(Debug, Clone)]
pub struct Notification {
    /// Assigned on publication, from 1, never reused.
    pub id: u64,
    pub kind: Kind,
    pub source: Source,
    /// The short summary a toast shows.
    pub title: String,
    /// The complete text.
    pub message: String,
    pub project_title: Option<String>,
    pub project_path: Option<PathBuf>,
    /// The save target of a failed save.
    pub target: Option<PathBuf>,
    /// The source file a frame notice names.
    pub frame: Option<String>,
    /// Local `HH:MM` of the publication.
    pub at: String,
    pub read: bool,
    /// Set when the event arrived while the window was in the background and
    /// is due a native banner.
    pub native: bool,
    pub crash: Option<CrashEvidence>,
}

impl Notification {
    pub fn new(
        kind: Kind,
        source: Source,
        title: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Notification {
            id: 0,
            kind,
            source,
            title: title.into(),
            message: message.into(),
            project_title: None,
            project_path: None,
            target: None,
            frame: None,
            at: String::new(),
            read: false,
            native: false,
            crash: None,
        }
    }
}

/// What an operating-system banner shows: the record's title and its context
/// line, never the full message or a full path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Banner {
    pub title: String,
    pub body: String,
}

/// The toast on screen. `deadline` is `None` while paused, with the time
/// left in `remaining`.
#[derive(Debug, Clone, Copy)]
struct Shown {
    id: u64,
    remaining: Duration,
    deadline: Option<Instant>,
}

/// The retained records, the toast queue and the routing state. Time enters
/// as an argument, so a caller drives the clock.
#[derive(Debug)]
pub struct Notifications {
    records: Vec<Notification>,
    queue: VecDeque<u64>,
    shown: Option<Shown>,
    projects_open: bool,
    center_open: bool,
    focused: bool,
    focused_windows: HashSet<u64>,
    selected: Option<u64>,
    expanded: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Notifications {
            records: Vec::new(),
            queue: VecDeque::new(),
            shown: None,
            projects_open: false,
            center_open: false,
            focused: true,
            focused_windows: HashSet::new(),
            selected: None,
            expanded: false,
        }
    }
}

impl Notifications {
    /// Every record, oldest first.
    pub fn records(&self) -> &[Notification] {
        &self.records
    }

    pub(crate) fn records_mut(&mut self) -> &mut [Notification] {
        &mut self.records
    }

    pub fn get(&self, id: u64) -> Option<&Notification> {
        self.records.iter().find(|n| n.id == id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Notification> {
        self.records.iter_mut().find(|n| n.id == id)
    }

    pub fn unread(&self) -> usize {
        self.records.iter().filter(|n| !n.read).count()
    }

    pub fn center_open(&self) -> bool {
        self.center_open
    }

    /// The record selected in the center.
    pub fn selected(&self) -> Option<u64> {
        self.selected
    }

    /// Whether the selected record shows its full message.
    pub fn expanded(&self) -> bool {
        self.expanded
    }

    /// Selects record `id`, collapsed. An unknown id changes nothing.
    pub fn select(&mut self, id: u64) {
        if self.get(id).is_some() {
            self.selected = Some(id);
            self.expanded = false;
        }
    }

    /// Moves the selection `delta` rows down the center's list (newest
    /// first), stopping at the first and last record. With nothing selected
    /// it selects the first row.
    pub fn move_selection(&mut self, delta: isize) {
        let count = self.records.len();
        if count == 0 {
            return;
        }
        let current = self
            .selected
            .and_then(|id| self.records.iter().rev().position(|n| n.id == id));
        let next = match current {
            Some(row) => row.saturating_add_signed(delta).min(count - 1),
            None => 0,
        };
        if current != Some(next) {
            self.select(self.records[count - 1 - next].id);
        }
    }

    /// Shows or hides the selected record's full message.
    pub fn toggle_expanded(&mut self) {
        if self.selected.is_some() {
            self.expanded = !self.expanded;
        }
    }

    /// Opens the center on record `id`, selected and expanded.
    pub fn open_center_at(&mut self, id: u64) {
        self.open_center();
        if self.get(id).is_some() {
            self.selected = Some(id);
            self.expanded = true;
        }
    }

    /// The toast currently on screen.
    pub fn visible(&self) -> Option<&Notification> {
        self.shown.and_then(|s| self.get(s.id))
    }

    /// When the visible toast closes; `None` with no toast or while paused.
    pub fn deadline(&self) -> Option<Instant> {
        self.shown.and_then(|s| s.deadline)
    }

    /// Appends `record`, routes it and returns its id. Projects open: unread
    /// and nothing shown. Window in the background: unread with a native
    /// delivery mark. Center open: read. Otherwise unread and queued as a
    /// toast.
    pub fn publish(&mut self, mut record: Notification, now: Instant) -> u64 {
        let id = self.records.last().map_or(1, |n| n.id + 1);
        record.id = id;
        record.read = false;
        record.native = false;
        if !self.projects_open {
            if !self.focused {
                record.native = true;
            } else if self.center_open {
                record.read = true;
            } else {
                self.queue.push_back(id);
            }
        }
        self.records.push(record);
        self.promote(now);
        id
    }

    /// Closes the visible toast when its own deadline has passed and shows
    /// the next queued one. A deadline belonging to an earlier toast closes
    /// nothing.
    pub fn expire_due(&mut self, now: Instant) {
        if self
            .shown
            .is_some_and(|s| s.deadline.is_some_and(|d| d <= now))
        {
            self.shown = None;
            self.promote(now);
        }
    }

    /// Closes the visible toast `id` before its deadline.
    pub fn dismiss(&mut self, id: u64, now: Instant) {
        if self.shown.is_some_and(|s| s.id == id) {
            self.shown = None;
            self.promote(now);
        }
    }

    /// Freezes the visible toast `id` with the time it has left.
    pub fn pause(&mut self, id: u64, now: Instant) {
        if let Some(shown) = self.shown.as_mut().filter(|s| s.id == id)
            && let Some(deadline) = shown.deadline.take()
        {
            shown.remaining = deadline.saturating_duration_since(now);
        }
    }

    /// Restarts a paused toast `id` with the time it had left.
    pub fn resume(&mut self, id: u64, now: Instant) {
        if let Some(shown) = self.shown.as_mut().filter(|s| s.id == id)
            && shown.deadline.is_none()
        {
            shown.deadline = Some(now + shown.remaining);
        }
    }

    /// Opens the center: every record becomes read and no toast stays up or
    /// queued.
    pub fn open_center(&mut self) {
        self.center_open = true;
        self.mark_all_read();
        self.suppress();
    }

    pub fn close_center(&mut self) {
        self.center_open = false;
    }

    /// Records whether the Projects overview is open. Opening it closes the
    /// center without reading anything and drops every toast; closing it
    /// brings nothing back.
    pub fn set_projects_open(&mut self, open: bool) {
        self.projects_open = open;
        if open {
            self.center_open = false;
            self.suppress();
        }
    }

    /// Records whether the app window has focus. Regaining it with the center
    /// open reads what arrived meanwhile.
    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
        if focused && self.center_open {
            self.mark_all_read();
        }
    }

    /// Records that app window `window` gained or lost focus. The app counts
    /// as focused while any window holds it, so focus moving between its own
    /// windows is not a background transition.
    pub fn set_window_focused(&mut self, window: u64, focused: bool) {
        if focused {
            self.focused_windows.insert(window);
        } else {
            self.focused_windows.remove(&window);
        }
        self.set_focused(!self.focused_windows.is_empty());
    }

    /// The native banner due for every record marked for one, each handed out
    /// once.
    pub fn take_native(&mut self) -> Vec<Banner> {
        let mut banners = Vec::new();
        for record in &mut self.records {
            if std::mem::take(&mut record.native) {
                banners.push(Banner {
                    title: record.title.clone(),
                    body: context_line(record),
                });
            }
        }
        banners
    }

    /// Drops the visible and queued toasts; the records stay.
    fn suppress(&mut self) {
        self.queue.clear();
        self.shown = None;
    }

    fn mark_all_read(&mut self) {
        for record in &mut self.records {
            record.read = true;
        }
    }

    /// Shows the queue head when no toast is up; its own kind sets its
    /// duration.
    fn promote(&mut self, now: Instant) {
        if self.shown.is_some() {
            return;
        }
        while let Some(id) = self.queue.pop_front() {
            if let Some(record) = self.get(id) {
                let remaining = record.kind.toast_duration();
                self.shown = Some(Shown {
                    id,
                    remaining,
                    deadline: Some(now + remaining),
                });
                return;
            }
        }
    }
}

/// A row's context line: the source (omitted for a run), the project title,
/// the frame file, save-target file or crash report count, and the time,
/// each present part joined by ` · `.
pub fn context_line(record: &Notification) -> String {
    let source = match record.source {
        Source::Run => None,
        Source::Save => Some("Save".to_string()),
        Source::Import => Some("Import".to_string()),
        Source::Startup => Some("Startup".to_string()),
    };
    let file = |path: &PathBuf| path.file_name().map(|n| n.to_string_lossy().into_owned());
    let detail = record
        .frame
        .clone()
        .or_else(|| record.target.as_ref().and_then(file))
        .or_else(|| {
            record
                .crash
                .as_ref()
                .filter(|crash| crash.reports > 0)
                .map(|crash| match crash.reports {
                    1 => "1 crash report".to_string(),
                    n => format!("{n} crash reports"),
                })
        });
    let time = (!record.at.is_empty()).then(|| record.at.clone());
    [source, record.project_title.clone(), detail, time]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(kind: Kind) -> Notification {
        Notification::new(kind, Source::Run, "title", "message")
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn shown_id(store: &Notifications) -> Option<u64> {
        store.visible().map(|n| n.id)
    }

    #[test]
    fn toasts_show_one_at_a_time_in_arrival_order() {
        let t0 = Instant::now();
        let mut store = Notifications::default();
        let info = store.publish(note(Kind::Info), t0);
        let crash = store.publish(note(Kind::Crash), t0);
        let failure = store.publish(note(Kind::Failure), t0);
        assert_eq!(shown_id(&store), Some(info));
        store.expire_due(t0 + secs(5));
        assert_eq!(shown_id(&store), Some(info), "6 s not yet over");
        store.expire_due(t0 + secs(6));
        assert_eq!(shown_id(&store), Some(crash));
        store.expire_due(t0 + secs(15));
        assert_eq!(shown_id(&store), Some(crash), "the crash holds 10 s");
        store.expire_due(t0 + secs(16));
        assert_eq!(shown_id(&store), Some(failure));
        store.expire_due(t0 + secs(21));
        assert_eq!(shown_id(&store), Some(failure));
        store.expire_due(t0 + secs(22));
        assert_eq!(shown_id(&store), None);
        assert_eq!(store.deadline(), None);
    }

    #[test]
    fn a_stale_deadline_never_closes_the_next_toast() {
        let t0 = Instant::now();
        let mut store = Notifications::default();
        let first = store.publish(note(Kind::Info), t0);
        let second = store.publish(note(Kind::Info), t0);
        store.dismiss(first, t0 + secs(1));
        assert_eq!(shown_id(&store), Some(second));
        // The first toast's own timer fires at its original deadline.
        store.expire_due(t0 + secs(6));
        assert_eq!(shown_id(&store), Some(second));
        store.expire_due(t0 + secs(7));
        assert_eq!(shown_id(&store), None);
    }

    #[test]
    fn pause_keeps_the_remaining_time() {
        let t0 = Instant::now();
        let mut store = Notifications::default();
        let id = store.publish(note(Kind::Info), t0);
        store.pause(id, t0 + secs(4));
        assert_eq!(store.deadline(), None);
        store.expire_due(t0 + secs(60));
        assert_eq!(shown_id(&store), Some(id), "a paused toast never expires");
        store.resume(id, t0 + secs(100));
        assert_eq!(store.deadline(), Some(t0 + secs(102)));
        store.expire_due(t0 + secs(101));
        assert_eq!(shown_id(&store), Some(id));
        store.expire_due(t0 + secs(102));
        assert_eq!(shown_id(&store), None);
    }

    #[test]
    fn routing_follows_projects_center_and_focus() {
        let t0 = Instant::now();
        for projects in [false, true] {
            for center in [false, true] {
                for focused in [true, false] {
                    let mut store = Notifications::default();
                    store.set_focused(focused);
                    if center {
                        store.open_center();
                    }
                    // Opening the overview closes the center again.
                    store.set_projects_open(projects);
                    let id = store.publish(note(Kind::Failure), t0);
                    let record = store.get(id).unwrap();
                    let center_visible = store.center_open();
                    let want_read = !projects && focused && center_visible;
                    let want_toast = !projects && focused && !center_visible;
                    let want_native = !projects && !focused;
                    let case = format!("projects {projects} center {center} focused {focused}");
                    assert_eq!(record.read, want_read, "read: {case}");
                    assert_eq!(store.visible().is_some(), want_toast, "toast: {case}");
                    assert_eq!(record.native, want_native, "native: {case}");
                }
            }
        }
    }

    #[test]
    fn opening_the_center_reads_all_and_drops_queued_toasts() {
        let t0 = Instant::now();
        let mut store = Notifications::default();
        store.publish(note(Kind::Info), t0);
        store.publish(note(Kind::Warning), t0);
        store.publish(note(Kind::Failure), t0);
        assert_eq!(store.unread(), 3);
        store.open_center();
        assert_eq!(store.unread(), 0);
        assert!(store.visible().is_none());
        assert_eq!(store.records().len(), 3, "history stays");
        store.close_center();
        store.expire_due(t0 + secs(60));
        assert!(store.visible().is_none(), "no queued toast comes back");
    }

    #[test]
    fn opening_projects_closes_the_center_without_reading() {
        let t0 = Instant::now();
        let mut store = Notifications::default();
        store.set_focused(false);
        store.open_center();
        let id = store.publish(note(Kind::Info), t0);
        assert!(!store.get(id).unwrap().read, "background arrival is unread");
        store.set_projects_open(true);
        assert!(!store.center_open());
        assert_eq!(store.unread(), 1);
        store.set_projects_open(false);
        assert!(store.visible().is_none(), "nothing replays");
        assert!(!store.center_open());
    }

    #[test]
    fn refocus_with_the_center_open_reads_background_arrivals() {
        let t0 = Instant::now();
        let mut store = Notifications::default();
        store.open_center();
        store.set_focused(false);
        let id = store.publish(note(Kind::Warning), t0);
        let record = store.get(id).unwrap();
        assert!(!record.read && record.native);
        store.set_focused(true);
        assert!(store.get(id).unwrap().read);
        assert_eq!(store.unread(), 0);
        assert!(store.visible().is_none(), "no toast replay");
    }

    #[test]
    fn ids_stay_unique_and_stable_across_reads() {
        let t0 = Instant::now();
        let mut store = Notifications::default();
        let a = store.publish(note(Kind::Info), t0);
        let b = store.publish(note(Kind::Info), t0);
        store.open_center();
        let c = store.publish(note(Kind::Info), t0);
        assert_eq!((a, b, c), (1, 2, 3));
        let ids: Vec<u64> = store.records().iter().map(|n| n.id).collect();
        assert_eq!(ids, [1, 2, 3]);
    }

    #[test]
    fn open_center_at_selects_and_expands_that_record() {
        let t0 = Instant::now();
        let mut store = Notifications::default();
        let ids: Vec<u64> = (0..5)
            .map(|_| store.publish(note(Kind::Info), t0))
            .collect();
        store.open_center_at(ids[1]);
        assert!(store.center_open());
        assert_eq!(store.selected(), Some(ids[1]), "not the newest");
        assert!(store.expanded());
        assert_eq!(store.unread(), 0);
        store.open_center_at(999);
        assert_eq!(
            store.selected(),
            Some(ids[1]),
            "an unknown id changes nothing"
        );
    }

    #[test]
    fn selection_moves_and_clamps_at_both_ends() {
        let t0 = Instant::now();
        let mut store = Notifications::default();
        let ids: Vec<u64> = (0..3)
            .map(|_| store.publish(note(Kind::Info), t0))
            .collect();
        store.move_selection(1);
        assert_eq!(
            store.selected(),
            Some(ids[2]),
            "the first row is the newest"
        );
        store.move_selection(-1);
        assert_eq!(
            store.selected(),
            Some(ids[2]),
            "Up at the first row stays put"
        );
        store.move_selection(1);
        store.move_selection(1);
        assert_eq!(store.selected(), Some(ids[0]));
        store.move_selection(1);
        assert_eq!(
            store.selected(),
            Some(ids[0]),
            "Down at the last row stays put"
        );
        store.toggle_expanded();
        store.move_selection(-1);
        assert!(!store.expanded(), "a new selection starts collapsed");
    }

    #[test]
    fn context_line_names_project_frame_and_time() {
        let mut brightness = note(Kind::Warning);
        brightness.project_title = Some("Mantis".into());
        brightness.frame = Some("02.tif".into());
        brightness.at = "16:41".into();
        assert_eq!(context_line(&brightness), "Mantis · 02.tif · 16:41");

        let mut save = Notification::new(Kind::Failure, Source::Save, "t", "m");
        save.project_title = Some("Mantis".into());
        save.target = Some(PathBuf::from("/data/out/mantis.ffp"));
        save.at = "16:43".into();
        assert_eq!(context_line(&save), "Save · Mantis · mantis.ffp · 16:43");

        let mut import = Notification::new(Kind::Info, Source::Import, "t", "m");
        import.project_title = Some("Mantis".into());
        import.at = "16:42".into();
        assert_eq!(context_line(&import), "Import · Mantis · 16:42");

        let mut crash = Notification::new(Kind::Crash, Source::Startup, "t", "m");
        crash.at = "16:40".into();
        crash.crash = Some(CrashEvidence {
            reports: 1,
            ..CrashEvidence::default()
        });
        assert_eq!(context_line(&crash), "Startup · 1 crash report · 16:40");

        assert_eq!(
            context_line(&note(Kind::Info)),
            "",
            "missing parts are skipped"
        );
    }

    #[test]
    fn crash_context_keeps_its_count_after_acknowledgement() {
        let mut crash = Notification::new(Kind::Crash, Source::Startup, "t", "m");
        crash.crash = Some(CrashEvidence {
            ids: vec!["a".into(), "b".into()],
            reports: 2,
            ..CrashEvidence::default()
        });
        assert_eq!(context_line(&crash), "Startup · 2 crash reports");
        let evidence = crash.crash.as_mut().unwrap();
        evidence.ids.clear();
        evidence.acknowledged = true;
        assert_eq!(context_line(&crash), "Startup · 2 crash reports");
    }
}
