// SPDX-License-Identifier: GPL-3.0-or-later

//! Plain-Rust shell state and the callback-forwarding seam. No `use slint::`
//! anywhere in this file: every Slint callback in the crate forwards into
//! `AppController`, whose method signatures name no Slint type, so the
//! window's behaviour is reachable from `cargo test` without a window.
//!
//! The canvas methods below (`pane_label`, `undock_enabled`, `frame_count`,
//! `frame_draw`, `plane_draw`, ...) live here rather than in `crate::canvas`
//! so they can reach `ShellState`'s private fields; `crate::canvas` owns
//! only the plain types and pure functions. The entry path's own state
//! (`GuiProject`, `ProjectTab`, `pending`) is per-project.
//!
//! The input dispatch methods below (`wheel`, `pointer`, `pointer_moved`,
//! `key`, ...) are the **one** point in the crate that maps a gesture to an
//! `Action`. They read `crate::input_map`'s types only -- no `use slint::`
//! here either.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::brush;
use crate::canvas::{FrameView, OverlayDraw};
use crate::diagnostics::{
    DiagnosticsEntryView, DiagnosticsScan, DiagnosticsScanEntry, DiagnosticsStatus,
    DiagnosticsView, clamp_manifest_scroll, manifest_content_height, size_text,
};
use crate::entry;
use crate::images;
use crate::input_config::Provenance;
use crate::input_map::{
    Action, ActionEvent, Button, InputRowView, InputTable, Modifiers, PointerKind, Trigger,
};
use crate::job;
use crate::overlay::{self, CardView, FailureCard, FailureKind};
use crate::recent;
use crate::settings_panel::{
    self, PanelModel, ParamValue, ParamValues, ParamViolation, StackContext,
};
use crate::viewport::{ImageSize, PaneDraw, PaneRect, Viewport};

pub use crate::entry::{
    EntryError, GroupingQuestion, GuiProject, ImportRefused, OpenAs, OpenOutcome, Opened, Proposal,
};

/// The stacking/editor mode switch's two positions. Editor is reachable only
/// on a tab whose planes are fused and whose frame list still matches them --
/// `AppController::set_mode` and `editor_enabled` enforce that, not the
/// Slint side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShellMode {
    #[default]
    Stacking,
    Editor,
}

/// The dock's tab row (Images / Settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DockTab {
    #[default]
    Images,
    Settings,
}

/// The settings modal's nav-rail sections, top to bottom. Four of the six
/// are disabled and cannot be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsSection {
    General,
    Performance,
    Storage,
    #[default]
    InputMap,
    Diagnostics,
    About,
}

/// The two fields a checkpoint row shows: the fusion algorithm used and when
/// it ran. Left unpopulated until real checkpoint history exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointRow {
    pub algorithm: String,
    pub created_at: String,
}

/// Every event the shell can raise, reaching an installed sink through
/// [`AppController::set_sink`].
/// `PartialEq` only, not `Eq`, since one variant carries `f32`.
#[derive(Debug, Clone, PartialEq)]
pub enum ShellEvent {
    ModeChanged(ShellMode),
    DockTabSelected(DockTab),
    WindowDragRequested,
    WindowMinimizeRequested,
    /// Carries the new maximized state, so the bridge can push it straight
    /// into `AppWindow`'s `window-maximized` property (the glyph binding)
    /// without a second lock on the controller.
    WindowMaximizeToggled(bool),
    WindowCloseRequested,
    CheckpointExportRequested,
    /// Unreachable while `AppController::undock_enabled` stays `true`,
    /// which holds as long as both panes are docked.
    PaneUndockRequested(crate::canvas::PaneSource),
    /// Raised by the one input dispatch point (`wheel`, `pointer`,
    /// `pointer_moved`, `key`) every time a trigger resolves to an
    /// `Action`.
    Input(ActionEvent),
    /// Raised by [`AppController::start_bundle_write`]'s background thread
    /// once the write attempt has settled, carrying its outcome.
    DiagnosticsWriteFinished(DiagnosticsStatus),
    /// Raised by [`AppController::start_manifest_scan`]'s background thread
    /// once a scan has settled, carrying the generation it started from and
    /// the scan itself.
    DiagnosticsScanFinished {
        generation: u64,
        scan: DiagnosticsScan,
    },
    /// The open project set changed: a tab opened, closed or was selected.
    ProjectsChanged,
    /// Only the tab strip's scroll state moved -- its offset, overflow
    /// layout or arrow flags: an arrow click or a tabs-region resize.
    TabScrollChanged,
    /// A folder read as more than one stack -- the one grouping question,
    /// asked once.
    QuestionRaised(GroupingQuestion),
    /// The settings panel's own state moved: a value, an algorithm choice,
    /// a reset, an open menu or the scroll offset -- never a tab switch,
    /// which `ProjectsChanged` already rebuilds the panel for.
    SettingsChanged,
    /// An open attempt was refused. Carries its already-rendered message:
    /// `perform` (`crate::bridge`) must not format user-facing text.
    EntryRefused(String),
    /// A fuse run's state or progress moved -- started, paused, resumed,
    /// cancelled, failed or completed.
    JobChanged,
    /// The `Images` panel's own state moved: a row's exclusion, the frame
    /// order, the sort selection, the scroll offset, or a frame-set-change
    /// warning opened or closed -- never a tab switch, which
    /// `ProjectsChanged` already rebuilds the panel for.
    ImagesChanged,
    /// The editor toolbar's own state moved: the brush size, its softness,
    /// the pipette's armed state, or undo/redo availability.
    EditorChanged,
    /// The projects overlay's own state moved: opened, closed, its gallery
    /// scrolled, or its failure cards changed.
    OverlayChanged,
    /// A background load ([`AppController::open_paths`]) started.
    LoadStarted,
    /// A background load ticked: how many frames its worker has read of how
    /// many it found. The first tick a load raises carries the frame total;
    /// before it, the total is unknown.
    LoadProgress {
        frames_read: u32,
        frame_total: u32,
    },
    /// A background load's result is ready to settle
    /// ([`AppController::settle_open`]).
    LoadReady,
    /// An import scan ([`AppController::add_frames`]) is ready to settle
    /// ([`AppController::settle_import`]).
    ImportReady,
}

/// One live drag: which action it started and the last position seen, so
/// `pointer_moved` can difference successive positions into deltas
/// (slint-ui/slint#13189 -- `PointerEvent` carries no delta of its own).
#[derive(Debug, Clone, Copy, PartialEq)]
struct DragState {
    action: Action,
    last: (f32, f32),
}

/// The one dispatch point's own state: the active table, where it came
/// from, the controller-tracked `space` pseudo-modifier, any live drag,
/// every action fired so far with no implementation, and the two pieces of
/// state zoom/pan needs: the last known cursor position (`wheel` and
/// `pointer` both record it, so a drag and a wheel share one notion of
/// where the cursor is) and the wheel's own sub-detent accumulator.
#[derive(Debug, Clone, Default)]
struct InputState {
    table: InputTable,
    provenance: Provenance,
    space_held: bool,
    /// `Alt` as the last hover move reported it -- shows the pipette cursor
    /// while held; it arms nothing.
    alt_held: bool,
    drag: Option<DragState>,
    unimplemented: Vec<Action>,
    last_pointer: (f32, f32),
    /// Accumulated wheel delta not yet spent into a ladder step -- one stop
    /// per accumulated 60 px. Reset on a sign change so reversing direction
    /// never burns a leftover, and whenever the resolved action differs from
    /// `last_wheel_action`.
    wheel_accum: f32,
    /// The action the previous wheel detent spent into, so switching from
    /// one wheel-bound action to another mid-session never carries over a
    /// leftover fraction of a step.
    last_wheel_action: Option<Action>,
}

/// The brush's display state: `size` is a diameter in image px, `soft` a
/// percentage -- the controller's one conversion point to storage units is
/// [`AppController::brush_radius_softness`]. Every write clamps `size` to
/// `1..=` the active tab's ceiling, and every reader clamps it again, so a
/// tab switch to a smaller image takes effect on the next read without a
/// hook of its own.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BrushState {
    size: f32,
    soft: f32,
    picking: bool,
}

impl Default for BrushState {
    fn default() -> Self {
        Self {
            size: 64.0,
            soft: 30.0,
            picking: false,
        }
    }
}

/// The rainbow overlay's own state: window-wide, since the same values
/// render for whichever tab is active. Starts off, at 65 % once turned on.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OverlayState {
    on: bool,
    opacity: f32,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            on: false,
            opacity: 0.65,
        }
    }
}

/// `size = ceiling^fraction` -- the logarithmic slider mapping, so the
/// track's right end is always the ceiling.
fn size_from_fraction(fraction: f32, ceiling: u32) -> f32 {
    let ceiling = (ceiling.max(1)) as f32;
    if ceiling <= 1.0 {
        1.0
    } else {
        ceiling.powf(fraction.clamp(0.0, 1.0))
    }
}

/// [`size_from_fraction`]'s inverse -- the slider's own thumb position for
/// the toolbar's current `size`.
fn fraction_from_size(size: f32, ceiling: u32) -> f32 {
    let ceiling = (ceiling.max(1)) as f32;
    if ceiling <= 1.0 {
        0.0
    } else {
        (size.max(1.0).ln() / ceiling.ln()).clamp(0.0, 1.0)
    }
}

/// The editor toolbar's brush readout -- [`AppController::brush_toolbar_view`]'s
/// return value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrushToolbarView {
    /// A diameter in image px, already clamped to `size_ceiling`.
    pub size: f32,
    /// `min(width, height)` of the active tab's fused image.
    pub size_ceiling: u32,
    /// The logarithmic slider's own thumb position, `0.0..=1.0`.
    pub size_fraction: f32,
    /// A percentage, `0.0..=100.0`.
    pub soft: f32,
    /// The linear slider's own thumb position, `0.0..=1.0`.
    pub soft_fraction: f32,
    pub picking: bool,
    /// Whether the active tab's edit session has an op to undo/redo.
    pub can_undo: bool,
    pub can_redo: bool,
}

/// The bottom bar's overlay controls -- [`AppController::overlay_view`]'s
/// return value. `opacity` is `0.0..=1.0`; the bar's own percentage readout
/// is formatted by the caller.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayView {
    pub on: bool,
    pub opacity: f32,
}

/// The brush cursor overlay's own view -- [`AppController::brush_cursor_view`]'s
/// return value.
#[derive(Debug, Clone, PartialEq)]
pub struct BrushCursorView {
    pub extent: f32,
    pub ring_commands: String,
    pub core_commands: String,
    /// The pipette is armed or `Alt` is held.
    pub picking: bool,
    /// The colour a pick would land on right now -- the Z-Map source under
    /// the pointer while `picking`, or the current brush source otherwise --
    /// shown in the cursor's swatch.
    pub pick_color: [u8; 3],
    /// The right pane's `x` minus the left pane's, logical px: the offset
    /// that carries a cursor's centre from the pane it is over onto the
    /// other pane, at the same scene point.
    pub mirror_dx: f32,
}

/// The diagnostics panel's own state: the two directories a bundle is
/// built from, the include-minidumps choice, the write destination and
/// where a write attempt stands. The message shown for each status is
/// fixed wording, not stored state -- `diagnostics_view()` derives it.
#[derive(Debug, Clone, Default)]
struct DiagnosticsState {
    data_dir: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    include_minidumps: bool,
    destination: PathBuf,
    status: DiagnosticsStatus,
    /// The manifest list's scroll offset and the two measurements needed to
    /// clamp and draw it: its content height (cached from the last
    /// [`AppController::diagnostics_view`] build) and its viewport height
    /// (pushed by the list's own `viewport-height-changed`).
    manifest_scroll_y: f32,
    manifest_content_height: f32,
    manifest_viewport_height: f32,
    /// The grabbed point's offset from the thumb's top edge while a
    /// scrollbar drag is live, `None` otherwise.
    manifest_scrollbar_drag: Option<f32>,
    /// The destination field's horizontal scroll offset, pushed by
    /// [`AppController::diagnostics_destination_cursor_moved`] whenever the
    /// cursor would otherwise land outside the field's visible width.
    destination_scroll_x: f32,
    /// The crash notice's own snapshot: the unreported crash ids found at
    /// start and whether the notice is currently shown for them. Taken once
    /// by [`AppController::scan_crash_notice`]; nothing re-scans mid-session.
    crash_notice_ids: Vec<String>,
    crash_notice_visible: bool,
    /// Bumped by every [`AppController::start_manifest_scan`] call, so a
    /// report from a superseded scan can be told apart from the current one.
    scan_generation: u64,
    /// The last manifest scan to report, tagged with the generation it came
    /// from. `None` until the first scan completes; a stored generation
    /// behind [`Self::scan_generation`] means a rescan is in flight.
    scan: Option<(u64, DiagnosticsScan)>,
}

/// One tab: its project state, its title, and the per-tab image
/// size/viewport -- each tab carries its own zoom. `image` is set once when
/// the tab is created and never changes; `viewport` starts `None` and is
/// fitted by the first `set_pane_geometry` call after the tab becomes
/// active.
struct ProjectTab {
    /// Assigned once by a monotonic counter on [`AppController`] and never
    /// reused -- what the live-run slot and the queue identify a tab by, so
    /// a run stays attached to its own tab across a close that shifts every
    /// later index down.
    id: u64,
    project: GuiProject,
    title: String,
    image: Option<ImageSize>,
    viewport: Option<Viewport>,
    /// This tab's [`StackContext`], read once here -- the settings panel
    /// resolves `auto` against it on every render rather than re-opening a
    /// header. `None` only for an unreadable or frame-less group, which the
    /// entry path never actually produces.
    ctx: Option<StackContext>,
    /// The active Fuse/Align algorithm id, seeded from the first id-sorted
    /// registry entry of each family for an unsaved tab, or from the sealed
    /// manifest for a reopened one.
    fuse_id: String,
    align_id: String,
    /// Only the explicitly-set keys of each section -- `resolve_in` derives
    /// everything else. A reopened project's manifest values land here as
    /// explicit.
    fuse_values: ParamValues,
    align_values: ParamValues,
    project_values: ParamValues,
    /// One violation per offending `"<section>:<id>"` key -- what lets two
    /// fields carry a message at the same time.
    errors: BTreeMap<String, ParamViolation>,
    /// The one open picker/enum menu's `"<section>:<id>"` key, if any.
    menu_open: Option<String>,
    /// The project's real checkpoint count -- `0` for an `Unsaved` tab, the
    /// reopened project's own on-disk chain length for an `Opened` one,
    /// incremented on every successful run ([`AppController::settle_job`])
    /// rather than inferred from which [`GuiProject`] variant the tab
    /// happens to be in. Counts held checkpoints too.
    checkpoints: usize,
    /// Every checkpoint fused since the last save, oldest first -- in no
    /// `.ffp` until [`AppController::save_active_project`] writes them.
    held: Vec<job::HeldCheckpoint>,
    /// A completed run whose alignment left frames unregistered, waiting for
    /// [`AppController::keep_fused`] or [`AppController::discard_fused`];
    /// not counted in `checkpoints` or `held` until kept.
    parked: Option<ParkedRun>,
    /// The ordered frame set a fuse actually consumes -- session-only, seeded
    /// from [`entry::project_frames`] and re-seeded from the run's own frames
    /// on every successful run, so a fused tab's list always matches what it
    /// just fused. `excluded` is the same length, `true` at every position whose
    /// frame is left out of the next fuse without moving any other position.
    frames: Vec<PathBuf>,
    excluded: Vec<bool>,
    /// The frame the left canvas pane draws -- a position into `frames`,
    /// never an identity. Seeded to the first not-excluded position when the
    /// tab is created, and kept in step by every edit that moves or shortens
    /// `frames`.
    selected: usize,
    /// The sort menu's own selection -- `None` is the dragged state, where
    /// neither `Name` nor `Capture time` is checked. Cleared by a manual
    /// drag.
    frame_sort: Option<images::FrameSort>,
    /// This tab's own card picture -- primed once, right here for a fresh
    /// tab and again in [`AppController::settle_job`] from each fused
    /// composite. `Arc`-shared rather than cloned per
    /// push, since [`AppController::project_cards`] rebuilds far more often
    /// than a picture actually changes.
    thumbnail: Option<Arc<overlay::Thumbnail>>,
    /// This tab's own run: `Idle` until a fuse starts on it, and whatever it
    /// last settled to afterward -- read by every bottom-bar and card
    /// surface for the tab that owns it, regardless of which tab is active.
    run: job::RunState,
    /// The request behind this tab's own last run, kept for `Retry` --
    /// `None` until a run is started on this tab.
    request: Option<job::JobRequest>,
    /// This tab's own editor session, opened over its fused Z-Map on first
    /// entry into editor mode -- `None` until then, and cleared whenever a
    /// newer checkpoint replaces the planes it was opened over.
    edit: Option<job::EditState>,
    /// This tab's own failed, cancelled or sources-missing run, if its most
    /// recent attempt did not succeed -- `None` once a later success clears
    /// it. Keyed by tab rather than by path so it survives an Unsaved ->
    /// Opened identity change and disappears with the tab on close.
    failure: Option<TabFailure>,
}

/// A run outcome recorded against its own tab rather than the project's
/// identity path -- [`ProjectTab::failure`]'s payload.
struct TabFailure {
    kind: FailureKind,
    stage: String,
    reason: String,
}

/// A held checkpoint whose run diverged on `diverged`, by frame file name.
struct ParkedRun {
    held: job::HeldCheckpoint,
    diverged: Vec<String>,
}

/// Whether a project's own load worker attempted to decode its thumbnail --
/// [`ProjectTab::new`] must tell "no worker ran" from "a worker ran and the
/// decode failed" apart, since only the first retries synchronously.
enum ThumbnailSource {
    /// No worker ran for this project's own load.
    NotAttempted,
    /// A worker ran; `None` is a failed decode, not a missing attempt.
    Decoded(Option<Arc<overlay::Thumbnail>>),
}

impl ProjectTab {
    /// `thumbnail` reports what the load's own worker already did for this
    /// project. [`ThumbnailSource::NotAttempted`] falls back to decoding it
    /// right here; a worker's own failed decode opens the tab with none.
    fn new(mut project: GuiProject, id: u64, thumbnail: ThumbnailSource) -> Self {
        let title = entry::tab_title(&project);
        let ctx = entry::tab_stack_context(&project);
        let image =
            entry::tab_image_size(&project).map(|(width, height)| ImageSize { width, height });
        let (fuse_id, fuse_values, align_id, align_values) =
            settings_panel::seed_algorithm_state(&project);
        let checkpoints = match &mut project {
            GuiProject::Unsaved(_) => 0,
            GuiProject::Opened(p) => p.checkpoint_chain().map(|chain| chain.len()).unwrap_or(1),
        };
        let frames = entry::project_frames(&project);
        let excluded = vec![false; frames.len()];
        let selected = images::first_included(&excluded);
        let thumbnail = match thumbnail {
            ThumbnailSource::Decoded(thumbnail) => thumbnail,
            ThumbnailSource::NotAttempted => {
                overlay::render_thumbnail(&mut project, &frames).map(Arc::new)
            }
        };
        ProjectTab {
            id,
            project,
            title,
            image,
            viewport: None,
            ctx,
            fuse_id,
            align_id,
            fuse_values,
            align_values,
            project_values: ParamValues::new(),
            errors: BTreeMap::new(),
            menu_open: None,
            checkpoints,
            held: Vec::new(),
            parked: None,
            frames,
            excluded,
            selected,
            frame_sort: None,
            thumbnail,
            run: job::RunState::Idle,
            request: None,
            edit: None,
            failure: None,
        }
    }

    /// Whether this tab holds work no `.ffp` contains: a never-saved
    /// project, or any checkpoint fused since the last save.
    fn unsaved(&self) -> bool {
        matches!(self.project, GuiProject::Unsaved(_)) || !self.held.is_empty()
    }

    /// Whether quitting now would lose a run of this tab: a held checkpoint,
    /// or a parked run still waiting for Keep or Discard.
    fn quit_loses_work(&self) -> bool {
        !self.held.is_empty() || self.parked.is_some()
    }

    /// The frames a fuse of this tab would actually use -- `frames` with
    /// every excluded position left out, order otherwise unchanged.
    fn included_frames(&self) -> Vec<PathBuf> {
        self.frames
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.excluded.get(*i).copied().unwrap_or(false))
            .map(|(_, path)| path.clone())
            .collect()
    }

    /// Whether this tab can enter editor mode: at least one checkpoint, its
    /// own run neither live nor waiting in the queue, and `frames` still
    /// naming the frame set the fused planes were built from -- the newest
    /// held checkpoint's own `sources`, or a reopened project's own manifest
    /// when nothing is held.
    fn can_edit(&self) -> bool {
        if self.checkpoints == 0 {
            return false;
        }
        if job::is_live(&self.run) || matches!(self.run, job::RunState::Queued { .. }) {
            return false;
        }
        let Some((fused, _)) = self.fused() else {
            return false;
        };
        self.frames == fused
    }

    /// The sources and registration behind this tab's newest composite --
    /// the newest held checkpoint's own `sources`/`registration`, or a
    /// reopened project's own manifest when nothing is held. `None` for an
    /// unsaved, never-fused tab.
    fn fused(&self) -> Option<(Vec<PathBuf>, job::Registration)> {
        match self.held.last() {
            Some(held) => Some((held.sources.clone(), held.registration.clone())),
            None => match &self.project {
                GuiProject::Opened(project) => Some((
                    entry::project_frames(&self.project),
                    project.manifest().registration.clone(),
                )),
                GuiProject::Unsaved(_) => None,
            },
        }
    }

    /// Permutes `frames` and `excluded` together by `order` (each entry the
    /// original index now at that position) -- what a manual drag and a sort
    /// both apply, so an exclusion stays attached to the frame it was set on
    /// rather than to a position. `selected` follows the same frame to its
    /// new position.
    fn reorder(&mut self, order: &[usize]) {
        let old_frames = std::mem::take(&mut self.frames);
        let old_excluded = std::mem::take(&mut self.excluded);
        self.frames = order.iter().map(|&i| old_frames[i].clone()).collect();
        self.excluded = order
            .iter()
            .map(|&i| old_excluded.get(i).copied().unwrap_or(false))
            .collect();
        self.selected = order
            .iter()
            .position(|&i| i == self.selected)
            .unwrap_or(self.selected);
    }

    /// The selected frame's path -- `None` only for an empty frame list.
    fn selected_frame(&self) -> Option<&PathBuf> {
        self.frames.get(self.selected)
    }
}

/// The tab strip's plain-Rust view of one tab -- the generated
/// `ProjectTabView` Slint struct's twin with no Slint type in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectTabRow {
    pub title: String,
    pub unsaved: bool,
    pub selected: bool,
    /// `""` idle, `"play"` running/paused/cancelling, `"ellipsis"` waiting
    /// -- which strip icon, if any, draws over this tab's own row.
    pub run_mark_icon: String,
    /// Whether pressing `Fuse stack` on this tab right now would enqueue it
    /// rather than start it.
    pub would_queue: bool,
}

/// The rail's plain-Rust view of one `Recent` row -- [`AppController::recent_rows`]'s
/// return type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentRow {
    pub name: String,
    pub folder: String,
    pub date_text: String,
}

/// What a chrome key press means -- [`AppController::chrome_key`]'s return
/// type. The picker and save variants need `crate::bridge`'s event-loop
/// thread, since a dialog cannot run inside the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeKey {
    /// Not a chrome key press -- falls through to [`AppController::key`].
    None,
    /// Already performed; nothing left for the bridge to do.
    Consumed,
    NewProject,
    OpenProject,
    /// Save the active tab's held checkpoints.
    SaveProject,
}

/// The close question's copy -- [`AppController::pending_close`]'s and
/// [`AppController::pending_quit_copy`]'s return type. A single-tab close
/// and a whole-window quit share one dialog, so the wording each proposes
/// -- including its title and footer labels -- travels with the copy
/// instead of living in `.slint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseGuardCopy {
    pub title: String,
    /// The line under the title naming the tab, or the window for a quit.
    pub context: String,
    pub body: String,
    /// Where the secondary answer writes.
    pub info: String,
    pub secondary_label: String,
    pub confirm_label: String,
}

/// The shell's state, carried `Send + Sync` so a worker thread inherits the
/// seam rather than re-cutting it.
#[derive(Default)]
struct ShellState {
    mode: ShellMode,
    dock_tab: DockTab,
    checkpoints: Vec<CheckpointRow>,
    maximized: bool,
    input: InputState,
    /// The editor toolbar's own brush state -- shared by every tab, so a
    /// stroke started in one editor session and finished after switching
    /// tabs is unreachable (`Paint`'s drag reads the active tab only).
    brush: BrushState,
    /// Every open project tab. Each tab owns its own image and viewport, so
    /// switching tabs keeps each tab's own zoom.
    projects: Vec<ProjectTab>,
    /// The selected tab; `None` means "+" is selected -- no project active,
    /// the empty state shown. Always `None` while `projects` is empty.
    active: Option<usize>,
    /// Both panes' current geometry, pushed by `set_pane_geometry`
    /// (`ui/shell/canvas.slint`'s five `out` properties, read once by
    /// `src/main.rs` at startup and again on every `canvas-resized`).
    panes: Option<(PaneRect, PaneRect)>,
    /// A grouping question raised by [`AppController::open_paths`], and the
    /// [`Proposal`] behind it -- both cleared by
    /// [`AppController::answer_question`] or
    /// [`AppController::cancel_question`], which is what makes "asked once"
    /// a property of the state machine rather than of the dialog.
    pending: Option<(GroupingQuestion, Proposal)>,
    /// The skipped-subfolder notice from the most recent open, if any.
    notice: Option<String>,
    /// The most recent refusal's already-rendered message, if any.
    last_refusal: Option<String>,
    /// Whether the "No frames here" refusal dialog is open -- raised by
    /// [`AppController::refuse_message`], cleared by
    /// [`AppController::dismiss_refusal`].
    refusal_open: bool,
    /// The tab strip's scroll offset in pixels, always within
    /// [`clamp_tab_scroll`]'s range for the current tabs and viewport.
    tab_scroll_x: f32,
    /// The tabs-region's width --
    /// [`AppController::set_tab_region_width`].
    tab_region_width: f32,
    /// The settings modal's visibility -- the only consumer of
    /// `--settings-section`. No `ShellEvent` variant needed:
    /// nothing else in the crate observes it, so `bridge.rs` reads the
    /// return value back directly rather than round-tripping the sink.
    settings_open: bool,
    /// The nav-rail section the modal shows, defaulting to the only one
    /// with content.
    settings_section: SettingsSection,
    diagnostics: DiagnosticsState,
    /// When the most recent close request was acted on --
    /// [`AppController::close_project_at`]'s debounce.
    last_close: Option<Instant>,
    /// The settings panel's scroll offset in pixels, always within
    /// [`settings_panel::clamp_panel_scroll`]'s range for the panel's
    /// current content and viewport.
    settings_scroll_y: f32,
    /// The settings panel's viewport height -- `FFSettingsPanel`'s own
    /// measured height, pushed on install and on every resize.
    settings_viewport_height: f32,
    /// Where a live scrollbar-thumb drag grabbed it, as an offset from the
    /// thumb's own top edge -- `None` when the pointer is up, or when it
    /// went down on the bare track (a jump, not a drag). Kept separate
    /// from `input_map`'s own `DragState`: this drag has no bindable
    /// action, no modifiers and no canvas gesture to route through.
    scrollbar_drag: Option<f32>,
    /// The `Images` panel's own scroll offset and viewport height --
    /// [`AppController::settings_scrollbar_thumb`]'s twin, kept separate
    /// since the two panels are never both the visible dock tab at once but
    /// each remembers its own scroll position across a tab switch.
    images_scroll_y: f32,
    images_viewport_height: f32,
    images_scrollbar_drag: Option<f32>,
    /// A frame-set-changing action waiting on its warning's `Cancel`/confirm
    /// -- raised only once the active tab already carries a checkpoint,
    /// cleared by [`AppController::confirm_frame_change`] or
    /// [`AppController::cancel_frame_change`].
    pending_frame_change: Option<PendingFrameChange>,
    /// Whether the `Images` panel's sort menu is open -- the same
    /// externally-driven convention `tab.menu_open` uses for a settings row.
    images_sort_menu_open: bool,
    /// A live grip drag, if any: which row it started on and the raw
    /// row-local `y` the grip reported at press time -- later `y` values
    /// minus this one, in whole [`images::ROW_HEIGHT`] steps, is how far
    /// the row has moved.
    drag_row: Option<DragRow>,
    /// Whether the projects overlay is open.
    overlay_open: bool,
    /// The tab, by id, whose close waits on the "save first?" answer --
    /// raised by [`AppController::request_close`] only when that tab holds
    /// checkpoints no `.ffp` contains.
    pending_close: Option<u64>,
    /// Whether a window quit waits on the "quit without saving?" answer --
    /// raised by [`AppController::window_close`] only when some tab holds
    /// checkpoints no `.ffp` contains. Mutually exclusive with
    /// `pending_close`: each is a no-op while the other question is open.
    pending_quit: bool,
    /// Every `Not a project` card -- a path with no open project behind it
    /// at all, cleared the moment that same path opens successfully. A
    /// failed, cancelled or sources-missing run instead lives on its own
    /// tab ([`ProjectTab::failure`]), since it always has an open project
    /// behind it.
    failures: Vec<FailureCard>,
    /// The gallery's own scroll offset and viewport height --
    /// [`ShellState::images_scroll_y`]'s twin.
    gallery_scroll_y: f32,
    gallery_viewport_height: f32,
    /// The gallery's measured content width -- what [`overlay::columns`]
    /// lays the grid out against.
    gallery_width: f32,
    gallery_scrollbar_drag: Option<f32>,
    /// The `Recent` list, most recent first -- loaded once at startup
    /// ([`AppController::install_recent`]) and appended to by every
    /// successful open. `None` config dir (its own resolution failed) means
    /// entries are tracked in memory for the session but never persisted.
    recent: Vec<recent::RecentEntry>,
    recent_config_dir: Option<PathBuf>,
    /// The active tab's planes changed underneath the open renderer cell --
    /// a background run settling on it or a close renumbering the tabs --
    /// so the next [`AppController::canvas_feed`] reloads even though the
    /// owner index still compares equal.
    canvas_dirty: bool,
    /// The in-flight background load, if any.
    load: LoadSlot,
    /// The in-flight additive import scan, if any.
    import: ImportSlot,
    /// The `Composite` pane's rainbow overlay -- window-wide (see
    /// [`OverlayState`]).
    overlay: OverlayState,
}

/// One load's worker result, parked here until
/// [`AppController::settle_open`] resolves it -- the way a completed fuse
/// run's own result is (`JobSlot`, below), minus the cancellation control a
/// fuse run needs: a load either finishes or fails.
#[derive(Default)]
struct LoadSlot {
    in_flight: bool,
    outcome: Arc<Mutex<Option<LoadResult>>>,
}

/// [`AppController::open_paths`]'s worker outcome, plus the thumbnail it
/// decoded for each opened project (in [`Opened::projects`] order, empty for
/// a [`OpenOutcome::Question`]/[`OpenOutcome::Refused`]) -- decoded on the
/// worker thread so [`AppController::settle_open`] never reads a frame's
/// pixels on the event-loop thread.
struct LoadResult {
    outcome: OpenOutcome,
    thumbnails: Vec<Option<Arc<overlay::Thumbnail>>>,
}

/// Decodes one thumbnail per project, in order -- what [`AppController::open_paths`]
/// and [`AppController::answer_question`] both run on their own worker thread.
fn decode_thumbnails(projects: &mut [GuiProject]) -> Vec<Option<Arc<overlay::Thumbnail>>> {
    projects
        .iter_mut()
        .map(|project| {
            let frames = entry::project_frames(project);
            overlay::render_thumbnail(project, &frames).map(Arc::new)
        })
        .collect()
}

/// One additive import's worker result, parked here until
/// [`AppController::settle_import`] resolves it -- [`LoadSlot`]'s twin for
/// [`AppController::add_frames`] rather than a fresh open.
#[derive(Default)]
struct ImportSlot {
    in_flight: bool,
    outcome: Arc<Mutex<Option<ImportScan>>>,
}

/// [`AppController::add_frames`]'s worker outcome: the tab it scanned, that
/// tab's frame list as of the scan's start, and the resolved import.
struct ImportScan {
    tab_id: u64,
    frames: Vec<PathBuf>,
    add: Vec<PathBuf>,
    replace: Vec<(usize, PathBuf)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DragRow {
    from: usize,
    press_y: f32,
    /// The grip's own `mouse_y` as of the most recent `Move` -- `press_y`
    /// until the first one arrives.
    current_y: f32,
}

/// An import's resolved shape, staged for [`AppController::apply_add_frames`]:
/// `add` are new, non-conflicting frames to append; `replace` is each
/// conflicting existing frame's own position in the tab's frame list, paired
/// with its incoming replacement.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ImportPlan {
    add: Vec<PathBuf>,
    replace: Vec<(usize, PathBuf)>,
}

impl ImportPlan {
    fn is_empty(&self) -> bool {
        self.add.is_empty() && self.replace.is_empty()
    }

    /// Drops every `replace` entry whose incoming path is already the frame
    /// at that position -- re-accepting a replacement with the very file
    /// already there changes nothing.
    fn without_same_path_replacements(mut self, existing: &[PathBuf]) -> Self {
        self.replace
            .retain(|(position, path)| existing.get(*position) != Some(path));
        self
    }
}

/// [`ShellState::pending_frame_change`]'s payload: which action is waiting,
/// and what it still needs applied once confirmed.
#[derive(Debug, Clone, PartialEq)]
enum PendingFrameChange {
    Add(ImportPlan),
    /// An import whose scan found conflicts -- the duplicate question, shown
    /// regardless of the tab's own checkpoint count.
    Duplicates(ImportPlan),
    Move {
        from: usize,
        to: usize,
    },
    Sort(crate::images::FrameSort),
    Remove(usize),
}

impl PendingFrameChange {
    fn kind(&self) -> crate::images::FrameSetChangeKind {
        match self {
            PendingFrameChange::Add(_) | PendingFrameChange::Duplicates(_) => {
                crate::images::FrameSetChangeKind::Add
            }
            PendingFrameChange::Move { .. } => crate::images::FrameSetChangeKind::Move,
            PendingFrameChange::Sort(_) => crate::images::FrameSetChangeKind::Sort,
            PendingFrameChange::Remove(_) => crate::images::FrameSetChangeKind::Remove,
        }
    }
}

impl ShellState {
    fn active(&self) -> Option<&ProjectTab> {
        self.active.and_then(|i| self.projects.get(i))
    }

    fn active_mut(&mut self) -> Option<&mut ProjectTab> {
        self.active.and_then(move |i| self.projects.get_mut(i))
    }
}

/// Whether `(x, y)` falls within `pane`'s rect -- [`AppController::anchor`]'s
/// only helper.
/// A short, stable digest of a path -- what keeps two different tabs that
/// happen to propose the same group name from fighting over one staging
/// directory. Not cryptographic: only uniqueness across concurrent runs on
/// this machine matters here.
pub(crate) fn path_digest(path: &std::path::Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

fn pane_contains(pane: PaneRect, x: f32, y: f32) -> bool {
    x >= pane.x && x <= pane.x + pane.width && y >= pane.y && y <= pane.y + pane.height
}

/// Widens `rect` by `margin` px on every side, clamped to `0..width` /
/// `0..height` -- the commit pass's bound between a stroke's raw preview
/// trace and its resampled, RDP'd committed path.
fn dilate_rect(rect: job::Rect, margin: u32, width: u32, height: u32) -> job::Rect {
    let x0 = rect.x.saturating_sub(margin);
    let y0 = rect.y.saturating_sub(margin);
    let x1 = (rect.x + rect.width + margin).min(width);
    let y1 = (rect.y + rect.height + margin).min(height);
    job::Rect {
        x: x0,
        y: y0,
        width: x1.saturating_sub(x0),
        height: y1.saturating_sub(y0),
    }
}

/// The extent of `tab`'s newest composite/Z-Map -- its newest held
/// checkpoint's, else its opened project's; `None` before any fuse.
fn fused_image_size(tab: &ProjectTab) -> Option<ImageSize> {
    match (tab.held.last(), &tab.project) {
        (Some(held), _) => Some(ImageSize {
            width: held.width,
            height: held.height,
        }),
        (None, GuiProject::Opened(project)) => Some(ImageSize {
            width: project.manifest().composite.width,
            height: project.manifest().composite.height,
        }),
        (None, GuiProject::Unsaved(_)) => None,
    }
}

/// `tab`'s own image size for every viewport computation: the newest
/// composite's extent once it has one ([`fused_image_size`]), else the raw
/// frame's -- so both panes and every zoom/pan/fit computation share one
/// size once a tab is fused.
fn display_image(tab: &ProjectTab) -> Option<ImageSize> {
    fused_image_size(tab).or(tab.image)
}

/// A one-sided margin so a scrolled cursor never sits flush against the
/// field's own edge, capped at half the visible width for a very narrow
/// field.
fn field_scroll_margin(viewport_width: f32) -> f32 {
    4.0_f32.min(viewport_width / 2.0).max(0.0)
}

/// The horizontal scroll offset that keeps a cursor at `cursor_x` (measured
/// from the field's unscrolled left edge) inside `[0, viewport_width]`,
/// shifting `current_offset` by the least amount needed --
/// [`AppController::diagnostics_destination_cursor_moved`]'s only helper.
fn clamp_field_scroll(cursor_x: f32, current_offset: f32, viewport_width: f32) -> f32 {
    let margin = field_scroll_margin(viewport_width);
    if cursor_x + current_offset < margin {
        margin - cursor_x
    } else if cursor_x + current_offset > viewport_width - margin {
        viewport_width - margin - cursor_x
    } else {
        current_offset
    }
}

/// The plain-Rust controller every Slint callback in the crate forwards to.
/// No method signature here names a Slint type.
#[derive(Default)]
pub struct AppController {
    state: ShellState,
    /// An `Arc`, not a `Box`, so [`Self::start_bundle_write`]'s thread can
    /// hold its own clone and call it once the write is done.
    sink: Option<Arc<dyn Fn(ShellEvent) + Send + Sync>>,
    job: JobSlot,
    /// Test-only override for the window's scratch root: unset, a run's
    /// ledger, staging, held checkpoints and plane cache live under the
    /// per-user data directory ([`AppController::scratch_root`]); set, they
    /// live under whatever directory the test handed in, so a test run never
    /// depends on -- or collides with -- a shared directory.
    scratch_root_for_test: Option<PathBuf>,
    /// Test-only override for the resident display tiles' edge, applied to
    /// every edit session opened afterwards.
    resident_tile_size_for_test: Option<u32>,
    /// The most recent run or save failure, taken exactly once by
    /// [`Self::take_settled_failure`] -- what lets the failure toast
    /// announce the run that actually failed without reopening itself on
    /// every later poll.
    settled_failure: Option<job::Notification>,
    /// Every run and save failure since the window opened, oldest first --
    /// append-only, so a later failure replacing the toast's own slot, a
    /// retry or a tab switch never drops what an earlier one recorded.
    notifications: Vec<job::Notification>,
    /// The last id handed to a [`ProjectTab`] -- incremented before each
    /// use, so ids start at 1 and no two tabs ever share one.
    next_tab_id: u64,
    /// Whether a fuse run may use the headless GPU backend at all -- `false`
    /// by default, so a headless test controller never probes an adapter;
    /// [`main`](crate) turns this on for the real window.
    gpu_fusion: bool,
}

/// The single live worker, if any: the control/observer pair a background
/// thread was handed, the tab it runs against, and the join handle
/// [`AppController::settle_job`] polls, plus the tabs waiting their turn. A
/// project's own run state and request live on its [`ProjectTab`] instead.
#[derive(Default)]
struct JobSlot {
    control: Option<Arc<job::JobControl>>,
    observer: Option<Arc<job::GuiObserver>>,
    tab_id: Option<u64>,
    handle: Option<std::thread::JoinHandle<Result<job::FuseOutcome, job::PipelineError>>>,
    /// Tab ids waiting for the live run to settle, in press order. The live
    /// run itself is position 1; the head of this queue is position 2.
    queue: VecDeque<u64>,
}

/// The toolbar/bottom-bar surfaces' plain-Rust view of the active run --
/// [`AppController::job_view`]'s return value.
#[derive(Debug, Clone, PartialEq)]
pub struct JobView {
    pub state: job::RunState,
    pub toolbar_run_state: String,
    pub stage_word: String,
    pub step_text: String,
    pub eta_text: String,
    pub bar_tone: String,
    pub fraction: f32,
    pub fuse_enabled: bool,
}

impl AppController {
    /// `tab_region_width` starts at its `ShellGeometry.base-width` value,
    /// not zero: a `changed` handler fires only for a *later* change, never
    /// for the width's initial value, so a zero default would read a
    /// freshly opened project's tab row as overflowing a strip nobody had
    /// measured yet. The real width still overwrites this the moment the
    /// window is ever resized.
    pub fn new() -> Self {
        let mut controller = Self::default();
        controller.state.tab_region_width = Self::BASE_TAB_REGION_WIDTH;
        controller
    }

    /// Test-only: a controller already in `mode` with no tab open, bypassing
    /// [`Self::set_mode`]'s check for a fused active tab -- for tests of
    /// editor-mode input dispatch that need no planes or edit session.
    pub fn new_with_mode_for_test(mode: ShellMode) -> Self {
        let mut controller = Self::new();
        controller.state.mode = mode;
        controller
    }

    /// Test-only: redirects the window's scratch root, so a test's ledger,
    /// staging and held checkpoints never depend on a directory other tests
    /// -- or a live window -- also write to.
    pub fn set_scratch_root_for_test(&mut self, root: PathBuf) {
        self.scratch_root_for_test = Some(root);
    }

    /// Test-only: grids every edit session opened afterwards at `size` px
    /// instead of the default, so a small image's strokes cross tile edges.
    pub fn set_resident_tile_size_for_test(&mut self, size: u32) {
        self.resident_tile_size_for_test = Some(size);
    }

    /// Drains the active tab's log of every region recomposited into its
    /// resident display tiles since the last call; empty with no edit
    /// session.
    pub fn take_recomposited(&mut self) -> Vec<job::Rect> {
        self.state
            .active_mut()
            .and_then(|tab| tab.edit.as_mut())
            .map(|edit| edit.tiles.take_recomposited())
            .unwrap_or_default()
    }

    /// Where runs stage, held checkpoints wait for a save and opened
    /// projects' planes are cached: `<data dir>/scratch`, or the OS temp
    /// directory when no data directory resolves. Never beside the frames.
    fn scratch_root(&self) -> PathBuf {
        self.scratch_root_for_test.clone().unwrap_or_else(|| {
            focusforge_diagnostics::data_dir_from_env()
                .map(|dir| dir.join("scratch"))
                .unwrap_or_else(|| std::env::temp_dir().join("focusforge-scratch"))
        })
    }

    /// Installs the observer sink. `src/bridge.rs` is the only caller
    /// outside tests; the controller tests install their own sink directly,
    /// since `i_slint_backend_testing::init_no_event_loop()` disables
    /// `slint::invoke_from_event_loop` and the bridge is never exercised by
    /// them.
    pub fn set_sink(&mut self, sink: Arc<dyn Fn(ShellEvent) + Send + Sync>) {
        self.sink = Some(sink);
    }

    fn emit(&self, event: ShellEvent) {
        if let Some(sink) = &self.sink {
            sink(event);
        }
    }

    pub fn mode(&self) -> ShellMode {
        self.state.mode
    }

    /// Returns `false` and emits nothing for [`ShellMode::Editor`] unless
    /// [`Self::editor_enabled`] is `true`. Entering editor mode opens the
    /// active tab's own edit session on first entry -- a failure to
    /// materialize or open its Z-Map also returns `false` and emits nothing,
    /// leaving the mode unchanged.
    pub fn set_mode(&mut self, mode: ShellMode) -> bool {
        if mode == ShellMode::Editor && !(self.editor_enabled() && self.ensure_edit_session()) {
            return false;
        }
        if mode != ShellMode::Editor {
            self.discard_live_stroke();
        }
        self.state.mode = mode;
        self.emit(ShellEvent::ModeChanged(mode));
        true
    }

    /// Opens the active tab's edit session over its whole Z-Map and its own
    /// frames unless the tab already holds one. `false` with no active tab,
    /// or when its Z-Map or its frames fail to materialize or open.
    fn ensure_edit_session(&mut self) -> bool {
        if self.state.active().is_some_and(|tab| tab.edit.is_some()) {
            return true;
        }
        let Some((_, zmap)) = self.materialize_active_tab_planes() else {
            return false;
        };
        let Some(tab) = self.state.active() else {
            return false;
        };
        let frames = tab.frames.clone();
        // `can_edit` guarantees `tab.frames` equals the fused sources, in
        // the same order, by the time editor mode is entered -- so indexing
        // `registration`'s per-frame transforms by position here matches.
        let Some((_, registration)) = tab.fused() else {
            return false;
        };
        let tile_size = self
            .resident_tile_size_for_test
            .unwrap_or(job::RESIDENT_TILE_SIZE);
        let Ok(edit) =
            job::open_edit_session(&zmap, &frames, registration, tile_size, job::host_budget())
        else {
            return false;
        };
        let Some(tab) = self.state.active_mut() else {
            return false;
        };
        tab.edit = Some(edit);
        true
    }

    /// Discards the active tab's own in-progress stroke, if any: restores
    /// its preview and drops it, so leaving the editor or switching tab
    /// mid-drag never leaves a preview outstanding on a tab nobody is
    /// drawing on anymore.
    fn discard_live_stroke(&mut self) {
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let Some(edit) = tab.edit.as_mut() else {
            return;
        };
        if edit.stroke.take().is_some() {
            edit.session.discard_preview();
        }
    }

    /// Whether the active tab can enter editor mode -- pure and I/O-free,
    /// safe to push after any event that might change it (a tab switch, a
    /// run settling, a project opening, a frame-set change).
    pub fn editor_enabled(&self) -> bool {
        self.state.active().is_some_and(ProjectTab::can_edit)
    }

    /// The active tab's own edit session, once editor mode has opened one.
    pub fn edit_session(&self) -> Option<&job::EditSession> {
        Some(&self.state.active()?.edit.as_ref()?.session)
    }

    /// Mutable twin of [`Self::edit_session`] -- the seam an editor op
    /// appends through.
    pub fn edit_session_mut(&mut self) -> Option<&mut job::EditSession> {
        Some(&mut self.state.active_mut()?.edit.as_mut()?.session)
    }

    /// The active tab's edited view for the renderer's overlay -- `None`
    /// before an edit session has opened.
    pub fn edited_view(&self) -> Option<crate::canvas::EditedView<'_>> {
        let edit = self.state.active()?.edit.as_ref()?;
        Some(crate::canvas::EditedView {
            indices: edit.session.indices(),
            map_width: edit.session.tile().width,
            tiles: &edit.tiles,
        })
    }

    /// The bottom bar's overlay controls.
    pub fn overlay_view(&self) -> OverlayView {
        OverlayView {
            on: self.state.overlay.on,
            opacity: self.state.overlay.opacity,
        }
    }

    /// Turns the `Composite` pane's rainbow overlay on or off.
    pub fn set_overlay_on(&mut self, on: bool) {
        self.state.overlay.on = on;
        self.emit(ShellEvent::EditorChanged);
    }

    /// Sets the overlay's opacity, `0.0..=1.0`, clamped.
    pub fn set_overlay_opacity_fraction(&mut self, fraction: f32) {
        self.state.overlay.opacity = fraction.clamp(0.0, 1.0);
        self.emit(ShellEvent::EditorChanged);
    }

    /// The `Composite` pane's overlay draw parameters, matching
    /// `focusforge-cli retouch --overlay --opacity`'s own blend -- `None`
    /// outside editor mode, with the overlay off, or before an edit session
    /// has opened.
    pub fn overlay_draw(&self) -> Option<OverlayDraw> {
        if self.mode() != ShellMode::Editor || !self.state.overlay.on {
            return None;
        }
        let tab = self.state.active()?;
        tab.edit.as_ref()?;
        Some(OverlayDraw::for_opacity(
            tab.frames.len() as u32,
            self.state.overlay.opacity,
        ))
    }

    /// In editor mode, gives a newly active editable tab its own edit
    /// session; drops the window back to stacking, with exactly one
    /// `ModeChanged(Stacking)`, when the active tab no longer satisfies
    /// [`Self::editor_enabled`] or its session cannot open -- a switch away
    /// from a fused tab, that tab's own run starting or being promoted, or a
    /// confirmed frame-set change.
    fn leave_editor_if_unfit(&mut self) {
        if self.state.mode == ShellMode::Editor
            && !(self.editor_enabled() && self.ensure_edit_session())
        {
            self.discard_live_stroke();
            self.state.mode = ShellMode::Stacking;
            self.emit(ShellEvent::ModeChanged(ShellMode::Stacking));
        }
    }

    /// The brush's source index: the active tab's `selected` position, or
    /// `None` with no active tab or a position past `u16::MAX`. Computed on
    /// every call from `selected`; no field holds a copy.
    pub fn brush_source(&self) -> Option<u16> {
        u16::try_from(self.state.active()?.selected).ok()
    }

    pub fn dock_tab(&self) -> DockTab {
        self.state.dock_tab
    }

    pub fn select_dock_tab(&mut self, tab: DockTab) {
        self.state.dock_tab = tab;
        self.emit(ShellEvent::DockTabSelected(tab));
    }

    /// Always empty -- the checkpoint panel's frame reads no storage.
    pub fn checkpoints(&self) -> &[CheckpointRow] {
        &self.state.checkpoints
    }

    /// Always `false` -- the checkpoint panel's `Export` stays disabled.
    pub fn export_enabled(&self) -> bool {
        false
    }

    /// The tab strip's pointer-down, forwarded from `window-drag-requested`.
    pub fn tab_strip_pressed(&self) {
        self.emit(ShellEvent::WindowDragRequested);
    }

    pub fn window_minimize(&self) {
        self.emit(ShellEvent::WindowMinimizeRequested);
    }

    /// The window controls' maximize glyph reads this back
    /// (`AppWindow::window-maximized`, wired in `src/bridge.rs`), so it
    /// looks different maximized than not.
    pub fn maximized(&self) -> bool {
        self.state.maximized
    }

    pub fn window_maximize_toggle(&mut self) {
        self.state.maximized = !self.state.maximized;
        self.emit(ShellEvent::WindowMaximizeToggled(self.state.maximized));
    }

    /// Quits the window: with no tab holding a checkpoint or a parked run,
    /// closes it at once. Otherwise raises [`Self::pending_quit_copy`] and
    /// touches no tab. A no-op while a close question is open.
    pub fn window_close(&mut self) {
        if self.state.pending_close.is_some() {
            return;
        }
        if !self.state.projects.iter().any(ProjectTab::quit_loses_work) {
            self.emit(ShellEvent::WindowCloseRequested);
        } else {
            self.state.pending_quit = true;
            self.emit(ShellEvent::ProjectsChanged);
        }
    }

    /// Never reachable while [`Self::export_enabled`] is always `false`,
    /// but declared so the seam exists to wire without moving anything.
    pub fn checkpoint_export_requested(&self) {
        self.emit(ShellEvent::CheckpointExportRequested);
    }

    /// `"Composite"` / `"Z-Map"` -- the label's geometry is fixed
    /// elsewhere, not its text.
    pub fn pane_label(&self, pane: crate::canvas::PaneSource) -> &'static str {
        match pane {
            crate::canvas::PaneSource::Composite => "Composite",
            crate::canvas::PaneSource::ZMap => "Z-Map",
        }
    }

    /// Always `true` -- both panes are docked together.
    pub fn undock_enabled(&self, _pane: crate::canvas::PaneSource) -> bool {
        true
    }

    /// The active tab's frame count, once a project is open -- the
    /// index-scale uniform (`crate::gpu_canvas`'s render path) is derived
    /// from this. `None` while "+" is selected.
    pub fn frame_count(&self) -> Option<u32> {
        self.state
            .active()
            .map(|tab| entry::tab_frame_count(&tab.project))
    }

    /// Forwards a pane's undock request -- unreachable while
    /// [`Self::undock_enabled`] is always `true` with both panes docked,
    /// but declared so the seam exists.
    pub fn undock_requested(&self, pane: crate::canvas::PaneSource) {
        self.emit(ShellEvent::PaneUndockRequested(pane));
    }

    /// Installs the input table `input_config::load` resolved -- called
    /// once at startup, before any gesture is dispatched.
    pub fn install_input_table(&mut self, table: InputTable, provenance: Provenance) {
        self.state.input.table = table;
        self.state.input.provenance = provenance;
    }

    /// Records `event`'s action as unimplemented (by name, never silently)
    /// when there is no implementation for it; an implemented one is
    /// applied instead ([`Self::apply_action`]). Either way
    /// [`ShellEvent::Input`] is raised -- nothing is both dropped and
    /// unrecorded, and nothing stops being emitted.
    fn emit_action(&mut self, event: ActionEvent) {
        let action = event.action();
        if action.implemented() {
            self.apply_action(&event);
        } else if !self.state.input.unimplemented.contains(&action) {
            self.state.input.unimplemented.push(action);
        }
        self.emit(ShellEvent::Input(event));
    }

    /// The viewport half of dispatch: `ZoomAtCursor`
    /// spends the wheel's accumulated delta into ladder steps about the
    /// cursor; `PanHorizontal` and the drag-driven `Pan` both read straight
    /// into [`crate::viewport::Viewport::pan_by`]. The editor half: the two
    /// brush wheels, undo/redo, the pipette's click and the paint drag.
    fn apply_action(&mut self, event: &ActionEvent) {
        match *event {
            ActionEvent::Fired {
                action: Action::ZoomAtCursor,
                steps,
            } => self.zoom_wheel(steps),
            ActionEvent::Fired {
                action: Action::PanHorizontal,
                steps,
            } => self.pan_viewport(steps, 0.0),
            ActionEvent::Update {
                action: Action::Pan,
                dx,
                dy,
            } => self.pan_viewport(dx, dy),
            ActionEvent::Fired {
                action: Action::StepSourceFrame,
                steps,
            } => self.step_source_frame(steps),
            ActionEvent::Fired {
                action: Action::BrushSize,
                steps,
            } => self.adjust_brush_size(steps),
            ActionEvent::Fired {
                action: Action::BrushSoftness,
                steps,
            } => self.adjust_brush_softness(steps),
            ActionEvent::Fired {
                action: Action::Undo,
                ..
            } => self.undo(),
            ActionEvent::Fired {
                action: Action::Redo,
                ..
            } => self.redo(),
            ActionEvent::Fired {
                action: Action::PickSourceUnderPointer,
                ..
            } => self.pick_source_under_pointer(),
            ActionEvent::Start {
                action: Action::Paint,
                x,
                y,
            } => self.paint_start(x, y),
            ActionEvent::Update {
                action: Action::Paint,
                dx,
                dy,
            } => self.paint_update(dx, dy),
            ActionEvent::End {
                action: Action::Paint,
            } => self.paint_end(),
            _ => {}
        }
    }

    /// Spends `steps` of `action`'s own wheel gesture into whole 60 px
    /// detents, anchored at the last known cursor position. The accumulator
    /// resets on a sign change and whenever `action` differs from the
    /// previous call's, so reversing direction, or switching from one
    /// wheel-bound action to another, never burns a leftover.
    fn wheel_detents(&mut self, action: Action, steps: f32) -> i32 {
        const DETENT: f32 = 60.0;
        if self.state.input.last_wheel_action != Some(action) {
            self.state.input.wheel_accum = 0.0;
            self.state.input.last_wheel_action = Some(action);
        }
        let accum = &mut self.state.input.wheel_accum;
        if *accum != 0.0 && accum.signum() != steps.signum() {
            *accum = 0.0;
        }
        *accum += steps;
        let mut detents = 0;
        while accum.abs() >= DETENT {
            let dir = if *accum > 0.0 { 1 } else { -1 };
            *accum -= DETENT * dir as f32;
            detents += dir;
        }
        detents
    }

    /// One ladder step per accumulated 60 px, about the last known cursor
    /// position.
    fn zoom_wheel(&mut self, steps: f32) {
        let Some((left, _)) = self.state.panes else {
            return;
        };
        let anchor = self.anchor();
        let detents = self.wheel_detents(Action::ZoomAtCursor, steps);
        let dir = detents.signum();
        for _ in 0..detents.unsigned_abs() {
            let Some(tab) = self.state.active_mut() else {
                return;
            };
            let Some(image) = display_image(tab) else {
                return;
            };
            if let Some(viewport) = tab.viewport.as_mut() {
                viewport.zoom_steps(dir, anchor, image, left);
            }
        }
    }

    /// `Ctrl + Wheel` over the canvas, or a plain wheel over the `Images`
    /// list ([`Self::images_list_wheel`]): steps the left pane's selected
    /// frame, clamped to the frame list. In editor mode this is also the
    /// brush source, which reads `tab.selected` directly
    /// ([`AppController::brush_source`]).
    fn step_source_frame(&mut self, steps: f32) {
        // Negated: scrolling toward the user (positive `steps`) steps to an
        // earlier frame, matching how the `Images` list itself scrolls.
        let detents = self.wheel_detents(Action::StepSourceFrame, -steps);
        if detents == 0 {
            return;
        }
        let Some(tab) = self.state.active() else {
            return;
        };
        let len = tab.frames.len();
        if len == 0 {
            return;
        }
        let next = (tab.selected as i32 + detents).clamp(0, len as i32 - 1) as usize;
        self.select_frame(next);
    }

    /// A plain wheel over the `Images` list: the same effect `Ctrl + Wheel`
    /// over the canvas has (`step_source_frame`) -- not gated on `Ctrl`,
    /// `legend` or `mutable`, so it works identically in stacking and editor
    /// mode.
    pub fn images_list_wheel(&mut self, steps: f32) {
        self.step_source_frame(steps);
    }

    /// The active tab's fused image's shorter side -- `Size`'s ceiling.
    /// `1` with no active tab or nothing fused yet, so the log-scale slider
    /// formulas never divide by zero.
    fn brush_size_ceiling(&self) -> u32 {
        self.state
            .active()
            .and_then(fused_image_size)
            .map(|image| image.width.min(image.height))
            .unwrap_or(1)
    }

    /// `Shift + Wheel` in editor mode: one detent multiplies or divides
    /// `Size` by `2^(1/4)`, clamped to `1..=ceiling`.
    fn adjust_brush_size(&mut self, steps: f32) {
        let detents = self.wheel_detents(Action::BrushSize, steps);
        if detents == 0 {
            return;
        }
        let ceiling = self.brush_size_ceiling();
        let factor = 2f32.powf(0.25 * detents as f32);
        self.state.brush.size = (self.state.brush.size * factor).clamp(1.0, ceiling as f32);
        self.emit(ShellEvent::EditorChanged);
    }

    /// `Alt + Wheel` in editor mode: one detent moves `Soft` by 5 points,
    /// clamped to `0..=100`.
    fn adjust_brush_softness(&mut self, steps: f32) {
        let detents = self.wheel_detents(Action::BrushSoftness, steps);
        if detents == 0 {
            return;
        }
        self.state.brush.soft = (self.state.brush.soft + 5.0 * detents as f32).clamp(0.0, 100.0);
        self.emit(ShellEvent::EditorChanged);
    }

    /// The editor toolbar's brush readout.
    pub fn brush_toolbar_view(&self) -> BrushToolbarView {
        let ceiling = self.brush_size_ceiling();
        let size = self.state.brush.size.clamp(1.0, ceiling as f32);
        BrushToolbarView {
            size,
            size_ceiling: ceiling,
            size_fraction: fraction_from_size(size, ceiling),
            soft: self.state.brush.soft,
            soft_fraction: self.state.brush.soft / 100.0,
            picking: self.state.brush.picking,
            can_undo: self.edit_session().is_some_and(job::EditSession::can_undo),
            can_redo: self.edit_session().is_some_and(job::EditSession::can_redo),
        }
    }

    /// The brush cursor overlay's own view: extent and path commands built from the current brush
    /// size/softness scaled to on-screen px by the active tab's own viewport, the pipette's own
    /// preview colour -- the Z-Map index under the pointer while armed or while `Alt` is held, the
    /// selected source otherwise -- and `mirror_dx`, the offset that carries either pane's cursor
    /// onto the other. `None` with no active tab, viewport or pane geometry, or while
    /// [`Self::canvas_blocked`] holds.
    pub fn brush_cursor_view(&self) -> Option<BrushCursorView> {
        if self.canvas_blocked() {
            return None;
        }
        let tab = self.state.active()?;
        let viewport = tab.viewport.as_ref()?;
        let (left, right) = self.state.panes?;
        let (radius, softness) = self.brush_radius_softness();
        let scale = viewport.scale() as f32;
        let paths = brush::cursor_paths(radius * scale, softness);
        let sources = tab.frames.len().max(1) as u32;
        let picking = self.state.brush.picking || self.state.input.alt_held;
        let hovered = picking
            .then(|| self.source_index_at(self.anchor()))
            .flatten()
            .filter(|&source| source != job::NO_SOURCE);
        let source = hovered.unwrap_or_else(|| u16::try_from(tab.selected).unwrap_or(u16::MAX));
        Some(BrushCursorView {
            extent: paths.extent,
            ring_commands: paths.ring,
            core_commands: paths.core,
            picking,
            pick_color: job::source_color(source, sources),
            mirror_dx: right.x - left.x,
        })
    }

    /// The slider's own callback: `fraction` is `0.0..=1.0` along the
    /// logarithmic track.
    pub fn set_brush_size_fraction(&mut self, fraction: f32) {
        let ceiling = self.brush_size_ceiling();
        self.state.brush.size = size_from_fraction(fraction, ceiling);
        self.emit(ShellEvent::EditorChanged);
    }

    /// The slider's own callback: `fraction` is `0.0..=1.0` along the linear
    /// track.
    pub fn set_brush_softness_fraction(&mut self, fraction: f32) {
        self.state.brush.soft = fraction.clamp(0.0, 1.0) * 100.0;
        self.emit(ShellEvent::EditorChanged);
    }

    /// Toggles the pipette's armed state.
    pub fn toggle_pipette(&mut self) {
        self.state.brush.picking = !self.state.brush.picking;
        self.emit(ShellEvent::EditorChanged);
    }

    /// The brush's radius and softness in the storage units core's retouch
    /// ops take -- the one conversion point from the toolbar's display
    /// conventions.
    fn brush_radius_softness(&self) -> (f32, f32) {
        let ceiling = self.brush_size_ceiling();
        let size = self.state.brush.size.clamp(1.0, ceiling as f32);
        (size / 2.0, self.state.brush.soft / 100.0)
    }

    /// `Ctrl + Z`: steps the active tab's edit session back one op and
    /// recomposites the region it returns. Ignored while a stroke is live.
    pub fn undo(&mut self) {
        self.undo_or_redo(job::EditSession::undo);
    }

    /// `Ctrl + Shift + Z`: re-applies the most recently undone op and
    /// recomposites the region it returns. Ignored while a stroke is live.
    pub fn redo(&mut self) {
        self.undo_or_redo(job::EditSession::redo);
    }

    /// Shared by [`Self::undo`]/[`Self::redo`]: `step` is whichever of the
    /// two returns `Some` (`None` inside meaning nothing visibly changed) or
    /// `None` when there was nothing to step. A no-op while a stroke is
    /// live.
    fn undo_or_redo(&mut self, step: fn(&mut job::EditSession) -> Option<Option<job::Rect>>) {
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let Some(edit) = tab.edit.as_mut() else {
            return;
        };
        if edit.stroke.is_some() {
            return;
        }
        let Some(region) = step(&mut edit.session) else {
            return;
        };
        if let Some(region) = region
            && let Err(err) = edit.recomposite(region)
        {
            tracing::error!(error = %err, "undo/redo recomposite failed");
        }
        self.emit(ShellEvent::EditorChanged);
    }

    /// Reads the Z-Map index under `local` (pane-local coordinates) --
    /// shared by [`Self::pick_source_at`] (which commits it as the brush
    /// source) and [`Self::brush_cursor_view`] (which only previews its
    /// colour). `None` off the image, off the active tab's tile, or with
    /// nothing to read; callers decide what `NO_SOURCE` means to them.
    fn source_index_at(&self, local: (f32, f32)) -> Option<u16> {
        let tab = self.state.active()?;
        let viewport = tab.viewport.as_ref()?;
        let (sx, sy) = viewport.to_source(local);
        if sx < 0.0 || sy < 0.0 {
            return None;
        }
        let (x, y) = (sx.floor() as u32, sy.floor() as u32);
        let session = tab.edit.as_ref().map(|edit| &edit.session)?;
        let tile = session.tile();
        if x >= tile.width || y >= tile.height {
            return None;
        }
        let idx = (y * tile.width + x) as usize;
        session.indices().get(idx).copied()
    }

    /// `Alt + click`, or the armed pipette's `Start`: reads the Z-Map index
    /// under `local` (pane-local coordinates) and makes it the brush source.
    /// `NO_SOURCE` and a point off the image leave the source unchanged.
    fn pick_source_at(&mut self, local: (f32, f32)) {
        let Some(source) = self.source_index_at(local) else {
            return;
        };
        if source == job::NO_SOURCE {
            return;
        }
        self.select_frame(source as usize);
    }

    /// `Alt + click`: picks without arming the pipette.
    fn pick_source_under_pointer(&mut self) {
        let local = self.anchor();
        self.pick_source_at(local);
    }

    /// `Paint`'s `Start`. With the pipette armed, this is a pick that also
    /// disarms it, and the rest of the drag is ignored (no
    /// [`job::EditState::stroke`] is opened, so [`Self::paint_update`] has
    /// nothing to push into). Otherwise, inside
    /// the right pane only, opens a stroke at the pointer's source position,
    /// freezing the brush's source, radius and softness for the stroke.
    fn paint_start(&mut self, x: f32, y: f32) {
        if self.state.brush.picking {
            let local = self.anchor();
            self.pick_source_at(local);
            self.state.brush.picking = false;
            self.emit(ShellEvent::EditorChanged);
            return;
        }
        let Some((_, right)) = self.state.panes else {
            return;
        };
        if !pane_contains(right, x, y) {
            return;
        }
        let Some(source) = self.brush_source() else {
            return;
        };
        let (radius, softness) = self.brush_radius_softness();
        let local = (x - right.x, y - right.y);
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let Some(viewport) = tab.viewport.as_ref() else {
            return;
        };
        let (sx, sy) = viewport.to_source(local);
        let Some(edit) = tab.edit.as_mut() else {
            return;
        };
        edit.stroke = Some(brush::LiveStroke::new(
            source,
            radius,
            softness,
            sx as f32 - 0.5,
            sy as f32 - 0.5,
        ));
    }

    /// `Paint`'s `Update`: integrates a screen-space delta into the live
    /// stroke's image-space trace. A no-op with no live stroke (a pipette
    /// pick's drag, or a stroke that started outside the right pane).
    fn paint_update(&mut self, dx: f32, dy: f32) {
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let Some(viewport) = tab.viewport.as_ref() else {
            return;
        };
        let scale = viewport.scale() as f32;
        let Some(stroke) = tab.edit.as_mut().and_then(|edit| edit.stroke.as_mut()) else {
            return;
        };
        stroke.push(dx / scale, dy / scale);
    }

    /// Called by every repaint before it reads anything else: takes the
    /// active tab's live stroke's newest segment, if any, previews it and
    /// recomposites exactly that segment's footprint into the resident
    /// tiles -- one tick per repaint, never per motion event. A no-op with
    /// no live stroke or no motion since the last flush.
    pub fn flush_live_stroke(&mut self) {
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let Some(edit) = tab.edit.as_mut() else {
            return;
        };
        let Some(stroke) = edit.stroke.as_mut() else {
            return;
        };
        let Some(segment) = stroke.take_segment() else {
            return;
        };
        let op = job::Op::BrushStroke {
            source_id: stroke.source(),
            path: segment,
            radius: stroke.radius(),
            softness: stroke.softness(),
        };
        let region = edit
            .session
            .preview(&op)
            .expect("a live stroke's own points are pre-clamped to what validate accepts");
        let Some(region) = region else {
            return;
        };
        if let Err(err) = edit.recomposite(region) {
            tracing::error!(error = %err, "live-stroke preview recomposite failed");
            return;
        }
        edit.stroke
            .as_mut()
            .expect("the stroke that produced this tick is still live")
            .tick_rects
            .push(region);
    }

    /// `Paint`'s `End`: flushes remaining motion, discards the preview, then
    /// appends the stroke's resampled path unless its footprint is empty or
    /// off-image. Recomposites the tick rects (dilated), or the appended
    /// region itself when no tick fired (a press-release with no motion).
    fn paint_end(&mut self) {
        self.flush_live_stroke();
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let Some(edit) = tab.edit.as_mut() else {
            return;
        };
        edit.session.discard_preview();
        let Some(stroke) = edit.stroke.take() else {
            return;
        };
        let op = job::Op::BrushStroke {
            source_id: stroke.source(),
            path: stroke.finish(),
            radius: stroke.radius(),
            softness: stroke.softness(),
        };
        let has_footprint = job::footprint(&op, edit.session.info())
            .ok()
            .flatten()
            .is_some();
        if !has_footprint {
            return;
        }
        let radius = stroke.radius();
        let tick_rects = stroke.tick_rects;
        let region = edit
            .session
            .append(op)
            .expect("a live stroke's own points are pre-clamped to what validate accepts");
        let Some(region) = region else { return };
        let map = edit.session.tile();
        if tick_rects.is_empty() {
            if let Err(err) = edit.recomposite(region) {
                tracing::error!(error = %err, "brush-stroke commit recomposite failed");
            }
        } else {
            let margin = (radius / 8.0).ceil() as u32 + 2;
            for rect in tick_rects {
                let dilated = dilate_rect(rect, margin, map.width, map.height);
                if let Err(err) = edit.recomposite(dilated) {
                    tracing::error!(error = %err, "brush-stroke commit recomposite failed");
                }
            }
        }
        self.emit(ShellEvent::EditorChanged);
    }

    /// A screen-pixel pan on whichever pane the cursor is over -- both
    /// panes share the one viewport, so only the size (identical for both)
    /// matters here.
    fn pan_viewport(&mut self, dx: f32, dy: f32) {
        let Some((left, _)) = self.state.panes else {
            return;
        };
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let Some(image) = display_image(tab) else {
            return;
        };
        let Some(viewport) = tab.viewport.as_mut() else {
            return;
        };
        viewport.pan_by(dx, dy, image, left);
    }

    /// The last known cursor position, mapped into pane-local coordinates:
    /// subtract the rect of whichever pane contains it, and when it is over
    /// the gutter, the divider or outside both, clamp it into the nearer
    /// pane's rect. Both panes carry the same viewport, so which pane the
    /// cursor is in does not change the result, only where inside it.
    fn anchor(&self) -> (f32, f32) {
        let (x, y) = self.state.input.last_pointer;
        let Some((left, right)) = self.state.panes else {
            return (x, y);
        };
        let pane = if pane_contains(left, x, y) {
            left
        } else if pane_contains(right, x, y) {
            right
        } else {
            let mid = (left.x + left.width + right.x) / 2.0;
            if x < mid { left } else { right }
        };
        (
            x.clamp(pane.x, pane.x + pane.width) - pane.x,
            y.clamp(pane.y, pane.y + pane.height) - pane.y,
        )
    }

    /// Installs both panes' current geometry, pushed by `ui/shell/canvas.slint`'s
    /// five `out` properties (`src/bridge.rs`'s `canvas-resized` handler and
    /// `src/main.rs`'s startup push). Fits the viewport on the first call
    /// after a project opens (`open_project` cannot, having no pane rect of
    /// its own); every later call re-`resize`s it, so fit stays sticky.
    pub fn set_pane_geometry(&mut self, left: PaneRect, right: PaneRect) {
        self.state.panes = Some((left, right));
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let Some(image) = display_image(tab) else {
            return;
        };
        match tab.viewport.as_mut() {
            Some(viewport) => viewport.resize(image, left),
            None => tab.viewport = Some(crate::viewport::fit(image, left)),
        }
    }

    /// Fits the active tab's viewport when it has none yet and a pane
    /// geometry is known. A tab that becomes active without the canvas
    /// resizing -- a load settling, a tab switch -- gets no
    /// [`Self::set_pane_geometry`] call of its own to fit it.
    pub fn fit_if_unfitted(&mut self) {
        let Some((left, _)) = self.state.panes else {
            return;
        };
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        if let (Some(image), None) = (display_image(tab), &tab.viewport) {
            tab.viewport = Some(crate::viewport::fit(image, left));
        }
    }

    /// One ladder step in, about the pane's centre -- a button has no
    /// cursor.
    pub fn zoom_in(&mut self) {
        self.zoom_button(1);
    }

    /// One ladder step out, about the pane's centre.
    pub fn zoom_out(&mut self) {
        self.zoom_button(-1);
    }

    fn zoom_button(&mut self, dir: i32) {
        let Some((left, _)) = self.state.panes else {
            return;
        };
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let Some(image) = display_image(tab) else {
            return;
        };
        let Some(viewport) = tab.viewport.as_mut() else {
            return;
        };
        let centre = (left.width / 2.0, left.height / 2.0);
        viewport.zoom_steps(dir, centre, image, left);
    }

    /// `Fit`: shows the whole image, centred, in whichever pane geometry is
    /// current.
    pub fn fit(&mut self) {
        let Some((left, _)) = self.state.panes else {
            return;
        };
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let Some(image) = display_image(tab) else {
            return;
        };
        tab.viewport = Some(crate::viewport::fit(image, left));
    }

    /// The zoom cluster's readout string -- empty while no tab is active or
    /// no viewport has fitted yet.
    pub fn zoom_readout(&self) -> String {
        self.state
            .active()
            .and_then(|tab| tab.viewport.as_ref())
            .map(Viewport::readout)
            .unwrap_or_default()
    }

    /// Whether the active tab's viewport is at the ladder's minimum --
    /// fit-to-window with one step below it.
    pub fn at_min_zoom(&self) -> bool {
        let Some(tab) = self.state.active() else {
            return false;
        };
        let (Some(image), Some((left, _)), Some(viewport)) =
            (display_image(tab), self.state.panes, tab.viewport.as_ref())
        else {
            return false;
        };
        viewport.at_min(image, left)
    }

    /// Whether the active tab's viewport is at the ladder's maximum --
    /// 500 %, when fit does not already exceed it.
    pub fn at_max_zoom(&self) -> bool {
        let Some(tab) = self.state.active() else {
            return false;
        };
        let (Some(image), Some((left, _)), Some(viewport)) =
            (display_image(tab), self.state.panes, tab.viewport.as_ref())
        else {
            return false;
        };
        viewport.at_max(image, left)
    }

    /// The active tab's viewport's visible source rect and its pane-local
    /// projection over the selected frame, for
    /// `crate::gpu_canvas::CanvasRenderer::redraw_frame` and the left
    /// `FFPane`'s placement properties. `None` while no tab is active, no
    /// frame size is known, or before the first pane geometry arrives.
    pub fn frame_draw(&self) -> Option<PaneDraw> {
        let tab = self.state.active()?;
        let image = display_image(tab)?;
        let (left, _) = self.state.panes?;
        tab.viewport.as_ref()?.draw(image, left)
    }

    /// The active tab's viewport's visible source rect and its pane-local
    /// projection over the newest composite/Z-Map's own extent -- the
    /// newest held checkpoint's, else the opened project's, else (before any
    /// fuse) the raw frame's own, the same `display_image` `frame_draw`
    /// uses -- for `crate::gpu_canvas::CanvasRenderer::redraw_planes` and the
    /// right `FFPane`'s placement properties. `None` while no tab is active
    /// or before the first pane geometry arrives.
    pub fn plane_draw(&self) -> Option<PaneDraw> {
        let tab = self.state.active()?;
        let image = display_image(tab)?;
        let (_, right) = self.state.panes?;
        tab.viewport.as_ref()?.draw(image, right)
    }

    /// The one wheel entry point. Axis normalisation first: an unmodified
    /// `delta_x`-dominant event resolves to the same row as
    /// `shift + delta_y`, because platforms commonly deliver a shifted
    /// wheel as a horizontal delta with the modifier consumed.
    /// `x`/`y` are the wheel's own cursor position -- `TouchArea` writes
    /// `mouse-x`/`mouse-y` before `scroll-event` is invoked.
    pub fn wheel(&mut self, delta_x: f32, delta_y: f32, x: f32, y: f32, mods: Modifiers) {
        // Every wheel-triggered action -- zoom, pan, anything else a table
        // row binds to `Trigger::Wheel` -- stays off while the canvas is
        // blocked.
        if self.canvas_blocked() {
            return;
        }
        self.state.input.last_pointer = (x, y);
        let (trigger, steps) = if delta_x.abs() > delta_y.abs() && !mods.shift {
            (Trigger::Wheel(mods.with_shift()), delta_x)
        } else {
            (Trigger::Wheel(mods), delta_y)
        };
        if let Some(action) = self.state.input.table.action(&trigger, self.state.mode) {
            self.emit_action(ActionEvent::Fired { action, steps });
        }
    }

    /// The one pointer-down/up entry point (also the Cancel path -- a
    /// cancelled grab is forwarded here as `PointerKind::Up` so a live drag
    /// never gets stuck on). `Down` starts a drag on a
    /// `Trigger::Drag` hit or fires once on a `Trigger::Click` hit; `Up`
    /// ends any live drag. While the canvas is blocked, `Down` starts no
    /// drag and fires no click, but `Up` still ends whatever drag was
    /// already live, so nothing sticks on once the modal opened mid-stroke.
    pub fn pointer(&mut self, kind: PointerKind, button: Button, x: f32, y: f32, mods: Modifiers) {
        self.state.input.last_pointer = (x, y);
        if self.canvas_blocked() && kind != PointerKind::Up {
            return;
        }
        match kind {
            PointerKind::Down => {
                let drag_trigger = Trigger::Drag { button, mods };
                if let Some(action) = self
                    .state
                    .input
                    .table
                    .action(&drag_trigger, self.state.mode)
                {
                    self.state.input.drag = Some(DragState {
                        action,
                        last: (x, y),
                    });
                    self.emit_action(ActionEvent::Start { action, x, y });
                    return;
                }
                let click_trigger = Trigger::Click { button, mods };
                if let Some(action) = self
                    .state
                    .input
                    .table
                    .action(&click_trigger, self.state.mode)
                {
                    self.emit_action(ActionEvent::Fired { action, steps: 0.0 });
                }
            }
            PointerKind::Up => {
                if let Some(drag) = self.state.input.drag.take() {
                    self.emit_action(ActionEvent::End {
                        action: drag.action,
                    });
                }
            }
            // Slint's `pointer-event` fires with `PointerEventKind::Move` on
            // every mouse movement over the `TouchArea`, *before* `moved` --
            // including the first movement of a drag. It carries no motion
            // of its own; `pointer_moved` is the real motion source, so a
            // live drag must not end here.
            PointerKind::Move => self.state.input.alt_held = mods.alt,
        }
    }

    /// Meaningful only while a drag is live. Deltas are differences
    /// between successive positions -- `PointerEvent` carries no movement
    /// delta of its own (slint-ui/slint#13189). While the canvas is
    /// blocked, a live drag commits no further stamp -- it stays live, so
    /// the next `pointer` `Up` still ends it, just with nothing added since
    /// the modal opened.
    pub fn pointer_moved(&mut self, x: f32, y: f32) {
        if self.canvas_blocked() {
            return;
        }
        let Some(drag) = &mut self.state.input.drag else {
            return;
        };
        let (last_x, last_y) = drag.last;
        let action = drag.action;
        drag.last = (x, y);
        self.emit_action(ActionEvent::Update {
            action,
            dx: x - last_x,
            dy: y - last_y,
        });
    }

    /// The one entry point for both key callbacks. `" "` sets/clears
    /// `space_held` (idempotent, absorbing a held key's auto-repeat) and
    /// resolves no row of its own; any other press looks up
    /// `Trigger::Key { key, mods }` -- a release resolves nothing. `text` is
    /// lowercased before the lookup: `Trigger::Key`'s `key` is always the
    /// unshifted character, and Slint reports `"Z"` for `Ctrl+Shift+Z`.
    pub fn key(&mut self, pressed: bool, text: &str, mods: Modifiers) {
        if text == " " {
            self.set_space_held(pressed);
            return;
        }
        if !pressed || self.canvas_blocked() {
            return;
        }
        let lowered = text.to_lowercase();
        let Some(key) = lowered.chars().next() else {
            return;
        };
        let trigger = Trigger::Key { key, mods };
        if let Some(action) = self.state.input.table.action(&trigger, self.state.mode) {
            self.emit_action(ActionEvent::Fired { action, steps: 0.0 });
        }
    }

    pub fn set_space_held(&mut self, held: bool) {
        self.state.input.space_held = held;
    }

    pub fn space_held(&self) -> bool {
        self.state.input.space_held
    }

    /// Called on focus loss so a Space or Alt held across an alt-tab does
    /// not latch (slint-ui/slint#6263).
    pub fn clear_input_modifiers(&mut self) {
        self.state.input.space_held = false;
        self.state.input.alt_held = false;
    }

    pub fn input_rows(&self) -> Vec<InputRowView> {
        self.state.input.table.display_rows()
    }

    /// The settings modal's one section-level provenance line.
    pub fn input_provenance_line(&self) -> String {
        match &self.state.input.provenance {
            Provenance::Defaults => {
                "Loaded from the shipped defaults (0 rows overridden)".to_string()
            }
            Provenance::File { path, overridden } => {
                let noun = if *overridden == 1 { "row" } else { "rows" };
                format!(
                    "Loaded from {} ({overridden} {noun} overridden)",
                    path.display()
                )
            }
        }
    }

    pub fn unimplemented_actions(&self) -> &[Action] {
        &self.state.input.unimplemented
    }

    pub fn settings_open(&self) -> bool {
        self.state.settings_open
    }

    pub fn open_settings(&mut self) -> bool {
        self.state.settings_open = true;
        true
    }

    pub fn close_settings(&mut self) -> bool {
        self.state.settings_open = false;
        false
    }

    /// Whether one of the six dialogs that block the chrome accelerators is
    /// open: Settings, the close/quit guard, the frame-set warning, the
    /// Keep/Discard divergence dialog, the grouping question or the refusal
    /// dialog.
    pub fn modal_open(&self) -> bool {
        self.state.settings_open
            || self.state.pending_close.is_some()
            || self.state.pending_quit
            || self.state.pending_frame_change.is_some()
            || self.pending_divergence().is_some()
            || self.state.pending.is_some()
            || self.state.refusal_open
    }

    /// Whether the canvas and the dock beneath it are blocked: a dialog from
    /// [`Self::modal_open`] is open, or the Projects overlay is.
    pub fn canvas_blocked(&self) -> bool {
        self.modal_open() || self.state.overlay_open
    }

    // -- The entry path: the picker, the drop route and `--open` all call
    // `open_paths`, so the two entrances and the flag cannot diverge. --

    /// The one entry point for the picker, the drop handler and `--open`.
    /// Classifies `paths` synchronously, then runs the directory walk,
    /// header reads or project unzip on a spawned worker, reporting
    /// [`ShellEvent::LoadStarted`]/[`ShellEvent::LoadProgress`]/[`ShellEvent::LoadReady`]
    /// through the sink; [`Self::settle_open`] applies the worker's outcome.
    ///
    /// Returns the worker's join handle; `None` when nothing started -- a
    /// load already in flight, or `paths` failed to classify.
    pub fn open_paths(&mut self, paths: &[PathBuf]) -> Option<std::thread::JoinHandle<()>> {
        if self.state.load.in_flight {
            return None;
        }
        let target = match entry::classify(paths) {
            Ok(target) => target,
            Err(err) => {
                self.refuse(err);
                return None;
            }
        };
        self.state.load.in_flight = true;
        self.emit(ShellEvent::LoadStarted);

        let sink = self.sink.clone();
        let outcome_slot = Arc::clone(&self.state.load.outcome);
        Some(std::thread::spawn(move || {
            let mut last_tick: Option<Instant> = None;
            let mut outcome = entry::open(target, |frames_read, frame_total| {
                let done = frame_total > 0 && frames_read == frame_total;
                let due = last_tick.is_none_or(|tick| tick.elapsed() >= job::POLL);
                if !(done || due) {
                    return;
                }
                last_tick = Some(Instant::now());
                if let Some(sink) = &sink {
                    sink(ShellEvent::LoadProgress {
                        frames_read: frames_read as u32,
                        frame_total: frame_total as u32,
                    });
                }
            });
            // Decoded here, alongside the header walk, so `settle_open`
            // never reads a frame's pixels on the event-loop thread --
            // opening the reference stack's 44.8 MP first frame measured
            // ~5s synchronously.
            let thumbnails = match &mut outcome {
                OpenOutcome::Opened(opened) => decode_thumbnails(&mut opened.projects),
                OpenOutcome::Question(..) | OpenOutcome::Refused(_) => Vec::new(),
            };
            *outcome_slot
                .lock()
                .expect("no thread panicked while holding the load outcome") = Some(LoadResult {
                outcome,
                thumbnails,
            });
            if let Some(sink) = &sink {
                sink(ShellEvent::LoadReady);
            }
        }))
    }

    /// Whether a background load started by [`Self::open_paths`] is still
    /// running -- what gates the loading indicator and a second
    /// [`Self::open_paths`] call while one is in flight.
    pub fn load_in_flight(&self) -> bool {
        self.state.load.in_flight
    }

    /// Picks up the parked outcome of a settled background load and resolves
    /// it exactly as a synchronous open does -- `accept`/`refuse` raise
    /// the same [`ShellEvent::ProjectsChanged`] / [`ShellEvent::QuestionRaised`]
    /// / [`ShellEvent::EntryRefused`]. Clears the in-flight flag. `None` when
    /// nothing is parked (called with no load having settled).
    pub fn settle_open(&mut self) -> Option<OpenOutcome> {
        let result = self
            .state
            .load
            .outcome
            .lock()
            .expect("no thread panicked while holding the load outcome")
            .take()?;
        self.state.load.in_flight = false;
        Some(self.resolve_open(result.outcome, result.thumbnails))
    }

    /// Answers the one pending grouping question, decoding the answer's own
    /// thumbnails on a spawned worker -- the same shape [`Self::open_paths`]
    /// uses, reported and settled through the same load slot. A no-op
    /// refusal, raised inline, when nothing is pending -- the UI only ever
    /// calls this while [`Self::pending_question`] is `Some`. Returns the
    /// worker's join handle; `None` when nothing started.
    pub fn answer_question(&mut self, answer: OpenAs) -> Option<std::thread::JoinHandle<()>> {
        if self.state.load.in_flight {
            return None;
        }
        let Some((_, proposal)) = self.state.pending.take() else {
            self.refuse(EntryError::Unreadable {
                path: PathBuf::new(),
            });
            return None;
        };
        self.state.load.in_flight = true;
        self.emit(ShellEvent::LoadStarted);

        let sink = self.sink.clone();
        let outcome_slot = Arc::clone(&self.state.load.outcome);
        Some(std::thread::spawn(move || {
            let mut opened = entry::answer_question(&proposal, answer);
            let thumbnails = decode_thumbnails(&mut opened.projects);
            *outcome_slot
                .lock()
                .expect("no thread panicked while holding the load outcome") = Some(LoadResult {
                outcome: OpenOutcome::Opened(opened),
                thumbnails,
            });
            if let Some(sink) = &sink {
                sink(ShellEvent::LoadReady);
            }
        }))
    }

    /// Withdraws the pending question with no project opened. Clears
    /// `pending`, which is what makes "asked once" a property of the state
    /// machine rather than of the dialog.
    pub fn cancel_question(&mut self) {
        self.state.pending = None;
    }

    /// The pending grouping question, if one is raised.
    pub fn pending_question(&self) -> Option<&GroupingQuestion> {
        self.state.pending.as_ref().map(|(question, _)| question)
    }

    /// The skipped-subfolder notice from the most recent open, if any.
    pub fn notice(&self) -> Option<&str> {
        self.state.notice.as_deref()
    }

    /// Clears the skipped-subfolder notice. Manual dismiss only -- unlike
    /// the run-failure toast, this one names something already finished,
    /// not a live status to time out on its own, so it waits for its own
    /// `x`. Clearing `self.state.notice` here, not only the Slint-side
    /// property, is what stops a later, unrelated push from resurrecting
    /// the old text.
    pub fn dismiss_notice(&mut self) {
        self.state.notice = None;
        self.emit(ShellEvent::ProjectsChanged);
    }

    /// The most recent refusal's already-rendered message, if any.
    pub fn last_refusal(&self) -> Option<&str> {
        self.state.last_refusal.as_deref()
    }

    /// Closes the refusal dialog -- its own "Pick another folder…" button.
    pub fn dismiss_refusal(&mut self) {
        self.state.refusal_open = false;
    }

    /// `thumbnails` are the pictures the load's own worker already decoded
    /// for `outcome`'s own projects -- both [`Self::open_paths`] and
    /// [`Self::answer_question`] decode on their worker before parking the
    /// outcome here, so [`ProjectTab::new`] never decodes one of its own.
    fn resolve_open(
        &mut self,
        outcome: OpenOutcome,
        thumbnails: Vec<Option<Arc<overlay::Thumbnail>>>,
    ) -> OpenOutcome {
        match outcome {
            OpenOutcome::Opened(opened) => {
                let skipped = opened.skipped.clone();
                self.accept(opened, thumbnails);
                OpenOutcome::Opened(Opened {
                    projects: Vec::new(),
                    skipped,
                })
            }
            OpenOutcome::Question(question, proposal) => {
                self.state.pending = Some((question.clone(), proposal.clone()));
                self.emit(ShellEvent::QuestionRaised(question.clone()));
                OpenOutcome::Question(question, proposal)
            }
            OpenOutcome::Refused(err) => self.refuse(err),
        }
    }

    /// Places every opened project in a tab, selects the first of them, and
    /// records the skipped-subfolder notice. New tabs always go at the end,
    /// whether "+" or a project tab was selected.
    /// Every skipped subfolder becomes a `Not a project` card; an opened
    /// project whose sources do not all resolve gets a `Sources missing`
    /// failure on its own tab; a path that opens successfully clears any
    /// `Not a project` card already held for it.
    fn accept(&mut self, opened: Opened, thumbnails: Vec<Option<Arc<overlay::Thumbnail>>>) {
        self.state.notice = notice_line(&opened.skipped);
        self.state.last_refusal = None;
        for skipped in &opened.skipped {
            self.record_failure(not_a_project_card(skipped.clone()));
        }
        if !opened.projects.is_empty() {
            self.set_active_tab(Some(self.state.projects.len()));
            self.close_projects_overlay();
            let opened_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let mut thumbnails = thumbnails.into_iter();
            for project in opened.projects {
                self.clear_failure(&project_identity_path(&project));
                self.record_recent(
                    entry::tab_title(&project),
                    recent_folder(&project),
                    Some(project_identity_path(&project)),
                    opened_at,
                );
                let sources_missing = if let GuiProject::Opened(p) = &project {
                    p.unresolved_sources().next().is_some()
                } else {
                    false
                };
                let source = match thumbnails.next() {
                    Some(decoded) => ThumbnailSource::Decoded(decoded),
                    None => ThumbnailSource::NotAttempted,
                };
                self.next_tab_id += 1;
                let id = self.next_tab_id;
                let mut tab = ProjectTab::new(project, id, source);
                if sources_missing {
                    tab.failure = Some(TabFailure {
                        kind: FailureKind::SourcesMissing,
                        stage: "NOTHING TO SHOW".to_string(),
                        reason: "The project points at frames that are not where it left them."
                            .to_string(),
                    });
                }
                self.state.projects.push(tab);
            }
        }
        self.settle_tab_scroll();
        self.state.settings_scroll_y = 0.0;
        self.settle_settings_scroll();
        // `ProjectsChanged` alone: its own push already rebuilds the panel
        // for the newly active tab (`push_projects`, `src/bridge.rs`) --
        // a second `SettingsChanged` here would schedule a second, redundant
        // rebuild one event-loop turn later, splitting one tab switch into
        // two visible updates.
        self.emit(ShellEvent::ProjectsChanged);
    }

    fn refuse(&mut self, err: EntryError) -> OpenOutcome {
        let message = err.to_string();
        match &err {
            EntryError::Unreadable { path } | EntryError::NoReadableFrames { path } => {
                self.record_failure(not_a_project_card(path.clone()));
            }
            _ => {}
        }
        self.refuse_message(message);
        OpenOutcome::Refused(err)
    }

    /// Refuses with an already-rendered message and no [`EntryError`] behind
    /// it -- what a re-fuse's own frame-reachability check uses, since
    /// nothing there is an entry-path failure.
    pub fn refuse_message(&mut self, message: String) {
        self.state.last_refusal = Some(message.clone());
        self.state.refusal_open = true;
        self.emit(ShellEvent::EntryRefused(message));
    }

    // -- Tabs as controller state. --

    /// Every open tab, in order -- the plain twin of
    /// `ui/shell/tab_strip.slint`'s `ProjectTabView`.
    pub fn project_tabs(&self) -> Vec<ProjectTabRow> {
        self.state
            .projects
            .iter()
            .enumerate()
            .map(|(i, tab)| ProjectTabRow {
                title: tab.title.clone(),
                unsaved: tab.unsaved(),
                selected: self.state.active == Some(i),
                run_mark_icon: match &tab.run {
                    job::RunState::Queued { .. } => "ellipsis".to_string(),
                    state if job::is_live(state) => "play".to_string(),
                    _ => String::new(),
                },
                would_queue: self.would_queue(i),
            })
            .collect()
    }

    /// The selected tab's index, `None` while "+" is selected.
    pub fn active_project(&self) -> Option<usize> {
        self.state.active
    }

    /// Whether "+" is the selected strip element -- no project active, the
    /// empty state shown. True with zero tabs.
    pub fn plus_selected(&self) -> bool {
        self.state.active.is_none()
    }

    /// Sets which tab is active, if any -- clearing a frame-set-change
    /// warning still queued for whichever tab was active before, on an
    /// actual switch. The pending edit belongs to that tab's own frame
    /// list, not to whatever becomes active next.
    fn set_active_tab(&mut self, active: Option<usize>) {
        if self.state.active != active {
            self.discard_live_stroke();
            self.state.pending_frame_change = None;
        }
        self.state.active = active;
        self.leave_editor_if_unfit();
    }

    /// "+": selects the pseudo-tab, leaving every project tab open and the
    /// strip's scroll where it is. Also closes the projects overlay, the
    /// same as a gallery card click.
    pub fn select_plus(&mut self) {
        if self.state.active.is_some() {
            self.set_active_tab(None);
            self.settle_settings_scroll();
            self.emit(ShellEvent::ProjectsChanged);
        }
        self.close_projects_overlay();
    }

    /// Also closes the projects overlay, the same as a gallery card click:
    /// the tab strip and the gallery both switch to the same project one
    /// way or another.
    pub fn select_project(&mut self, index: usize) {
        if index < self.state.projects.len() {
            self.set_active_tab(Some(index));
            self.settle_tab_scroll();
            self.settle_settings_scroll();
            self.emit(ShellEvent::ProjectsChanged);
        }
        self.close_projects_overlay();
    }

    /// How long after a close further close requests are ignored: 300 ms. A
    /// double-click on a tab's close glyph otherwise closes two tabs -- the
    /// first click closes one, the next tab slides under the pointer and the
    /// second click closes that one too.
    pub const CLOSE_DEBOUNCE: Duration = Duration::from_millis(300);

    /// [`Self::close_project_at`] at the current time.
    pub fn close_project(&mut self, index: usize) {
        self.close_project_at(index, Instant::now());
    }

    /// Closes tab `index` as of `now` without asking, unless a close was
    /// acted on less than [`Self::CLOSE_DEBOUNCE`] before. `now` is a
    /// parameter so tests step the clock without sleeping.
    pub fn close_project_at(&mut self, index: usize, now: Instant) {
        if index < self.state.projects.len() && !self.close_debounced(now) {
            self.remove_tab(index, now);
        }
    }

    /// [`Self::request_close_at`] at the current time -- what every close
    /// path of the window calls.
    pub fn request_close(&mut self, index: usize) {
        self.request_close_at(index, Instant::now());
    }

    /// Asks to close tab `index` as of `now`: a tab holding checkpoints no
    /// `.ffp` contains raises [`Self::pending_close`] and stays open; any
    /// other tab closes at once. A no-op out of range, inside
    /// [`Self::CLOSE_DEBOUNCE`], or while a quit question is open.
    pub fn request_close_at(&mut self, index: usize, now: Instant) {
        if self.state.pending_quit {
            return;
        }
        let Some(tab) = self.state.projects.get(index) else {
            return;
        };
        if self.close_debounced(now) {
            return;
        }
        if tab.held.is_empty() {
            self.remove_tab(index, now);
        } else {
            self.state.pending_close = Some(tab.id);
            self.emit(ShellEvent::ProjectsChanged);
        }
    }

    fn close_debounced(&self, now: Instant) -> bool {
        self.state
            .last_close
            .is_some_and(|last| now.saturating_duration_since(last) < Self::CLOSE_DEBOUNCE)
    }

    /// Closes tab `index`, re-selecting its neighbour -- "+" when the last
    /// tab closes, and "+" stays selected if it already was.
    fn remove_tab(&mut self, index: usize, now: Instant) {
        self.state.last_close = Some(now);
        let closed = self.state.projects.remove(index);
        if self.state.pending_close == Some(closed.id) {
            self.state.pending_close = None;
        }
        if let GuiProject::Opened(project) = &closed.project {
            let _ =
                std::fs::remove_dir_all(job::plane_cache_dir(&self.scratch_root(), project.path()));
        }
        remove_held(&closed.held);
        if let Some(parked) = &closed.parked {
            remove_held(std::slice::from_ref(&parked.held));
        }
        // A closed tab leaves the queue by its own id -- no renumbering by
        // index is needed for the tabs behind it.
        self.job.queue.retain(|&id| id != closed.id);
        self.renumber_queue();
        // Closing another tab never moves the selection: only the indices
        // after the removed one shift down by one -- not the tab switch
        // `set_active_tab` guards against, so a still-active tab's own
        // pending warning survives the renumbering.
        let closed_was_active = self.state.active == Some(index);
        self.state.active = match self.state.active {
            Some(active) if active == index && !self.state.projects.is_empty() => {
                Some(index.min(self.state.projects.len() - 1))
            }
            Some(active) if active > index => Some(active - 1),
            Some(active) if active < index => Some(active),
            _ => None,
        };
        if closed_was_active {
            self.state.pending_frame_change = None;
        }
        self.leave_editor_if_unfit();
        // A close renumbers every later tab down by one, so an index-shaped
        // owner marker can end up comparing equal to a *different* tab --
        // force a reload rather than trust the comparison.
        self.state.canvas_dirty = true;
        self.settle_tab_scroll();
        self.settle_settings_scroll();
        self.emit(ShellEvent::ProjectsChanged);
    }

    // -- The projects overlay: its toggle, its gallery and the rail's
    // actions the overlay itself does not draw. --

    /// Installs the `Recent` list loaded at startup and the directory it
    /// persists to -- `main.rs`'s own read, mirroring
    /// `install_input_table`; this crate's one exception to "no fs touch in
    /// `controller.rs`" is [`recent::save`]/[`recent::load`] themselves,
    /// which live in `crate::recent`, not here.
    pub fn install_recent(&mut self, entries: Vec<recent::RecentEntry>, config_dir: PathBuf) {
        self.state.recent = entries;
        self.state.recent_config_dir = Some(config_dir);
    }

    /// The rail's own `Recent` rows, dates rendered against `now_unix` --
    /// a parameter rather than a live clock read, so a test controls it.
    pub fn recent_rows(&self, now_unix: i64) -> Vec<RecentRow> {
        self.state
            .recent
            .iter()
            .map(|e| RecentRow {
                name: e.name.clone(),
                folder: e.folder.display().to_string(),
                date_text: recent::format_date(e.opened_at, now_unix),
            })
            .collect()
    }

    /// Records `name`/`folder`/`path` at the front of the `Recent` list and
    /// persists it, if a config directory resolved at startup. A write
    /// failure is silent: the `Recent` list is a convenience, never a
    /// reason to interrupt an open.
    fn record_recent(
        &mut self,
        name: String,
        folder: PathBuf,
        path: Option<PathBuf>,
        opened_at: i64,
    ) {
        recent::record(&mut self.state.recent, name, folder, path, opened_at);
        if let Some(dir) = &self.state.recent_config_dir {
            let _ = recent::save(dir, &self.state.recent);
        }
    }

    /// Opens the `Recent` row at `index`: an already-open tab for that
    /// target is selected instead of opened again, closing the overlay the
    /// same as a fresh open; otherwise the target is opened through
    /// [`Self::open_paths`], which itself refuses a target that no longer
    /// exists. A no-op, starting no worker, when `index` is out of range or
    /// a load is already in flight.
    pub fn open_recent(&mut self, index: usize) -> Option<std::thread::JoinHandle<()>> {
        if self.state.load.in_flight {
            return None;
        }
        let entry = self.state.recent.get(index)?;
        let target = entry.path.clone().unwrap_or_else(|| entry.folder.clone());
        if let Some(tab_index) = self
            .state
            .projects
            .iter()
            .position(|t| project_identity_path(&t.project) == target)
        {
            self.select_project(tab_index);
            return None;
        }
        self.open_paths(&[target])
    }

    /// Whether the projects overlay is open.
    pub fn projects_overlay_open(&self) -> bool {
        self.state.overlay_open
    }

    /// Opens the overlay, or closes it if it is already open -- the
    /// four-square button's own toggle, and `Esc`'s. Opening also primes any
    /// tab whose picture never got a chance to (a fresh tab and a fuse
    /// settling already prime their own), the exception rather than the
    /// rule.
    pub fn toggle_projects_overlay(&mut self) {
        self.state.overlay_open = !self.state.overlay_open;
        if self.state.overlay_open {
            for tab in &mut self.state.projects {
                if tab.thumbnail.is_none() {
                    tab.thumbnail =
                        overlay::render_thumbnail(&mut tab.project, &tab.frames).map(Arc::new);
                }
            }
        }
        self.set_gallery_scroll(self.state.gallery_scroll_y);
        self.emit(ShellEvent::OverlayChanged);
    }

    /// Closes the overlay if it is open; a no-op otherwise.
    pub fn close_projects_overlay(&mut self) {
        if self.state.overlay_open {
            self.state.overlay_open = false;
            self.emit(ShellEvent::OverlayChanged);
        }
    }

    /// A key press against the window's five fixed shortcuts -- `Esc` and
    /// `Ctrl+N/O/S/W` -- checked before [`Self::key`]'s rebindable table, so
    /// none of the five can be shadowed by a user's rebind. The four
    /// Ctrl-chords apply anywhere in the window, whether or not the overlay
    /// is open; none of them acts while [`Self::modal_open`] holds. `Esc`
    /// only closes an open overlay. Every other key press reads
    /// [`ChromeKey::None`] and falls through unchanged.
    pub fn chrome_key(&mut self, text: &str, mods: Modifiers) -> ChromeKey {
        let Some(key) = text.chars().next() else {
            return ChromeKey::None;
        };
        if key == '\u{1b}' {
            if self.state.overlay_open {
                self.close_projects_overlay();
                return ChromeKey::Consumed;
            }
            return ChromeKey::None;
        }
        if !mods.ctrl {
            return ChromeKey::None;
        }
        let key = key.to_ascii_lowercase();
        if !matches!(key, 'n' | 'o' | 's' | 'w') {
            return ChromeKey::None;
        }
        if self.modal_open() {
            return ChromeKey::Consumed;
        }
        match key {
            'n' => ChromeKey::NewProject,
            'o' => ChromeKey::OpenProject,
            's' if self.save_enabled() => ChromeKey::SaveProject,
            'w' => {
                if let Some(i) = self.active_project() {
                    self.request_close(i);
                }
                ChromeKey::Consumed
            }
            // A disabled `Ctrl+S`.
            _ => ChromeKey::Consumed,
        }
    }

    /// The gallery's own heading: `"Open projects"` while every attempt this
    /// session opened, `"Projects"` the moment any did not.
    pub fn gallery_heading(&self) -> &'static str {
        if self.state.failures.is_empty() {
            "Open projects"
        } else {
            "Projects"
        }
    }

    /// The gallery's own count line, beside its heading: `"N open"`, or
    /// `"N · M did not open"` once a failure card exists.
    pub fn gallery_count_text(&self) -> String {
        if self.state.failures.is_empty() {
            format!("{} open", self.state.projects.len())
        } else {
            format!(
                "{} · {} did not open",
                self.state.projects.len() + self.state.failures.len(),
                self.state.failures.len()
            )
        }
    }

    /// Every card the gallery shows, positioned on the grid: one per open
    /// project, then one per stack that did not become one this session.
    /// `crate::bridge` is the one place this crosses into Slint.
    pub fn project_cards(&self) -> Vec<CardView> {
        let columns = overlay::columns(self.state.gallery_width);
        let running = self.running_tab();
        let progress = if running.is_some() {
            self.job.observer.as_ref().and_then(|o| o.last_progress())
        } else {
            None
        };
        let mut cards = Vec::with_capacity(self.state.projects.len() + self.state.failures.len());
        for (i, tab) in self.state.projects.iter().enumerate() {
            let fusing = running == Some(i);
            let frame_count = entry::tab_frame_count(&tab.project);
            let phase_line = fusing
                .then(|| job::phase_line(&tab.run, progress.as_ref()))
                .flatten();
            // A live run takes priority over the failure panel; the failure
            // stays underneath until a success clears it.
            let showing_failure = tab.failure.as_ref().filter(|_| !job::is_live(&tab.run));
            let (kind, headline, stage, reason, action_label) = match showing_failure {
                Some(failure) => (
                    failure.kind.card_kind().to_string(),
                    failure.kind.headline().to_string(),
                    failure.stage.clone(),
                    failure.reason.clone(),
                    failure.kind.action_label().to_string(),
                ),
                None => (
                    "open".to_string(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                ),
            };
            let (x, y) = overlay::card_origin(cards.len(), columns);
            cards.push(CardView {
                kind,
                opened: true,
                current: self.state.active == Some(i),
                unsaved: tab.unsaved(),
                fusing,
                fusing_fraction: if fusing {
                    job::bar_fraction(progress.as_ref())
                } else {
                    0.0
                },
                project_name: tab.title.clone(),
                meta_text: open_card_meta(frame_count, tab.checkpoints, phase_line.as_deref()),
                headline,
                stage,
                reason,
                action_label,
                picture: tab.thumbnail.clone(),
                x,
                y,
            });
        }
        for failure in &self.state.failures {
            let (x, y) = overlay::card_origin(cards.len(), columns);
            cards.push(CardView {
                kind: failure.kind.card_kind().to_string(),
                opened: false,
                current: false,
                unsaved: false,
                fusing: false,
                fusing_fraction: 0.0,
                project_name: failure.title.clone(),
                meta_text: failure.meta.clone(),
                headline: failure.kind.headline().to_string(),
                stage: failure.stage.clone(),
                reason: failure.reason.clone(),
                action_label: failure.kind.action_label().to_string(),
                picture: None,
                x,
                y,
            });
        }
        cards
    }

    /// Records a `Not a project` `card`, replacing any card already held for
    /// the same path.
    fn record_failure(&mut self, card: FailureCard) {
        self.state.failures.retain(|c| c.path != card.path);
        self.state.failures.push(card);
        self.emit(ShellEvent::OverlayChanged);
    }

    /// Clears any `Not a project` card held for `path` -- what a path
    /// opening successfully does to whichever such card stood for its last
    /// attempt.
    fn clear_failure(&mut self, path: &Path) {
        let before = self.state.failures.len();
        self.state.failures.retain(|c| c.path != path);
        if self.state.failures.len() != before {
            self.emit(ShellEvent::OverlayChanged);
        }
    }

    /// A gallery card's own click, by its position in [`Self::project_cards`]:
    /// switches to that project and closes the overlay for an open card, a
    /// no-op for a not-opened one -- there is nothing to switch to.
    pub fn select_gallery_card(&mut self, index: usize) {
        if index >= self.state.projects.len() {
            return;
        }
        // Closes the overlay too -- `select_project`'s own job now.
        self.select_project(index);
    }

    /// The card at gallery index `index`'s own kind string, if it is showing
    /// a failure -- what `crate::bridge` reads before deciding whether its
    /// action needs the folder picker. Reads the tab's own failure for an
    /// open project's card, the `Not a project` list for the rest.
    pub fn gallery_failure_kind(&self, index: usize) -> Option<&'static str> {
        if let Some(tab) = self.state.projects.get(index) {
            return tab.failure.as_ref().map(|f| f.kind.card_kind());
        }
        let failure_index = index.checked_sub(self.state.projects.len())?;
        self.state
            .failures
            .get(failure_index)
            .map(|f| f.kind.card_kind())
    }

    /// `Run again` on a `Did not stack`/`Cancelled` card: re-runs that tab's
    /// own kept request. Never more than one card's worth of work: a no-op
    /// while any run is already live, or with no matching failure, or with
    /// no request kept for it. Returns whether a run started.
    pub fn run_gallery_action(&mut self, index: usize) -> bool {
        if self.any_job_running() {
            return false;
        }
        let Some(tab) = self.state.projects.get(index) else {
            return false;
        };
        let Some(failure) = &tab.failure else {
            return false;
        };
        if !matches!(failure.kind, FailureKind::Failed | FailureKind::Cancelled) {
            return false;
        }
        let Some(request) = tab.request.clone() else {
            return false;
        };
        self.state.active = Some(index);
        self.run_request(index, request);
        true
    }

    /// `Locate frames…` on a `Sources missing` card: re-resolves that tab's
    /// own frames against `folder` in memory only -- no write, and no touch
    /// of the sealed `.ffp`'s own fingerprint. Clears the tab's failure only
    /// once every source resolves; a folder missing some of them changes
    /// nothing, silently.
    pub fn locate_missing_sources(&mut self, index: usize, folder: PathBuf) {
        let Some(tab) = self.state.projects.get(index) else {
            return;
        };
        let sources_missing =
            matches!(&tab.failure, Some(f) if f.kind == FailureKind::SourcesMissing);
        let GuiProject::Opened(project) = &tab.project else {
            return;
        };
        if !sources_missing {
            return;
        }
        let candidates: Vec<PathBuf> = project
            .manifest()
            .sources
            .iter()
            .map(|source| {
                let name = Path::new(&source.path)
                    .file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(&source.path));
                folder.join(name)
            })
            .collect();
        if candidates.iter().all(|p| p.is_file()) {
            let tab = &mut self.state.projects[index];
            tab.excluded = vec![false; candidates.len()];
            tab.frames = candidates;
            tab.selected = images::first_included(&tab.excluded);
            tab.failure = None;
            self.emit(ShellEvent::OverlayChanged);
        }
    }

    pub fn scroll_gallery(&mut self, delta_px: f32) {
        self.set_gallery_scroll(self.state.gallery_scroll_y + delta_px);
    }

    fn gallery_content_height(&self) -> f32 {
        let columns = overlay::columns(self.state.gallery_width);
        overlay::content_height(
            self.state.projects.len() + self.state.failures.len(),
            columns,
        )
    }

    fn set_gallery_scroll(&mut self, offset_px: f32) {
        let content_height = self.gallery_content_height();
        let viewport_height = self.state.gallery_viewport_height;
        let offset = settings_panel::clamp_panel_scroll(offset_px, content_height, viewport_height);
        if offset != self.state.gallery_scroll_y {
            self.state.gallery_scroll_y = offset;
            self.emit(ShellEvent::OverlayChanged);
        }
    }

    /// The gallery scrollbar thumb's current `(top, height)` in
    /// track-pixel space -- [`Self::images_scrollbar_thumb`]'s twin.
    fn gallery_scrollbar_thumb(&self) -> (f32, f32) {
        let content_height = self.gallery_content_height();
        let viewport_height = self.state.gallery_viewport_height;
        if content_height <= 0.0 {
            return (0.0, viewport_height);
        }
        let height = (viewport_height * viewport_height / content_height).max(16.0);
        let top = self.state.gallery_scroll_y * viewport_height / content_height;
        (top, height)
    }

    /// A press on the gallery scrollbar -- [`Self::images_scrollbar_pointer`]'s
    /// twin.
    pub fn gallery_scrollbar_pointer(&mut self, kind: PointerKind, mouse_y: f32) {
        match kind {
            PointerKind::Down => {
                let (thumb_top, thumb_height) = self.gallery_scrollbar_thumb();
                if mouse_y >= thumb_top && mouse_y < thumb_top + thumb_height {
                    self.state.gallery_scrollbar_drag = Some(mouse_y - thumb_top);
                } else {
                    let viewport_height = self.state.gallery_viewport_height.max(1.0);
                    let content_height = self.gallery_content_height();
                    let target_top = mouse_y - thumb_height / 2.0;
                    self.set_gallery_scroll(target_top * content_height / viewport_height);
                    self.state.gallery_scrollbar_drag = Some(thumb_height / 2.0);
                }
            }
            PointerKind::Up => self.state.gallery_scrollbar_drag = None,
            PointerKind::Move => {}
        }
    }

    /// The live gallery scrollbar drag, if any -- [`Self::images_scrollbar_dragged`]'s
    /// twin.
    pub fn gallery_scrollbar_dragged(&mut self, mouse_y: f32) {
        let Some(grab_offset) = self.state.gallery_scrollbar_drag else {
            return;
        };
        let viewport_height = self.state.gallery_viewport_height.max(1.0);
        let content_height = self.gallery_content_height();
        let target_top = mouse_y - grab_offset;
        self.set_gallery_scroll(target_top * content_height / viewport_height);
    }

    /// The gallery viewport's measured height -- pushed on install and on
    /// every resize; re-clamps the scroll offset.
    pub fn set_gallery_viewport_height(&mut self, height: f32) {
        self.state.gallery_viewport_height = height.max(0.0);
        let content_height = self.gallery_content_height();
        self.state.gallery_scroll_y = settings_panel::clamp_panel_scroll(
            self.state.gallery_scroll_y,
            content_height,
            self.state.gallery_viewport_height,
        );
        self.emit(ShellEvent::OverlayChanged);
    }

    pub fn gallery_scroll_y(&self) -> f32 {
        self.state.gallery_scroll_y
    }

    /// The gallery's own scrollable content height, in pixels -- what
    /// `FFScrollbar`'s `content-height` is pushed from.
    pub fn gallery_content_height_px(&self) -> f32 {
        self.gallery_content_height()
    }

    /// The gallery's measured content width -- pushed on install and on
    /// every resize; re-clamps the scroll offset since a width change moves
    /// the column count.
    pub fn set_gallery_width(&mut self, width: f32) {
        let width = width.max(0.0);
        if width == self.state.gallery_width {
            return;
        }
        self.state.gallery_width = width;
        let content_height = self.gallery_content_height();
        self.state.gallery_scroll_y = settings_panel::clamp_panel_scroll(
            self.state.gallery_scroll_y,
            content_height,
            self.state.gallery_viewport_height,
        );
        self.emit(ShellEvent::OverlayChanged);
    }

    // -- The settings panel. The model is rebuilt from scratch on every
    // call -- the schema is small, and rebuilding is what keeps "one
    // section per family, one field per `ParamDesc`" true by construction
    // rather than by an incremental patch that could drift from it. --

    /// Builds the whole panel: Fuse, Align, Crop, in that order, each
    /// section's rows flattened from `AlgorithmRef::params()`'s own
    /// declaration order. With no tab active ("+" selected) the panel still
    /// renders -- the default algorithm of each family, with no explicit
    /// values and no `StackContext`, so an `auto` parameter reads as
    /// `pending` rather than as a user's choice.
    pub fn settings_panel(&self) -> PanelModel {
        let viewport_height = self.state.settings_viewport_height;
        let active = self
            .state
            .active()
            .map(|tab| settings_panel::ActiveTabParams {
                ctx: tab.ctx.as_ref(),
                errors: &tab.errors,
                menu_open: tab.menu_open.as_deref(),
                fuse_id: &tab.fuse_id,
                fuse_values: &tab.fuse_values,
                align_id: &tab.align_id,
                align_values: &tab.align_values,
                project_values: &tab.project_values,
            });
        let mut rows = settings_panel::build_rows(active);
        settings_panel::assign_tops(&mut rows);

        let content_height = settings_panel::content_height(&rows);
        let scroll_y = settings_panel::clamp_panel_scroll(
            self.state.settings_scroll_y,
            content_height,
            viewport_height,
        );
        let sticky = settings_panel::sticky_heading(&rows, scroll_y);
        let sticky_push = settings_panel::sticky_push(&rows, sticky, scroll_y);
        PanelModel {
            rows,
            sticky,
            sticky_push,
            content_height,
            scroll_y,
            viewport_height,
        }
    }

    /// Sets one parameter's value directly -- a slider or a toggle, whose
    /// control already guarantees a value inside the schema's own bounds.
    /// One `ShellEvent::SettingsChanged` per call.
    pub fn set_param_value(&mut self, section: &str, id: &str, value: ParamValue) {
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        tab_values_mut(tab, section).insert(id.to_string(), value);
        tab.errors.remove(&settings_panel::menu_key(section, id));
        self.emit(ShellEvent::SettingsChanged);
    }

    /// Parses typed text against the field's own descriptor
    /// (`ParamValue::parse`): on `Ok` inserts the value and clears that
    /// field's error; on `Err` stores its one violation under this field's
    /// key, leaving any other field's own error untouched -- what lets two
    /// fields carry a message at the same time.
    pub fn set_param_text(&mut self, section: &str, id: &str, text: &str) {
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let Some(desc) = settings_panel::find_desc(section, &tab.fuse_id, &tab.align_id, id) else {
            return;
        };
        let key = settings_panel::menu_key(section, id);
        match ParamValue::parse(desc, text) {
            Ok(value) => {
                tab_values_mut(tab, section).insert(id.to_string(), value);
                tab.errors.remove(&key);
            }
            Err(err) => {
                let violation = err
                    .into_violations()
                    .into_iter()
                    .next()
                    .expect("ParamValue::parse returns exactly one violation");
                tab.errors.insert(key, violation);
            }
        }
        self.emit(ShellEvent::SettingsChanged);
    }

    /// Selects a Fuse or Align algorithm, clearing that section's values
    /// and errors -- a fresh algorithm resolves its own schema from
    /// scratch, never carrying over a stray value the previous one
    /// happened to share an id with.
    pub fn set_fuse_algorithm(&mut self, id: &str) {
        self.set_algorithm("fuse", id);
    }

    pub fn set_align_algorithm(&mut self, id: &str) {
        self.set_algorithm("align", id);
    }

    fn set_algorithm(&mut self, section: &str, id: &str) {
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        match section {
            "fuse" => tab.fuse_id = id.to_string(),
            "align" => tab.align_id = id.to_string(),
            _ => return,
        }
        tab_values_mut(tab, section).clear();
        let prefix = format!("{section}:");
        tab.errors.retain(|k, _| !k.starts_with(&prefix));
        self.emit(ShellEvent::SettingsChanged);
    }

    /// Removes every explicit key of every section's `ParamValues` and
    /// drops every error at once -- the mechanically exact "back to
    /// default": no stored `Provenance`, no schema change, just re-resolving
    /// with nothing explicit any more. The panel's own bottom-of-list
    /// control, one emit for all three sections.
    pub fn reset_all_sections(&mut self) {
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        tab.fuse_values.clear();
        tab.align_values.clear();
        tab.project_values.clear();
        tab.errors.clear();
        self.emit(ShellEvent::SettingsChanged);
    }

    /// Opens or closes one picker/enum menu, `key` being the row's own
    /// `"<section>:<id>"` -- only one menu is ever open at a time.
    pub fn toggle_param_menu(&mut self, key: &str) {
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        tab.menu_open = if tab.menu_open.as_deref() == Some(key) {
            None
        } else {
            Some(key.to_string())
        };
        self.emit(ShellEvent::SettingsChanged);
    }

    /// [`Self::toggle_param_menu`], composing the row key from its own
    /// section and id -- what a picker/enum row's own callback reaches
    /// through, so the `"<section>:<id>"` format stays this module's alone.
    pub fn toggle_param_menu_for(&mut self, section: &str, id: &str) {
        self.toggle_param_menu(&settings_panel::menu_key(section, id));
    }

    /// Converts a slider's thumb position back into this field's own value
    /// ([`settings_panel::slider_value`]) against its declared bounds, then
    /// stores it -- the one entry point a drag gesture reaches through.
    /// A no-op when no tab is active or the field's descriptor cannot be
    /// found (an algorithm switch mid-drag).
    pub fn set_param_fraction(&mut self, section: &str, id: &str, fraction: f32) {
        let Some(tab) = self.state.active() else {
            return;
        };
        let Some(desc) = settings_panel::find_desc(section, &tab.fuse_id, &tab.align_id, id) else {
            return;
        };
        let value = settings_panel::slider_value(fraction, &desc.kind);
        self.set_param_value(section, id, value);
    }

    /// Scrolls the settings panel by `delta_px`, clamped to its current
    /// content/viewport. Pushes only when it moved.
    pub fn scroll_settings_panel(&mut self, delta_px: f32) {
        self.set_settings_scroll(self.state.settings_scroll_y + delta_px);
    }

    /// Sets the settings panel's scroll offset to an absolute pixel
    /// position, clamped to its current content/viewport. Pushes only
    /// when it moved.
    fn set_settings_scroll(&mut self, offset_px: f32) {
        let content_height = self.settings_panel().content_height;
        let viewport_height = self.state.settings_viewport_height;
        let offset = settings_panel::clamp_panel_scroll(offset_px, content_height, viewport_height);
        if offset != self.state.settings_scroll_y {
            self.state.settings_scroll_y = offset;
            self.emit(ShellEvent::SettingsChanged);
        }
    }

    /// The settings scrollbar thumb's current `(top, height)` in
    /// track-pixel space -- the same two formulas `ff_scrollbar.slint`
    /// draws it with, so a press can tell the thumb from the bare track.
    fn settings_scrollbar_thumb(&self) -> (f32, f32) {
        let content_height = self.settings_panel().content_height;
        let viewport_height = self.state.settings_viewport_height;
        if content_height <= 0.0 {
            return (0.0, viewport_height);
        }
        let height = (viewport_height * viewport_height / content_height).max(16.0);
        let top = self.state.settings_scroll_y * viewport_height / content_height;
        (top, height)
    }

    /// A press on the settings scrollbar: on the thumb itself, grabs it
    /// at that point so `scrollbar_dragged` below can track the pointer
    /// with no jump; on the bare track, jumps there, centring the thumb
    /// on the press and grabbing its centre for the drag that follows.
    /// `PointerKind::Move` is ignored, the same reason as
    /// [`AppController::pointer`]'s own -- it carries no motion, and
    /// ending the drag here would end it on its own first movement.
    pub fn scrollbar_pointer(&mut self, kind: PointerKind, mouse_y: f32) {
        match kind {
            PointerKind::Down => {
                let (thumb_top, thumb_height) = self.settings_scrollbar_thumb();
                if mouse_y >= thumb_top && mouse_y < thumb_top + thumb_height {
                    self.state.scrollbar_drag = Some(mouse_y - thumb_top);
                } else {
                    let viewport_height = self.state.settings_viewport_height.max(1.0);
                    let content_height = self.settings_panel().content_height;
                    let target_top = mouse_y - thumb_height / 2.0;
                    self.set_settings_scroll(target_top * content_height / viewport_height);
                    self.state.scrollbar_drag = Some(thumb_height / 2.0);
                }
            }
            PointerKind::Up => self.state.scrollbar_drag = None,
            PointerKind::Move => {}
        }
    }

    /// The live settings-scrollbar drag, if any: `mouse_y` minus the grab
    /// offset is the thumb's new top edge, converted back to a
    /// content-space offset the same way `settings_scrollbar_thumb`
    /// converts the other way.
    pub fn scrollbar_dragged(&mut self, mouse_y: f32) {
        let Some(grab_offset) = self.state.scrollbar_drag else {
            return;
        };
        let viewport_height = self.state.settings_viewport_height.max(1.0);
        let content_height = self.settings_panel().content_height;
        let target_top = mouse_y - grab_offset;
        self.set_settings_scroll(target_top * content_height / viewport_height);
    }

    /// The settings panel viewport's measured height -- pushed on install
    /// and on every resize; re-clamps the scroll offset.
    pub fn set_settings_viewport_height(&mut self, height: f32) {
        self.state.settings_viewport_height = height.max(0.0);
        let content_height = self.settings_panel().content_height;
        self.state.settings_scroll_y = settings_panel::clamp_panel_scroll(
            self.state.settings_scroll_y,
            content_height,
            self.state.settings_viewport_height,
        );
        self.emit(ShellEvent::SettingsChanged);
    }

    pub fn settings_scroll_y(&self) -> f32 {
        self.state.settings_scroll_y
    }

    // -- The `Images` panel. `frames`/`excluded` live on the active
    // `ProjectTab`; every mutation below is a no-op outside stacking mode,
    // and every frame-set-changing one -- everything but the reversible
    // exclude -- is gated behind the active tab's own checkpoint count. --

    /// Builds the whole `Images` panel for the active tab -- empty with no
    /// tab active ("+" selected).
    pub fn images_panel(&self) -> images::ImagesModel {
        let viewport_height = self.state.images_viewport_height;
        let legend = self.mode() == ShellMode::Editor;
        let Some(tab) = self.state.active() else {
            return images::build_model(&[], &[], None, 0.0, legend);
        };
        let content_height = tab.frames.len() as f32 * images::ROW_HEIGHT;
        let scroll_y = settings_panel::clamp_panel_scroll(
            self.state.images_scroll_y,
            content_height,
            viewport_height,
        );
        images::build_model(
            &tab.frames,
            &tab.excluded,
            Some(tab.selected),
            scroll_y,
            legend,
        )
    }

    /// The active tab's checkpoint count, `0` with none active -- what every
    /// frame-set-change gate below reads.
    fn frame_change_checkpoints(&self) -> usize {
        self.checkpoint_count().unwrap_or(0)
    }

    /// Toggles one frame's exclusion -- reversible, changes no other frame's
    /// position, never gated behind a checkpoint (the reversible default).
    /// A no-op outside stacking mode, past the frame list's end, or while
    /// this tab's own job is running.
    pub fn toggle_frame_excluded(&mut self, index: usize) {
        if self.mode() != ShellMode::Stacking || self.job_running() {
            return;
        }
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let Some(excluded) = tab.excluded.get_mut(index) else {
            return;
        };
        *excluded = !*excluded;
        self.emit(ShellEvent::ImagesChanged);
    }

    /// Selects the frame at `index` for the left pane. No gate: which frame
    /// is on the canvas changes no frame set, so this works even mid-run or
    /// past the checkpoint count that gates a set-changing edit. A no-op
    /// past the frame list's end or with no tab active.
    pub fn select_frame(&mut self, index: usize) {
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        if index >= tab.frames.len() {
            return;
        }
        tab.selected = index;
        self.emit(ShellEvent::ImagesChanged);
    }

    /// The selected frame's path -- `None` with no tab active or an empty
    /// frame list.
    pub fn selected_frame_path(&self) -> Option<PathBuf> {
        self.state.active()?.selected_frame().cloned()
    }

    /// The selected frame's own view onto the active tab's newest fuse --
    /// `None` before any fuse, when the left pane draws the raw frame. The
    /// transform is looked up by path in the fused sources, identity for a
    /// frame added to the tab after its last fuse.
    pub fn selected_frame_view(&self) -> Option<FrameView> {
        let tab = self.state.active()?;
        let (sources, registration) = tab.fused()?;
        let path = tab.selected_frame()?;
        let transform = sources
            .iter()
            .position(|source| source == path)
            .map(|i| registration.transform(i))
            .unwrap_or_else(job::Transform::identity);
        let extent = display_image(tab)?;
        Some(FrameView {
            transform,
            origin: registration.origin(),
            extent: (extent.width, extent.height),
        })
    }

    /// The left pane's label: the selected frame's file name, or empty with
    /// no tab active or nothing selected. In editor mode, appends the
    /// brush source's own position, 1-based, out of the tab's frame count
    /// (`tab.selected` is also the brush source).
    pub fn frame_pane_label(&self) -> String {
        let Some(name) = self
            .selected_frame_path()
            .map(|path| images::file_name(&path))
        else {
            return String::new();
        };
        if self.mode() != ShellMode::Editor {
            return name;
        }
        let tab = self
            .state
            .active()
            .expect("selected_frame_path returned Some, so an active tab exists");
        format!(
            "{name} · source {} of {}",
            tab.selected + 1,
            tab.frames.len()
        )
    }

    /// Adds `paths` to the active tab: hashes on a worker thread, then
    /// [`Self::settle_import`] applies the scan. An [`ImportRefused`] leaves
    /// the tab untouched.
    pub fn add_frames(
        &mut self,
        paths: &[PathBuf],
    ) -> Result<Option<std::thread::JoinHandle<()>>, ImportRefused> {
        let Some(tab) = self.state.active() else {
            return Ok(None);
        };
        if paths.is_empty() {
            return Ok(None);
        }
        if self.mode() != ShellMode::Stacking {
            return Err(ImportRefused::NotStacking);
        }
        if self.job_running() {
            return Err(ImportRefused::JobRunning);
        }
        if self.state.import.in_flight {
            return Err(ImportRefused::StillChecking);
        }
        let incoming = entry::expand_import(paths)?;
        let tab_id = tab.id;
        let frames = tab.frames.clone();

        self.state.import.in_flight = true;
        let sink = self.sink.clone();
        let outcome_slot = Arc::clone(&self.state.import.outcome);
        Ok(Some(std::thread::spawn(move || {
            let (add, replace) = entry::diff_import(&frames, incoming);
            *outcome_slot
                .lock()
                .expect("no thread panicked while holding the import outcome") = Some(ImportScan {
                tab_id,
                frames,
                add,
                replace,
            });
            if let Some(sink) = &sink {
                sink(ShellEvent::ImportReady);
            }
        })))
    }

    /// Adds a drop to the active tab through [`Self::add_frames`]; with no
    /// active tab, or for a lone saved project, opens it through
    /// [`Self::open_paths`] instead.
    pub fn drop_paths(
        &mut self,
        paths: &[PathBuf],
    ) -> Result<Option<std::thread::JoinHandle<()>>, ImportRefused> {
        if self.state.active().is_some() && !entry::is_lone_project(paths) {
            return self.add_frames(paths);
        }
        Ok(self.open_paths(paths))
    }

    /// Applies the scan [`Self::add_frames`] parked, only to the tab it
    /// scanned and only while that tab is active, idle in Stacking mode and
    /// its frame list unchanged; otherwise discards it.
    pub fn settle_import(&mut self) {
        let Some(scan) = self
            .state
            .import
            .outcome
            .lock()
            .expect("no thread panicked while holding the import outcome")
            .take()
        else {
            return;
        };
        self.state.import.in_flight = false;
        let Some(index) = self.tab_index(scan.tab_id) else {
            return;
        };
        if self.state.active != Some(index)
            || self.mode() != ShellMode::Stacking
            || self.job_running()
            || self.state.projects[index].frames != scan.frames
        {
            return;
        }
        let plan = ImportPlan {
            add: scan.add,
            replace: scan.replace,
        };
        if plan.replace.is_empty() {
            self.stage_import(plan);
        } else {
            self.state.pending_frame_change = Some(PendingFrameChange::Duplicates(plan));
            self.emit(ShellEvent::ImagesChanged);
        }
    }

    /// Stages a conflict-free import: nothing for an empty plan, the counted
    /// "Add and re-fuse" warning once a checkpoint exists, or applies at
    /// once before the first one. Always emits `ImagesChanged`, so an
    /// answered duplicate question closes even when its plan is empty.
    fn stage_import(&mut self, plan: ImportPlan) {
        if !plan.is_empty() {
            if self.frame_change_checkpoints() > 0 {
                self.state.pending_frame_change = Some(PendingFrameChange::Add(plan));
            } else {
                self.apply_add_frames(&plan);
            }
        }
        self.emit(ShellEvent::ImagesChanged);
    }

    fn apply_add_frames(&mut self, plan: &ImportPlan) {
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        for (position, path) in &plan.replace {
            if let Some(slot) = tab.frames.get_mut(*position) {
                *slot = path.clone();
            }
        }
        tab.frames.extend_from_slice(&plan.add);
        tab.excluded
            .extend(std::iter::repeat_n(false, plan.add.len()));
        tab.frame_sort = None;
    }

    /// Removes the frame at `index` -- immediately, before the first
    /// checkpoint; behind the counted warning once one exists. A no-op
    /// outside stacking mode, past the frame list's end, or while this
    /// tab's own job is running.
    pub fn remove_frame(&mut self, index: usize) {
        if self.mode() != ShellMode::Stacking || self.job_running() {
            return;
        }
        let Some(tab) = self.state.active() else {
            return;
        };
        if index >= tab.frames.len() {
            return;
        }
        if self.frame_change_checkpoints() > 0 {
            self.state.pending_frame_change = Some(PendingFrameChange::Remove(index));
        } else {
            self.apply_remove_frame(index);
        }
        self.emit(ShellEvent::ImagesChanged);
    }

    fn apply_remove_frame(&mut self, index: usize) {
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        if index >= tab.frames.len() {
            return;
        }
        tab.frames.remove(index);
        if index < tab.excluded.len() {
            tab.excluded.remove(index);
        }
        tab.selected = if tab.selected > index {
            tab.selected - 1
        } else {
            tab.selected.min(tab.frames.len().saturating_sub(1))
        };
    }

    /// Drags the frame at `from` to `to` -- the manual order, which clears
    /// the sort menu's own selection. Immediate before the first checkpoint,
    /// behind the counted warning once one exists. A no-op outside stacking
    /// mode or while this tab's own job is running.
    pub fn move_frame(&mut self, from: usize, to: usize) {
        if self.mode() != ShellMode::Stacking || self.job_running() {
            return;
        }
        let Some(tab) = self.state.active() else {
            return;
        };
        if from >= tab.frames.len() || to >= tab.frames.len() {
            return;
        }
        if self.frame_change_checkpoints() > 0 {
            self.state.pending_frame_change = Some(PendingFrameChange::Move { from, to });
        } else {
            self.apply_move_frame(from, to);
        }
        self.emit(ShellEvent::ImagesChanged);
    }

    fn apply_move_frame(&mut self, from: usize, to: usize) {
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let order = images::move_order(tab.frames.len(), from, to);
        tab.reorder(&order);
        tab.frame_sort = None;
    }

    /// Sorts the active tab's frame list by `sort` -- one code path for "the
    /// order changed": the same gating and application [`Self::move_frame`]
    /// uses for a manual drag. A no-op while this tab's own job is running.
    pub fn set_frame_sort(&mut self, sort: images::FrameSort) {
        if self.mode() != ShellMode::Stacking || self.state.active().is_none() || self.job_running()
        {
            return;
        }
        self.state.images_sort_menu_open = false;
        if self.frame_change_checkpoints() > 0 {
            self.state.pending_frame_change = Some(PendingFrameChange::Sort(sort));
        } else {
            self.apply_sort(sort);
        }
        self.emit(ShellEvent::ImagesChanged);
    }

    /// Opens or closes the sort menu -- the same externally-driven
    /// convention [`Self::toggle_param_menu`] uses for a settings row. A
    /// no-op outside stacking mode.
    pub fn toggle_images_sort_menu(&mut self) {
        if self.mode() != ShellMode::Stacking {
            return;
        }
        self.state.images_sort_menu_open = !self.state.images_sort_menu_open;
        self.emit(ShellEvent::ImagesChanged);
    }

    pub fn images_sort_menu_open(&self) -> bool {
        self.state.images_sort_menu_open
    }

    /// The row currently being dragged, if any -- the origin slot's own
    /// placeholder styling and the floating copy [`Self::drag_offset`]
    /// positions both render against it.
    pub fn dragging_row(&self) -> Option<usize> {
        self.state.drag_row.map(|drag| drag.from)
    }

    /// The live pixel offset a drag has moved by since the press, clamped so
    /// the dragged row's floating copy never leaves the list's own content
    /// band. `0.0` with no drag in progress.
    pub fn drag_offset(&self) -> f32 {
        let Some(drag) = self.state.drag_row else {
            return 0.0;
        };
        let Some(tab) = self.state.active() else {
            return 0.0;
        };
        let top = drag.from as f32 * images::ROW_HEIGHT;
        let content_height = tab.frames.len() as f32 * images::ROW_HEIGHT;
        let max = (content_height - images::ROW_HEIGHT - top).max(0.0);
        (drag.current_y - drag.press_y).clamp(-top, max)
    }

    /// The row index a drag would land at if released right now -- the same
    /// rounding [`Self::frame_drag_pointer`]'s own release arm applies,
    /// read live off the drag's current position rather than a fresh
    /// pointer sample. `None` with no drag in progress.
    pub fn drag_target_row(&self) -> Option<usize> {
        let drag = self.state.drag_row?;
        Some(self.drag_target(&drag, drag.current_y))
    }

    /// The grip's raw pointer stream for the row at `index`: `Down` starts a
    /// drag there; `Up` commits it, moving the row by however many whole
    /// [`images::ROW_HEIGHT`] steps `mouse_y` has travelled from the press;
    /// `Move` updates [`Self::drag_offset`], the floating copy's live
    /// position. A no-op outside stacking mode or while this tab's own job
    /// is running.
    pub fn frame_drag_pointer(&mut self, index: usize, kind: PointerKind, mouse_y: f32) {
        if self.mode() != ShellMode::Stacking || self.job_running() {
            return;
        }
        match kind {
            PointerKind::Down => {
                self.state.drag_row = Some(DragRow {
                    from: index,
                    press_y: mouse_y,
                    current_y: mouse_y,
                });
                self.emit(ShellEvent::ImagesChanged);
            }
            PointerKind::Move => {
                let Some(drag) = self.state.drag_row.as_mut() else {
                    return;
                };
                if drag.from != index {
                    return;
                }
                drag.current_y = mouse_y;
                self.emit(ShellEvent::ImagesChanged);
            }
            PointerKind::Up => {
                let Some(drag) = self.state.drag_row.take() else {
                    return;
                };
                let target = self.drag_target(&drag, mouse_y);
                if target != drag.from {
                    self.move_frame(drag.from, target);
                } else {
                    self.emit(ShellEvent::ImagesChanged);
                }
            }
        }
    }

    fn drag_target(&self, drag: &DragRow, mouse_y: f32) -> usize {
        let Some(tab) = self.state.active() else {
            return drag.from;
        };
        let last = tab.frames.len().saturating_sub(1);
        let steps = ((mouse_y - drag.press_y) / images::ROW_HEIGHT).round() as isize;
        (drag.from as isize + steps).clamp(0, last as isize) as usize
    }

    fn apply_sort(&mut self, sort: images::FrameSort) {
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let order = images::sort_order(&tab.frames, sort);
        tab.reorder(&order);
        tab.frame_sort = Some(sort);
    }

    /// The sort menu's own selection -- `None` while the order came from a
    /// manual drag, or no tab is active.
    pub fn frame_sort(&self) -> Option<images::FrameSort> {
        self.state.active().and_then(|tab| tab.frame_sort)
    }

    /// The frame-set-change warning still open, if any -- `None` whenever
    /// the active tab carries no checkpoint of its own to invalidate, since
    /// there is nothing for a frame-set edit to disagree with. A pending
    /// duplicate question shows regardless of checkpoint count: it is about
    /// the import decision itself, not about re-fuse risk.
    pub fn pending_frame_change(&self) -> Option<images::FrameSetWarningCopy> {
        let pending = self.state.pending_frame_change.as_ref()?;
        if let PendingFrameChange::Duplicates(plan) = pending {
            let tab = self.state.active()?;
            let names: Vec<String> = plan
                .replace
                .iter()
                .filter_map(|&(position, _)| tab.frames.get(position).map(|p| images::file_name(p)))
                .collect();
            return Some(images::duplicate_question_copy(&names));
        }
        if self.frame_change_checkpoints() == 0 {
            return None;
        }
        Some(images::frame_set_warning_copy(
            pending.kind(),
            self.frame_change_checkpoints(),
        ))
    }

    /// The close question still open, if any: which tab, how many
    /// checkpoints closing it loses, and where saving it would write.
    pub fn pending_close(&self) -> Option<CloseGuardCopy> {
        let id = self.state.pending_close?;
        let tab = self.state.projects.iter().find(|tab| tab.id == id)?;
        let n = tab.held.len();
        let (body, them) = if n == 1 {
            (
                "1 checkpoint has been fused since the last save. It belongs to no saved \
                 project.\n\nClosing without saving loses it."
                    .to_string(),
                "it",
            )
        } else {
            (
                format!(
                    "{n} checkpoints have been fused since the last save. They belong to no \
                     saved project.\n\nClosing without saving loses them."
                ),
                "them",
            )
        };
        let info = match &tab.project {
            GuiProject::Unsaved(_) => "Saving asks where to write the project file.".to_string(),
            GuiProject::Opened(project) => format!(
                "Saving adds {them} to {}.",
                project
                    .path()
                    .file_name()
                    .unwrap_or(project.path().as_os_str())
                    .to_string_lossy()
            ),
        };
        Some(CloseGuardCopy {
            title: "Save this state first?".to_string(),
            context: format!("Closing {}", tab.title),
            body,
            info,
            secondary_label: "Save and close".to_string(),
            confirm_label: "Close without saving".to_string(),
        })
    }

    /// The pending close's tab index, clearing the question either way.
    fn take_pending_close(&mut self) -> Option<usize> {
        let id = self.state.pending_close.take()?;
        self.state.projects.iter().position(|tab| tab.id == id)
    }

    /// `Cancel` on the close question: the tab stays open, all it holds kept.
    pub fn cancel_close(&mut self) {
        self.state.pending_close = None;
        self.emit(ShellEvent::ProjectsChanged);
    }

    /// `Close without saving`: closes the asking tab and drops its held
    /// checkpoints.
    pub fn close_without_saving(&mut self) {
        match self.take_pending_close() {
            Some(index) => self.remove_tab(index, Instant::now()),
            None => self.emit(ShellEvent::ProjectsChanged),
        }
    }

    /// `Save and close`: saves the asking tab as [`Self::save_project_at`]
    /// does, then closes it only if the save wrote. Returns whether it wrote;
    /// the question is cleared either way.
    pub fn save_and_close(&mut self, pick: impl FnOnce(&Path) -> Option<PathBuf>) -> bool {
        let Some(index) = self.take_pending_close() else {
            self.emit(ShellEvent::ProjectsChanged);
            return false;
        };
        if !self.save_project_at(index, pick) {
            self.emit(ShellEvent::ProjectsChanged);
            return false;
        }
        self.remove_tab(index, Instant::now());
        true
    }

    /// The quit question still open, if any: how many tabs hold checkpoints
    /// no `.ffp` contains or a parked run nobody decided on.
    pub fn pending_quit_copy(&self) -> Option<CloseGuardCopy> {
        if !self.state.pending_quit {
            return None;
        }
        let n = self
            .state
            .projects
            .iter()
            .filter(|tab| tab.quit_loses_work())
            .count();
        let body = if n == 1 {
            "1 project has unsaved work. Quitting now loses all of it.".to_string()
        } else {
            format!("{n} projects have unsaved work. Quitting now loses all of it.")
        };
        Some(CloseGuardCopy {
            title: "Quit without saving?".to_string(),
            context: "Quitting FocusForge".to_string(),
            body,
            info: String::new(),
            secondary_label: "Save all and quit".to_string(),
            confirm_label: "Quit without saving".to_string(),
        })
    }

    /// `Cancel` on the quit question: every tab stays open, all it holds
    /// kept.
    pub fn cancel_quit(&mut self) {
        self.state.pending_quit = false;
        self.emit(ShellEvent::ProjectsChanged);
    }

    /// `Quit without saving`: closes the window, dropping every tab's held
    /// checkpoints.
    pub fn quit_without_saving(&mut self) {
        self.state.pending_quit = false;
        self.remove_parked_runs();
        self.emit(ShellEvent::WindowCloseRequested);
    }

    /// `Save all and quit`: saves every tab holding checkpoints no `.ffp`
    /// contains, in order, as [`Self::save_project_at`] does. The first save
    /// that does not write stops the loop and cancels the quit -- whatever
    /// already saved stays saved. Returns whether every save wrote.
    pub fn save_all_and_quit(&mut self, pick: impl Fn(&Path) -> Option<PathBuf>) -> bool {
        let indices: Vec<usize> = self
            .state
            .projects
            .iter()
            .enumerate()
            .filter(|(_, tab)| !tab.held.is_empty())
            .map(|(index, _)| index)
            .collect();
        for index in indices {
            if !self.save_project_at(index, &pick) {
                self.state.pending_quit = false;
                self.emit(ShellEvent::ProjectsChanged);
                return false;
            }
        }
        self.state.pending_quit = false;
        self.remove_parked_runs();
        self.emit(ShellEvent::WindowCloseRequested);
        true
    }

    /// The close guard's `Cancel`, whichever question is open -- a close
    /// question if one is pending, otherwise a quit question. A no-op with
    /// neither open.
    pub fn close_guard_cancelled(&mut self) {
        if self.state.pending_close.is_some() {
            self.cancel_close();
        } else if self.state.pending_quit {
            self.cancel_quit();
        }
    }

    /// The close guard's secondary answer, whichever question is open --
    /// [`Self::save_and_close`] for a close question, [`Self::save_all_and_quit`]
    /// for a quit question. `false` with neither open.
    pub fn close_guard_secondary_pressed(
        &mut self,
        pick: impl Fn(&Path) -> Option<PathBuf>,
    ) -> bool {
        if self.state.pending_close.is_some() {
            self.save_and_close(pick)
        } else if self.state.pending_quit {
            self.save_all_and_quit(pick)
        } else {
            false
        }
    }

    /// The close guard's destructive answer, whichever question is open --
    /// [`Self::close_without_saving`] for a close question,
    /// [`Self::quit_without_saving`] for a quit question. A no-op with
    /// neither open.
    pub fn close_guard_confirmed(&mut self) {
        if self.state.pending_close.is_some() {
            self.close_without_saving();
        } else if self.state.pending_quit {
            self.quit_without_saving();
        }
    }

    /// Applies the pending frame-set change and re-fuses -- the ordinary
    /// [`Self::start_fuse`], which appends a checkpoint onto an
    /// already-`Opened` tab rather than discarding any. A no-op, returning
    /// `false`, with nothing pending.
    pub fn confirm_frame_change(&mut self) -> bool {
        let Some(pending) = self.state.pending_frame_change.take() else {
            return false;
        };
        match pending {
            PendingFrameChange::Add(plan) => self.apply_add_frames(&plan),
            // Never calls `start_fuse` directly; stages through the same
            // checkpoint gate every other add goes through.
            PendingFrameChange::Duplicates(plan) => {
                let existing = self
                    .state
                    .active()
                    .map(|tab| tab.frames.clone())
                    .unwrap_or_default();
                self.stage_import(plan.without_same_path_replacements(&existing));
                return false;
            }
            PendingFrameChange::Move { from, to } => self.apply_move_frame(from, to),
            PendingFrameChange::Sort(sort) => self.apply_sort(sort),
            PendingFrameChange::Remove(index) => self.apply_remove_frame(index),
        }
        self.emit(ShellEvent::ImagesChanged);
        self.start_fuse()
    }

    /// The duplicate question's non-destructive answer: keeps every
    /// existing frame, staging only the incoming files that were not
    /// conflicts (through the same checkpoint gate every other add goes
    /// through). `false` with nothing pending, or nothing pending a
    /// duplicate answer.
    pub fn keep_existing(&mut self) -> bool {
        match self.state.pending_frame_change.take() {
            Some(PendingFrameChange::Duplicates(plan)) => {
                self.stage_import(ImportPlan {
                    add: plan.add,
                    replace: Vec::new(),
                });
                true
            }
            other => {
                self.state.pending_frame_change = other;
                false
            }
        }
    }

    /// Withdraws the pending frame-set change with nothing applied -- the
    /// project stays exactly as it was.
    pub fn cancel_frame_change(&mut self) {
        self.state.pending_frame_change = None;
        self.emit(ShellEvent::ImagesChanged);
    }

    /// `Remove`'s own non-destructive footer button: withdraws the pending
    /// removal and excludes that frame instead -- the reversible default,
    /// immediate, no re-fuse. `false` when nothing pending is a removal.
    pub fn exclude_instead(&mut self) -> bool {
        match self.state.pending_frame_change.take() {
            Some(PendingFrameChange::Remove(index)) => {
                self.toggle_frame_excluded(index);
                true
            }
            other => {
                self.state.pending_frame_change = other;
                false
            }
        }
    }

    /// Every frame-set warning's own non-destructive alternative: opens a
    /// new, checkpoint-free tab over the active tab's current frame set and
    /// explicit Fuse/Align settings, withdraws the pending change, and
    /// leaves the active tab exactly as it was. `false` with nothing
    /// pending or no active tab.
    pub fn new_project_instead(&mut self) -> bool {
        if self.state.pending_frame_change.take().is_none() {
            return false;
        }
        let Some(tab) = self.state.active() else {
            return false;
        };
        let dir = match &tab.project {
            GuiProject::Opened(project) => project
                .path()
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default(),
            GuiProject::Unsaved(group) => group.dir.clone(),
        };
        let name = format!("{} copy", tab.title);
        self.next_tab_id += 1;
        let id = self.next_tab_id;
        let mut new_tab = ProjectTab::new(
            entry::duplicate_as_unsaved(dir, name, tab.frames.clone()),
            id,
            ThumbnailSource::NotAttempted,
        );
        new_tab.fuse_id = tab.fuse_id.clone();
        new_tab.align_id = tab.align_id.clone();
        new_tab.fuse_values = tab.fuse_values.clone();
        new_tab.align_values = tab.align_values.clone();
        self.state.projects.push(new_tab);
        self.set_active_tab(Some(self.state.projects.len() - 1));
        self.emit(ShellEvent::ProjectsChanged);
        true
    }

    fn set_images_scroll(&mut self, offset_px: f32) {
        let content_height = self.images_panel().content_height;
        let viewport_height = self.state.images_viewport_height;
        let offset = settings_panel::clamp_panel_scroll(offset_px, content_height, viewport_height);
        if offset != self.state.images_scroll_y {
            self.state.images_scroll_y = offset;
            self.emit(ShellEvent::ImagesChanged);
        }
    }

    /// The `Images` panel scrollbar thumb's current `(top, height)` in
    /// track-pixel space -- [`Self::settings_scrollbar_thumb`]'s twin.
    fn images_scrollbar_thumb(&self) -> (f32, f32) {
        let content_height = self.images_panel().content_height;
        let viewport_height = self.state.images_viewport_height;
        if content_height <= 0.0 {
            return (0.0, viewport_height);
        }
        let height = (viewport_height * viewport_height / content_height).max(16.0);
        let top = self.state.images_scroll_y * viewport_height / content_height;
        (top, height)
    }

    /// A press on the `Images` scrollbar -- [`Self::scrollbar_pointer`]'s
    /// twin.
    pub fn images_scrollbar_pointer(&mut self, kind: PointerKind, mouse_y: f32) {
        match kind {
            PointerKind::Down => {
                let (thumb_top, thumb_height) = self.images_scrollbar_thumb();
                if mouse_y >= thumb_top && mouse_y < thumb_top + thumb_height {
                    self.state.images_scrollbar_drag = Some(mouse_y - thumb_top);
                } else {
                    let viewport_height = self.state.images_viewport_height.max(1.0);
                    let content_height = self.images_panel().content_height;
                    let target_top = mouse_y - thumb_height / 2.0;
                    self.set_images_scroll(target_top * content_height / viewport_height);
                    self.state.images_scrollbar_drag = Some(thumb_height / 2.0);
                }
            }
            PointerKind::Up => self.state.images_scrollbar_drag = None,
            PointerKind::Move => {}
        }
    }

    /// The live `Images` scrollbar drag, if any -- [`Self::scrollbar_dragged`]'s
    /// twin.
    pub fn images_scrollbar_dragged(&mut self, mouse_y: f32) {
        let Some(grab_offset) = self.state.images_scrollbar_drag else {
            return;
        };
        let viewport_height = self.state.images_viewport_height.max(1.0);
        let content_height = self.images_panel().content_height;
        let target_top = mouse_y - grab_offset;
        self.set_images_scroll(target_top * content_height / viewport_height);
    }

    /// The `Images` panel viewport's measured height -- pushed on install
    /// and on every resize; re-clamps the scroll offset.
    pub fn set_images_viewport_height(&mut self, height: f32) {
        self.state.images_viewport_height = height.max(0.0);
        let content_height = self.images_panel().content_height;
        self.state.images_scroll_y = settings_panel::clamp_panel_scroll(
            self.state.images_scroll_y,
            content_height,
            self.state.images_viewport_height,
        );
        self.emit(ShellEvent::ImagesChanged);
    }

    pub fn images_scroll_y(&self) -> f32 {
        self.state.images_scroll_y
    }

    /// The current index of the tab carrying id `id`, if it is still open --
    /// what lets the live-run slot and the queue outlive a close that shifts
    /// every later index down.
    fn tab_index(&self, id: u64) -> Option<usize> {
        self.state.projects.iter().position(|tab| tab.id == id)
    }

    /// The tab a worker thread is actually running against, if any --
    /// `None` the instant the worker has finished and its outcome not yet
    /// read, and `None` once its own tab has closed.
    fn running_tab(&self) -> Option<usize> {
        if self.job.control.is_some() {
            self.job.tab_id.and_then(|id| self.tab_index(id))
        } else {
            None
        }
    }

    /// Whether the *active* tab's own run is live -- the deferred-capability
    /// pattern `editor_enabled`/`export_enabled` already use. The panel
    /// reads this as a whole-panel disable, scoped to whichever tab is in
    /// front.
    pub fn job_running(&self) -> bool {
        self.state
            .active()
            .is_some_and(|tab| job::is_live(&tab.run))
    }

    /// Whether a worker thread is live at all, regardless of which tab it
    /// belongs to -- what refuses a second concurrent start. Stays `true`
    /// across the gap between a worker finishing and [`Self::settle_job`]
    /// reading its outcome.
    pub fn any_job_running(&self) -> bool {
        self.job.control.is_some()
    }

    /// Whether tab `index` would join the queue if its primary button
    /// (`Fuse stack` or, on a failed tab, `Retry`) were pressed right now:
    /// another run is live and this tab holds none of it -- neither the
    /// live run, a place in the queue, nor a pause.
    pub fn would_queue(&self, index: usize) -> bool {
        self.any_job_running()
            && self.state.projects.get(index).is_some_and(|tab| {
                !matches!(tab.run, job::RunState::Running | job::RunState::Paused)
            })
    }

    /// Whether there is work the shell still owes an answer to -- a live
    /// worker or a non-empty queue. Broader than [`Self::any_job_running`]:
    /// a queued tab has no worker of its own, but the poll timer still has
    /// to keep running for it.
    pub fn work_pending(&self) -> bool {
        self.any_job_running() || !self.job.queue.is_empty()
    }

    /// Rewrites every queued tab's [`job::RunState::Queued`] position from
    /// [`JobSlot::queue`]'s current order -- the live run holds position 1,
    /// so the queue's head reads `2`. Called at the end of every push,
    /// removal or pop so [`job::stage_word`] stays a pure function of the
    /// state it is handed.
    fn renumber_queue(&mut self) {
        let ids: Vec<u64> = self.job.queue.iter().copied().collect();
        for (i, id) in ids.into_iter().enumerate() {
            if let Some(index) = self.tab_index(id)
                && let Some(tab) = self.state.projects.get_mut(index)
            {
                tab.run = job::RunState::Queued { position: i + 2 };
            }
        }
    }

    /// Assembles the [`job::JobRequest`] tab `index` would run right now,
    /// from its current frames and explicit settings -- what a direct start
    /// and a promotion out of the queue both call, so a queued tab's request
    /// is built at the moment it starts rather than at the moment it was
    /// queued. `None` only if `index` no longer names an open tab.
    fn fuse_request_for(&self, index: usize) -> Option<job::JobRequest> {
        let tab = self.state.projects.get(index)?;
        let frames = tab.included_frames();
        let project_path = match &tab.project {
            GuiProject::Unsaved(group) => group.dir.join(format!("{}.ffp", group.name)),
            GuiProject::Opened(project) => project.path().to_path_buf(),
        };
        let staging_key = format!("{}-{:x}", tab.title, path_digest(&project_path));
        let scratch_root = self.scratch_root();
        Some(job::JobRequest {
            frames,
            hold_dir: scratch_root
                .join("held")
                .join(&staging_key)
                .join(tab.checkpoints.to_string()),
            scratch_root,
            staging_key,
            fuse_id: tab.fuse_id.clone(),
            fuse_values: tab.fuse_values.clone(),
            align_id: Some(tab.align_id.clone()),
            align_values: tab.align_values.clone(),
            align_lax: true,
            crop: true,
            gpu: self.gpu_fusion,
        })
    }

    /// Turns GPU-backed fusion on or off for every run this controller
    /// starts from here on -- never mid-run.
    pub fn set_gpu_fusion(&mut self, gpu_fusion: bool) {
        self.gpu_fusion = gpu_fusion;
    }

    /// The live run's own [`job::resolve_backend`] account, `None` before it
    /// has resolved or once the run has settled ([`Self::settle_job`] clears
    /// the observer).
    pub fn fusion_backend(&self) -> Option<String> {
        self.job.observer.as_ref().and_then(|o| o.backend_line())
    }

    /// Refuses a run whose recorded frame is no longer on disk, naming it
    /// in the message -- shared by [`Self::start_fuse`] and the queue
    /// promotion [`Self::settle_job`] runs.
    fn refuse_missing_frame(&mut self, missing: &Path) {
        self.refuse_message(format!(
            "{} is no longer on disk. This project cannot be fused again until it is back.",
            missing.display()
        ));
    }

    /// If another run is live, enqueues tab `id` unless it already holds
    /// the live run or a queue slot -- the shared tail of [`Self::start_fuse`]
    /// and [`Self::retry_fuse`]. `None` when no run is live, so the caller
    /// falls through to starting immediately.
    fn try_enqueue(&mut self, id: u64) -> Option<bool> {
        if !self.any_job_running() {
            return None;
        }
        if self.job.tab_id == Some(id) || self.job.queue.contains(&id) {
            return Some(false);
        }
        self.job.queue.push_back(id);
        self.renumber_queue();
        self.emit(ShellEvent::JobChanged);
        self.leave_editor_if_unfit();
        Some(true)
    }

    /// Starts a fuse run on the active tab, off the UI thread -- refuses
    /// while the tab carries a settings violation or a recorded frame is
    /// missing, checked before anything else so an invalid tab is never
    /// queued. A completed run adds a held checkpoint to the tab and writes
    /// no project. While another run is live, a valid press enqueues
    /// the tab instead. Returns whether a run started or was enqueued.
    pub fn start_fuse(&mut self) -> bool {
        let Some(active) = self.state.active else {
            return false;
        };
        let Some(tab) = self.state.active() else {
            return false;
        };
        if !tab.errors.is_empty() {
            return false;
        }
        let frames = tab.included_frames();
        if let Some(missing) = frames.iter().find(|path| !path.is_file()) {
            self.refuse_missing_frame(missing);
            return false;
        }
        if let Some(enqueued) = self.try_enqueue(tab.id) {
            return enqueued;
        }
        let Some(request) = self.fuse_request_for(active) else {
            return false;
        };
        self.run_request(active, request)
    }

    /// Pauses a running job, or resumes a paused one -- a no-op while the
    /// active tab is not the one actually running, or that run is
    /// cancelling, failed, idle or fused.
    pub fn pause_or_resume(&mut self) {
        let Some(control) = self.job.control.clone() else {
            return;
        };
        let running = self.running_tab();
        if running.is_none() || running != self.state.active {
            return;
        }
        let Some(tab) = running.and_then(|i| self.state.projects.get_mut(i)) else {
            return;
        };
        match tab.run {
            job::RunState::Running => {
                control.pause();
                tab.run = job::RunState::Paused;
            }
            job::RunState::Paused => {
                control.clear();
                if let Some(observer) = &self.job.observer {
                    observer.resume();
                }
                tab.run = job::RunState::Running;
            }
            _ => return,
        }
        self.emit(ShellEvent::JobChanged);
    }

    /// Requests a cancel, or leaves the queue. A queued active tab is
    /// removed from the queue and returned to `Idle`, never touching the
    /// live run's control. Otherwise the cancel lands at the next frame
    /// boundary during alignment, or the next tile during fusion, showing
    /// `Cancelling` until then. A no-op unless the active tab is queued or
    /// is the one actually running.
    pub fn cancel_fuse(&mut self) {
        if let Some(id) = self.state.active().map(|tab| tab.id)
            && let Some(pos) = self.job.queue.iter().position(|&queued| queued == id)
        {
            self.job.queue.remove(pos);
            if let Some(tab) = self.state.active_mut() {
                tab.run = job::RunState::Idle;
            }
            self.renumber_queue();
            self.emit(ShellEvent::JobChanged);
            return;
        }
        let Some(control) = self.job.control.clone() else {
            return;
        };
        let running = self.running_tab();
        if running.is_none() || running != self.state.active {
            return;
        }
        let Some(tab) = running.and_then(|i| self.state.projects.get_mut(i)) else {
            return;
        };
        control.cancel();
        if let Some(observer) = &self.job.observer {
            observer.resume();
        }
        tab.run = job::RunState::Cancelling;
        self.emit(ShellEvent::JobChanged);
    }

    /// Starts the same work again, from the request the active tab's own
    /// failed run stored -- controller-level only: the engine itself carries
    /// no notion of retry. Enqueues behind another live run rather than
    /// refusing, same as [`Self::start_fuse`]; the stored request is reused
    /// unchanged, since a failed run's own request was valid enough to have
    /// started once.
    pub fn retry_fuse(&mut self) -> bool {
        let Some(active) = self.state.active else {
            return false;
        };
        let Some(tab) = self.state.projects.get(active) else {
            return false;
        };
        let Some(request) = tab.request.clone() else {
            return false;
        };
        let id = tab.id;
        if let Some(enqueued) = self.try_enqueue(id) {
            return enqueued;
        }
        self.run_request(active, request)
    }

    /// Starts `request` on tab `tab_index`, off the UI thread, discarding a
    /// run the tab still has parked. Refuses with a message, leaving the tab
    /// as it was, when an algorithm id does not resolve. Returns whether the
    /// run started.
    fn run_request(&mut self, tab_index: usize, request: job::JobRequest) -> bool {
        let algorithms = match job::resolve_request(&request) {
            Ok(algorithms) => algorithms,
            Err(message) => {
                self.refuse_message(message);
                return false;
            }
        };
        let control = job::JobControl::new();
        let observer = Arc::new(job::GuiObserver::new());
        self.job.control = Some(Arc::clone(&control));
        self.job.observer = Some(Arc::clone(&observer));
        self.job.tab_id = self.state.projects.get(tab_index).map(|tab| tab.id);
        if let Some(tab) = self.state.projects.get_mut(tab_index) {
            // The new run holds into the same directory the parked one uses.
            if let Some(parked) = tab.parked.take() {
                remove_held(std::slice::from_ref(&parked.held));
            }
        }
        self.job.handle = Some(job::spawn(request.clone(), algorithms, control, observer));
        if let Some(tab) = self.state.projects.get_mut(tab_index) {
            tab.run = job::RunState::Running;
            tab.request = Some(request);
        }
        self.emit(ShellEvent::JobChanged);
        self.leave_editor_if_unfit();
        true
    }

    /// The active tab's own failed run message, `None` outside
    /// [`job::RunState::Failed`].
    pub fn job_failure_message(&self) -> Option<&str> {
        match self.state.active().map(|tab| &tab.run) {
            Some(job::RunState::Failed(message)) => Some(message.as_str()),
            _ => None,
        }
    }

    /// The most recent run or save failure, taken exactly once -- `None`
    /// once already read, or with no failure to announce. What the failure
    /// toast names, so it fires only for the run that actually failed and
    /// never re-opens on a later poll.
    pub fn take_settled_failure(&mut self) -> Option<job::Notification> {
        self.settled_failure.take()
    }

    /// Every run and save failure since the window opened, oldest first,
    /// kept after its toast has been dismissed.
    pub fn notifications(&self) -> &[job::Notification] {
        &self.notifications
    }

    /// The toolbar/bottom-bar surfaces' current view of the active tab's
    /// run, empty rather than another tab's numbers with no active tab or
    /// with the active tab not the one actually running.
    pub fn job_view(&self) -> JobView {
        let run = self
            .state
            .active()
            .map(|tab| tab.run.clone())
            .unwrap_or_default();
        let progress = if self.state.active == self.running_tab() {
            self.job.observer.as_ref().and_then(|o| o.last_progress())
        } else {
            None
        };
        JobView {
            toolbar_run_state: job::toolbar_run_state(&run).to_string(),
            stage_word: job::stage_word(&run, progress.as_ref()),
            step_text: job::progress_text(&run, progress.as_ref()),
            eta_text: job::eta_text(&run, progress.as_ref()),
            bar_tone: job::bar_tone(&run).to_string(),
            fraction: job::bar_fraction(progress.as_ref()),
            fuse_enabled: self.fuse_enabled(),
            state: run,
        }
    }

    /// Whether the active tab's fuse control is enabled: a real project tab
    /// holding two or more frames -- `false` with `"+"` selected, with no
    /// tab, or below two frames, `Unsaved` or `Opened` alike.
    pub fn fuse_enabled(&self) -> bool {
        self.state
            .active()
            .is_some_and(|tab| entry::tab_frame_count(&tab.project) >= 2)
    }

    /// The active tab's checkpoint count -- real, incrementing on every
    /// successful run, `None` while no tab is active.
    pub fn checkpoint_count(&self) -> Option<usize> {
        self.state.active().map(|tab| tab.checkpoints)
    }

    /// The checkpoints panel's own line: the active tab's checkpoint count in
    /// the shared wording, or the empty-state line while no tab is active or
    /// it holds none yet.
    pub fn checkpoint_panel_line(&self) -> String {
        match self.checkpoint_count() {
            Some(count) if count > 0 => checkpoint_phrase(count),
            _ => "None yet — fuse a stack".to_string(),
        }
    }

    /// Whether the active tab has a composite to show: at least one
    /// checkpoint. `false` while no tab is active.
    pub fn composite_ready(&self) -> bool {
        self.checkpoint_count().is_some_and(|count| count > 0)
    }

    /// Joins the worker thread if it has finished, moving the shell out of
    /// `Running`/`Cancelling` and, on completion, adding the run's held
    /// checkpoint to its tab. Returns the held composite and Z-Map paths for
    /// the bridge to feed the canvas; `None` while still running or on any
    /// other outcome. Never blocks.
    pub fn settle_job(&mut self) -> Option<(PathBuf, PathBuf)> {
        let handle = self.job.handle.take()?;
        if !handle.is_finished() {
            self.job.handle = Some(handle);
            return None;
        }
        // Read before the observer is cleared below: the phase this run was
        // on when it stopped is what a failure or cancellation card's own
        // stage line names.
        let progress = self.job.observer.as_ref().and_then(|o| o.last_progress());
        let phase = job::progress_phase(progress.as_ref());
        let tab_index = self.job.tab_id.take().and_then(|id| self.tab_index(id));
        let owner = tab_index.and_then(|i| self.state.projects.get(i));
        let was_cancelling = owner.is_some_and(|tab| tab.run == job::RunState::Cancelling);
        let settled = job::settle(handle.join());
        self.job.control = None;
        self.job.observer = None;
        // With the owning tab gone (closed mid-run), this writes nothing --
        // no tab gains a checkpoint, and no outcome lands on whichever tab
        // happens to be active now.
        if let Some(tab) = tab_index.and_then(|i| self.state.projects.get_mut(i)) {
            tab.run = settled.state.clone();
        }
        if tab_index == self.state.active {
            // The run's own tab is the active one -- the canvas must reload
            // even though the owner already matches, since its newest
            // checkpoint just changed.
            self.state.canvas_dirty = true;
        }
        let tab_info = tab_index
            .and_then(|i| self.state.projects.get(i))
            .map(|tab| (project_identity_path(&tab.project), tab.title.clone()));

        match &settled.state {
            job::RunState::Failed(message) => {
                let operation = settled
                    .operation
                    .expect("settle() sets operation exactly when state is Failed");
                let notification = job::Notification {
                    operation,
                    message: message.clone(),
                    project_title: tab_info.as_ref().map(|(_, title)| title.clone()),
                    project_path: tab_info.as_ref().map(|(path, _)| path.clone()),
                    target: None,
                };
                self.notifications.push(notification.clone());
                self.settled_failure = Some(notification);
                if let Some(tab) = tab_index.and_then(|i| self.state.projects.get_mut(i)) {
                    tab.failure = Some(TabFailure {
                        kind: FailureKind::Failed,
                        stage: format!("FAILED WHILE {}", phase.name().to_uppercase()),
                        reason: message.clone(),
                    });
                }
            }
            job::RunState::Idle if was_cancelling => {
                if let Some(tab) = tab_index.and_then(|i| self.state.projects.get_mut(i)) {
                    tab.failure = Some(TabFailure {
                        kind: FailureKind::Cancelled,
                        stage: format!("STOPPED WHILE {}", phase.name().to_uppercase()),
                        reason: "You stopped this one. Nothing was written.".to_string(),
                    });
                }
            }
            _ => {}
        }

        let owner = tab_index.filter(|&i| i < self.state.projects.len());
        let planes = match (settled.held, owner) {
            (None, _) => None,
            (Some(held), None) => {
                remove_held(std::slice::from_ref(&held));
                None
            }
            (Some(held), Some(index)) if !settled.diverged.is_empty() => {
                let tab = &mut self.state.projects[index];
                tab.run = job::RunState::Idle;
                tab.parked = Some(ParkedRun {
                    held,
                    diverged: settled.diverged,
                });
                None
            }
            (Some(held), Some(index)) => Some(self.commit_held(index, held)),
        };
        // Promotes the queue head: an id that no longer resolves (its tab
        // closed) or whose recorded frames are no longer all on disk is
        // dropped and refused, and the next id in line is tried. A popped
        // tab is out of the queue either way, so its run leaves `Queued`
        // here and a promotion overwrites it with `Running` below.
        while let Some(id) = self.job.queue.pop_front() {
            let Some(index) = self.tab_index(id) else {
                continue;
            };
            if let Some(tab) = self.state.projects.get_mut(index) {
                tab.run = job::RunState::Idle;
            }
            let missing = self
                .state
                .projects
                .get(index)
                .and_then(|tab| tab.included_frames().into_iter().find(|p| !p.is_file()));
            if let Some(missing) = missing {
                self.refuse_missing_frame(&missing);
                continue;
            }
            let Some(request) = self.fuse_request_for(index) else {
                continue;
            };
            if self.run_request(index, request) {
                break;
            }
        }
        self.renumber_queue();
        self.emit(ShellEvent::JobChanged);
        planes
    }

    /// Adds `held` to tab `index` as its newest checkpoint, re-seeding the
    /// frame list from the tab's own last request, and returns its composite
    /// and Z-Map paths for the canvas. Switches the shell into Editor mode
    /// when `index` is the tab currently in view.
    fn commit_held(&mut self, index: usize, held: job::HeldCheckpoint) -> (PathBuf, PathBuf) {
        let tab = &mut self.state.projects[index];
        tab.checkpoints += 1;
        if let Some(request) = &tab.request {
            tab.frames = request.frames.clone();
        }
        tab.excluded = vec![false; tab.frames.len()];
        tab.selected = images::first_included(&tab.excluded);
        tab.frame_sort = None;
        tab.edit = None;
        tab.thumbnail = overlay::render_thumbnail_from_frame(&held.composite).map(Arc::new);
        tab.failure = None;
        let planes = (held.composite.clone(), held.zmap.clone());
        tab.held.push(held);
        // The new checkpoint's own extent may differ from what the viewport
        // was fitted to (a first fuse's crop, a re-fuse over a changed frame
        // set) -- refit on the next `fit_if_unfitted` rather than keep
        // drawing the old extent's zoom/pan.
        tab.viewport = None;
        self.enter_editor_if_viewing(index);
        planes
    }

    /// Switches into Editor mode when `index` is the tab currently in view --
    /// a fuse that commits on a tab the user has since navigated away from
    /// leaves the shell where it is.
    fn enter_editor_if_viewing(&mut self, index: usize) {
        if self.state.active == Some(index) {
            self.set_mode(ShellMode::Editor);
        }
    }

    /// The frames the active tab's parked run could not align, by file name;
    /// `None` while it has no parked run.
    pub fn pending_divergence(&self) -> Option<&[String]> {
        let parked = self.state.active()?.parked.as_ref()?;
        Some(parked.diverged.as_slice())
    }

    /// `Keep` on the active tab's parked run: commits it exactly as a run
    /// without divergence commits, returning the planes the canvas reloads
    /// from. `None` with nothing parked.
    pub fn keep_fused(&mut self) -> Option<(PathBuf, PathBuf)> {
        let index = self.state.active?;
        let parked = self.state.projects.get_mut(index)?.parked.take()?;
        let at = job::format_utc_hhmm(parked.held.created_at);
        let planes = self.commit_held(index, parked.held);
        self.state.projects[index].run = job::RunState::Fused { at };
        self.state.canvas_dirty = true;
        self.emit(ShellEvent::JobChanged);
        Some(planes)
    }

    /// `Discard` on the active tab's parked run: deletes its held files and
    /// leaves the tab `Idle` with its checkpoint count unchanged.
    pub fn discard_fused(&mut self) {
        let Some(parked) = self.state.active_mut().and_then(|tab| tab.parked.take()) else {
            return;
        };
        remove_held(std::slice::from_ref(&parked.held));
        self.emit(ShellEvent::JobChanged);
    }

    /// Deletes every tab's parked run -- what quitting does with a decision
    /// nobody made.
    fn remove_parked_runs(&mut self) {
        for tab in &mut self.state.projects {
            if let Some(parked) = tab.parked.take() {
                remove_held(std::slice::from_ref(&parked.held));
            }
        }
    }

    /// The active tab's composite and Z-Map paths: its newest held
    /// checkpoint's, else an opened project's planes materialized into its
    /// cache directory. `None` for an [`GuiProject::Unsaved`] tab that holds
    /// nothing yet.
    pub fn materialize_active_tab_planes(&mut self) -> Option<(PathBuf, PathBuf)> {
        let scratch_root = self.scratch_root();
        let tab = self.state.active_mut()?;
        if let Some(held) = tab.held.last() {
            return Some((held.composite.clone(), held.zmap.clone()));
        }
        let GuiProject::Opened(project) = &mut tab.project else {
            return None;
        };
        job::materialize_planes(project, &scratch_root).ok()
    }

    /// The active tab's checkpoints fused since its last save, oldest first;
    /// empty with no active tab.
    pub fn held_checkpoints(&self) -> &[job::HeldCheckpoint] {
        self.active_project()
            .and_then(|i| self.state.projects.get(i))
            .map_or(&[], |tab| tab.held.as_slice())
    }

    /// Whether the active tab holds checkpoints no `.ffp` contains yet --
    /// what enables `Save project` and its `Ctrl+S`.
    pub fn save_enabled(&self) -> bool {
        !self.held_checkpoints().is_empty()
    }

    /// [`Self::save_project_at`] on the active tab; `false` with none active.
    pub fn save_active_project(&mut self, pick: impl FnOnce(&Path) -> Option<PathBuf>) -> bool {
        self.active_project()
            .is_some_and(|index| self.save_project_at(index, pick))
    }

    /// Writes tab `index`'s held checkpoints into one `.ffp`: a never-saved
    /// tab asks `pick`, proposing `<frames dir>/<group name>.ffp`; an opened
    /// one appends to its own file. A failed write keeps everything held and
    /// leaves [`Self::take_settled_failure`] a message. Returns whether it wrote.
    pub fn save_project_at(
        &mut self,
        index: usize,
        pick: impl FnOnce(&Path) -> Option<PathBuf>,
    ) -> bool {
        let Some(tab) = self.state.projects.get(index) else {
            return false;
        };
        if tab.held.is_empty() {
            return false;
        }
        let (target, onto_existing) = match &tab.project {
            GuiProject::Opened(project) => (project.path().to_path_buf(), true),
            GuiProject::Unsaved(group) => {
                match pick(&group.dir.join(format!("{}.ffp", group.name))) {
                    Some(path) => (path, false),
                    None => return false,
                }
            }
        };
        let project = match job::save_held(&target, &tab.held, onto_existing) {
            Ok(project) => project,
            Err(err) => {
                let notification = job::Notification {
                    operation: job::FailedOperation::Save,
                    message: format!("Could not save {}: {err}", target.display()),
                    project_title: Some(tab.title.clone()),
                    project_path: Some(project_identity_path(&tab.project)),
                    target: Some(target.clone()),
                };
                self.notifications.push(notification.clone());
                self.settled_failure = Some(notification);
                return false;
            }
        };
        let tab = &mut self.state.projects[index];
        tab.project = GuiProject::Opened(Box::new(project));
        tab.title = entry::tab_title(&tab.project);
        remove_held(&tab.held);
        tab.held.clear();
        self.state.canvas_dirty = true;
        self.emit(ShellEvent::ProjectsChanged);
        true
    }

    /// What the open renderer cell should do for the frame about to draw,
    /// given which tab it currently holds (`None` before anything has
    /// loaded). `Keep` when the owner already matches the active tab and
    /// nothing changed underneath it since; otherwise materializes the
    /// active tab's own planes and answers `Load`, or `Clear` when it has
    /// none (no tab active, `+` selected, or nothing fused yet).
    pub fn canvas_feed(&mut self, loaded: Option<usize>) -> crate::canvas::CanvasFeed {
        if loaded == self.state.active && !self.state.canvas_dirty {
            return crate::canvas::CanvasFeed::Keep;
        }
        self.state.canvas_dirty = false;
        let Some(tab) = self.state.active else {
            return crate::canvas::CanvasFeed::Clear;
        };
        match self.materialize_active_tab_planes() {
            Some((composite, zmap)) => crate::canvas::CanvasFeed::Load {
                tab,
                composite,
                zmap,
            },
            None => crate::canvas::CanvasFeed::Clear,
        }
    }

    /// Re-clamps the settings scroll offset against the now-active tab's
    /// own panel content, and pushes it -- called by every place the active
    /// tab changes.
    fn settle_settings_scroll(&mut self) {
        let content_height = self.settings_panel().content_height;
        self.state.settings_scroll_y = settings_panel::clamp_panel_scroll(
            self.state.settings_scroll_y,
            content_height,
            self.state.settings_viewport_height,
        );
    }

    // -- The scrolling tab strip. The arithmetic is the free functions
    // below `impl AppController` ([`tabs_overflow`],
    // [`tab_viewport_width`], [`clamp_tab_scroll`],
    // [`tab_scroll_into_view`]); `.slint` only reads the results. --

    /// `tabs-region`'s width at `ShellGeometry.base-width`: 1440 minus the
    /// 46 px projects slot, the 105 px window-glyph cluster (`base-width -
    /// glyph-first-x`) and the 32 px
    /// `strip-cluster-gap` -- see [`AppController::new`] for why the region
    /// starts here rather than at zero.
    const BASE_TAB_REGION_WIDTH: f32 = 1257.0;

    /// The tabs-region's measured width, pushed from `FFTabStrip`'s
    /// `tabs-region-width` (at install, and on every change). Re-clamps the
    /// offset, so a wider window never leaves an empty gap at the right, and
    /// always pushes on a real change: a narrower strip can leave the offset
    /// where it was yet light the right arrow or switch to the overflow layout.
    pub fn set_tab_region_width(&mut self, width: f32) {
        let width = width.max(0.0);
        if width == self.state.tab_region_width {
            return;
        }
        self.state.tab_region_width = width;
        self.state.tab_scroll_x = self.clamped_tab_scroll(self.state.tab_scroll_x);
        self.emit(ShellEvent::TabScrollChanged);
    }

    fn clamped_tab_scroll(&self, offset: f32) -> f32 {
        let count = self.state.projects.len();
        let viewport = tab_viewport_width(count, self.state.tab_region_width);
        clamp_tab_scroll(offset, tab_content_width(count), viewport)
    }

    /// Re-clamps after the tab set changed, then brings the active tab into
    /// view. Emits nothing: every caller emits `ProjectsChanged`, which
    /// pushes the scroll state too.
    fn settle_tab_scroll(&mut self) {
        let count = self.state.projects.len();
        let clamped = self.clamped_tab_scroll(self.state.tab_scroll_x);
        self.state.tab_scroll_x = match self.state.active {
            Some(index) => tab_scroll_into_view(
                clamped,
                index,
                count,
                tab_viewport_width(count, self.state.tab_region_width),
            ),
            None => clamped,
        };
    }

    /// Scrolls by `steps` whole tab-pitches -- negative is left, positive is
    /// right -- clamped by [`clamp_tab_scroll`]. Pushes only when it moved.
    pub fn scroll_tabs(&mut self, steps: i32) {
        let offset = self.clamped_tab_scroll(self.state.tab_scroll_x + steps as f32 * TAB_PITCH);
        if offset != self.state.tab_scroll_x {
            self.state.tab_scroll_x = offset;
            self.emit(ShellEvent::TabScrollChanged);
        }
    }

    pub fn tab_scroll_x(&self) -> f32 {
        self.state.tab_scroll_x
    }

    /// Whether the strip is in its overflow layout: arrows shown, "+" pinned.
    pub fn tabs_overflow(&self) -> bool {
        tabs_overflow(self.state.projects.len(), self.state.tab_region_width)
    }

    pub fn can_scroll_left(&self) -> bool {
        self.state.tab_scroll_x > 0.0
    }

    pub fn can_scroll_right(&self) -> bool {
        self.state.tab_scroll_x < self.clamped_tab_scroll(f32::MAX)
    }

    pub fn settings_section(&self) -> SettingsSection {
        self.state.settings_section
    }

    pub fn select_settings_section(&mut self, section: SettingsSection) {
        self.state.settings_section = section;
    }

    /// Feeds the diagnostics panel its data and configuration directories
    /// and derives the default destination from the data directory:
    /// `<data_dir>/diagnostics/<default file name>`, or the file name alone
    /// when no data directory resolves.
    pub fn set_diagnostics_dirs(&mut self, data_dir: Option<PathBuf>, config_dir: Option<PathBuf>) {
        self.state.diagnostics.data_dir = data_dir;
        self.state.diagnostics.config_dir = config_dir;
        let destination = self.default_destination();
        self.set_diagnostics_destination(destination);
    }

    /// `<data_dir>/diagnostics/<a freshly timestamped file name>`, or the
    /// file name alone when no data directory resolves -- never the same
    /// path twice in the same second, so a destination this builds is never
    /// found existing by the write that follows it.
    fn default_destination(&self) -> PathBuf {
        let file_name = focusforge_diagnostics::bundle::default_file_name();
        match &self.state.diagnostics.data_dir {
            Some(dir) => dir.join("diagnostics").join(file_name),
            None => PathBuf::from(file_name),
        }
    }

    /// `bundle::write` refuses an existing path unless forced, so every
    /// destination change re-derives whether that is the state to show.
    fn set_diagnostics_destination(&mut self, path: PathBuf) {
        self.state.diagnostics.status = if path.exists() {
            DiagnosticsStatus::ExistingFile
        } else {
            DiagnosticsStatus::Idle
        };
        self.state.diagnostics.destination = path;
    }

    pub fn set_destination(&mut self, text: &str) {
        self.set_diagnostics_destination(PathBuf::from(text));
    }

    /// Sets the choice and starts the rescan its new value needs, so no
    /// caller can change it without one.
    pub fn set_include_minidumps(&mut self, include: bool) -> std::thread::JoinHandle<()> {
        self.state.diagnostics.include_minidumps = include;
        self.start_manifest_scan()
    }

    /// Where a write attempt currently stands -- what the footer's one
    /// button does on a click depends on it (`bridge.rs`'s
    /// `on_diagnostics_primary_clicked`).
    pub fn diagnostics_status(&self) -> DiagnosticsStatus {
        self.state.diagnostics.status
    }

    /// Starts a bundle write in the background: marks the view `Writing`
    /// synchronously, then writes on a thread that creates the
    /// destination's parent directory, forces an overwrite exactly when the
    /// destination already existed, and reports its outcome through the
    /// cloned sink.
    pub fn start_bundle_write(&mut self) -> std::thread::JoinHandle<()> {
        let force = self.state.diagnostics.status == DiagnosticsStatus::ExistingFile;
        self.state.diagnostics.status = DiagnosticsStatus::Writing;
        let sink = self.sink.clone();
        let options = focusforge_diagnostics::bundle::Options {
            data_dir: self.state.diagnostics.data_dir.clone(),
            config_dir: self.state.diagnostics.config_dir.clone(),
            include_minidumps: self.state.diagnostics.include_minidumps,
        };
        let destination = self.state.diagnostics.destination.clone();

        std::thread::spawn(move || {
            let manifest = focusforge_diagnostics::bundle::manifest(&options);
            if let Some(parent) = destination.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let redaction = focusforge_diagnostics::bundle::Redaction::from_env();
            let status = match focusforge_diagnostics::bundle::write(
                &manifest,
                &destination,
                force,
                &redaction,
            ) {
                Ok(written) => {
                    if let Some(data_dir) = options.data_dir.as_deref() {
                        let bundled = focusforge_diagnostics::crash_notice::crash_ids(
                            data_dir,
                            &written.sources,
                        );
                        // Best effort: the bundle is written either way.
                        if let Err(e) =
                            focusforge_diagnostics::crash_notice::mark_reported(data_dir, &bundled)
                        {
                            tracing::warn!(
                                error = %e,
                                "the packaged crashes could not be marked as reported"
                            );
                        }
                    }
                    DiagnosticsStatus::Written
                }
                Err(e) => {
                    tracing::warn!(error = %e, "diagnostics bundle write failed");
                    DiagnosticsStatus::Failed
                }
            };
            if let Some(sink) = sink {
                sink(ShellEvent::DiagnosticsWriteFinished(status));
            }
        })
    }

    /// Starts a manifest scan in the background: bumps the generation so a
    /// superseded report is discarded, then on a spawned thread builds the
    /// manifest and resolves every entry's byte size (`text.len()` /
    /// `fs::metadata`) into a [`DiagnosticsScan`] snapshot. Reports through
    /// the cloned sink; no directory I/O happens on the main thread.
    pub fn start_manifest_scan(&mut self) -> std::thread::JoinHandle<()> {
        self.state.diagnostics.scan_generation += 1;
        let generation = self.state.diagnostics.scan_generation;
        let sink = self.sink.clone();
        let options = focusforge_diagnostics::bundle::Options {
            data_dir: self.state.diagnostics.data_dir.clone(),
            config_dir: self.state.diagnostics.config_dir.clone(),
            include_minidumps: self.state.diagnostics.include_minidumps,
        };

        std::thread::spawn(move || {
            let manifest = focusforge_diagnostics::bundle::manifest(&options);
            let entries: Vec<DiagnosticsScanEntry> = manifest
                .entries
                .iter()
                .map(|entry| {
                    let bytes = match &entry.source {
                        focusforge_diagnostics::bundle::EntrySource::Generated(text) => {
                            text.len() as u64
                        }
                        focusforge_diagnostics::bundle::EntrySource::File(path) => {
                            fs::metadata(path).map(|m| m.len()).unwrap_or(0)
                        }
                    };
                    DiagnosticsScanEntry {
                        name: entry.name.clone(),
                        bytes,
                        redacted: entry.redacted,
                    }
                })
                .collect();
            let scan = DiagnosticsScan {
                entries,
                has_logs: manifest.has_logs(),
                has_crash_evidence: manifest.has_crash_evidence(),
            };
            if let Some(sink) = sink {
                sink(ShellEvent::DiagnosticsScanFinished { generation, scan });
            }
        })
    }

    /// Stores a background write's outcome -- called once
    /// [`ShellEvent::DiagnosticsWriteFinished`] reaches the event loop,
    /// since the thread that produced it holds no lock on `self`.
    pub fn finish_bundle_write(&mut self, status: DiagnosticsStatus) {
        self.state.diagnostics.status = status;
        if status != DiagnosticsStatus::Written {
            return;
        }
        // A fresh destination for the next export, so a second click needs
        // no extra step. `default_file_name` is timestamped to the second,
        // so two writes started within the same second can still collide --
        // rare enough that landing on the ordinary existing-file state
        // (rather than keeping `Written`) is the whole answer.
        let fresh = self.default_destination();
        if fresh.exists() {
            self.state.diagnostics.status = DiagnosticsStatus::ExistingFile;
        }
        self.state.diagnostics.destination = fresh;
        // A bundle written from the window already marks the crashes it
        // packaged; dropping the notice's own snapshot means a user who
        // only ever bundles never sees it return for that evidence.
        self.state.diagnostics.crash_notice_ids.clear();
        self.state.diagnostics.crash_notice_visible = false;
    }

    /// Stores a background scan's outcome, called once
    /// [`ShellEvent::DiagnosticsScanFinished`] arrives. Drops a report whose
    /// generation a later rescan has already superseded; otherwise refreshes
    /// the cached content height and scroll clamp via [`Self::diagnostics_view`].
    pub fn finish_manifest_scan(&mut self, generation: u64, scan: DiagnosticsScan) {
        if generation != self.state.diagnostics.scan_generation {
            return;
        }
        self.state.diagnostics.scan = Some((generation, scan));
        self.diagnostics_view();
    }

    /// Snapshots `data_dir`'s unreported crash evidence and shows the notice
    /// exactly when that snapshot is non-empty. No data directory and an
    /// unreadable crash directory both yield an empty snapshot and a hidden
    /// notice -- neither ever fails the window's start-up. Called once at
    /// start; nothing re-scans mid-session.
    pub fn scan_crash_notice(&mut self) {
        let ids = match &self.state.diagnostics.data_dir {
            Some(dir) => focusforge_diagnostics::crash_notice::unreported(dir),
            None => Vec::new(),
        };
        self.state.diagnostics.crash_notice_visible = !ids.is_empty();
        self.state.diagnostics.crash_notice_ids = ids;
    }

    pub fn crash_notice_visible(&self) -> bool {
        self.state.diagnostics.crash_notice_visible
    }

    pub fn crash_notice_count(&self) -> usize {
        self.state.diagnostics.crash_notice_ids.len()
    }

    /// "1 crash found" / "N crashes found", worded for the toast's title.
    pub fn crash_notice_text(&self) -> String {
        match self.crash_notice_count() {
            1 => "1 crash found".to_string(),
            n => format!("{n} crashes found"),
        }
    }

    /// Marks the snapshot's crash ids reported, best-effort: a write
    /// failure is logged and never surfaced, mirroring the CLI's own
    /// precedent. Takes the snapshot, so a second call marks nothing.
    fn mark_crash_notice_reported(&mut self) {
        self.state.diagnostics.crash_notice_visible = false;
        let ids = std::mem::take(&mut self.state.diagnostics.crash_notice_ids);
        if ids.is_empty() {
            return;
        }
        let Some(data_dir) = self.state.diagnostics.data_dir.clone() else {
            return;
        };
        if let Err(e) = focusforge_diagnostics::crash_notice::mark_reported(&data_dir, &ids) {
            tracing::warn!(error = %e, "the crash notice's evidence could not be marked as reported");
        }
    }

    /// Hides the notice without writing anything: the evidence stays
    /// unreported, so it returns on the next start.
    pub fn crash_notice_timed_out(&mut self) {
        self.state.diagnostics.crash_notice_visible = false;
    }

    /// Hides the notice and marks its evidence reported.
    pub fn dismiss_crash_notice(&mut self) {
        self.mark_crash_notice_reported();
    }

    /// Marks the notice's evidence reported the same way [`Self::dismiss_crash_notice`]
    /// does, then opens the settings modal on the diagnostics section.
    /// Returns the settings-open state the way [`Self::open_settings`] does,
    /// so a caller pushes it without a second lock.
    pub fn review_crashes(&mut self) -> bool {
        self.mark_crash_notice_reported();
        let open = self.open_settings();
        self.select_settings_section(SettingsSection::Diagnostics);
        open
    }

    /// A press on the manifest scrollbar: on the thumb itself, grabs it at
    /// that point so a following drag tracks the pointer with no jump; on
    /// the bare track, jumps there, centring the thumb on the press and
    /// grabbing its centre for the drag that follows. `PointerKind::Move`
    /// is ignored -- it carries no motion, and ending the drag here would
    /// end it on its own first movement.
    pub fn manifest_scrollbar_pointer(&mut self, kind: PointerKind, mouse_y: f32) {
        match kind {
            PointerKind::Down => {
                let (thumb_top, thumb_height) = self.manifest_scrollbar_thumb();
                if mouse_y >= thumb_top && mouse_y < thumb_top + thumb_height {
                    self.state.diagnostics.manifest_scrollbar_drag = Some(mouse_y - thumb_top);
                } else {
                    let viewport_height = self.state.diagnostics.manifest_viewport_height.max(1.0);
                    let content_height = self.state.diagnostics.manifest_content_height;
                    let target_top = mouse_y - thumb_height / 2.0;
                    self.set_manifest_scroll(target_top * content_height / viewport_height);
                    self.state.diagnostics.manifest_scrollbar_drag = Some(thumb_height / 2.0);
                }
            }
            PointerKind::Up => self.state.diagnostics.manifest_scrollbar_drag = None,
            PointerKind::Move => {}
        }
    }

    /// The live manifest-scrollbar drag, if any: `mouse_y` minus the grab
    /// offset is the thumb's new top edge, converted back to a
    /// content-space offset the same way `manifest_scrollbar_thumb`
    /// converts the other way.
    pub fn manifest_scrollbar_dragged(&mut self, mouse_y: f32) {
        let Some(grab_offset) = self.state.diagnostics.manifest_scrollbar_drag else {
            return;
        };
        let viewport_height = self.state.diagnostics.manifest_viewport_height.max(1.0);
        let content_height = self.state.diagnostics.manifest_content_height;
        let target_top = mouse_y - grab_offset;
        self.set_manifest_scroll(target_top * content_height / viewport_height);
    }

    fn set_manifest_scroll(&mut self, offset_px: f32) {
        let content_height = self.state.diagnostics.manifest_content_height;
        let viewport_height = self.state.diagnostics.manifest_viewport_height;
        self.state.diagnostics.manifest_scroll_y =
            clamp_manifest_scroll(offset_px, content_height, viewport_height);
    }

    /// The manifest scrollbar thumb's current `(top, height)` in
    /// track-pixel space -- the same two formulas the scrollbar itself
    /// draws it with, so a press can tell the thumb from the bare track.
    fn manifest_scrollbar_thumb(&self) -> (f32, f32) {
        let content_height = self.state.diagnostics.manifest_content_height;
        let viewport_height = self.state.diagnostics.manifest_viewport_height;
        if content_height <= 0.0 {
            return (0.0, viewport_height);
        }
        let height = (viewport_height * viewport_height / content_height).max(16.0);
        let top = self.state.diagnostics.manifest_scroll_y * viewport_height / content_height;
        (top, height)
    }

    /// The manifest list's measured viewport height -- pushed once, when
    /// the panel is first constructed; re-clamps the scroll offset.
    pub fn set_manifest_viewport_height(&mut self, height: f32) {
        self.state.diagnostics.manifest_viewport_height = height.max(0.0);
        self.set_manifest_scroll(self.state.diagnostics.manifest_scroll_y);
    }

    /// Scrolls the manifest list by `delta_px`, clamped to its current
    /// content/viewport -- the wheel's own path, alongside the scrollbar's.
    pub fn manifest_scrolled(&mut self, delta_px: f32) {
        self.set_manifest_scroll(self.state.diagnostics.manifest_scroll_y + delta_px);
    }

    /// The manifest list's current scroll offset -- cheap to read after any
    /// of the methods above, since none of them touch the stored scan
    /// (unlike [`Self::diagnostics_view`], which re-clamps against it).
    pub fn manifest_scroll_offset(&self) -> f32 {
        self.state.diagnostics.manifest_scroll_y
    }

    /// The destination field's cursor moved to `cursor_x`, measured from
    /// the field's own unscrolled left edge; `viewport_width` is the
    /// field's currently visible width. Scrolls the field by the least
    /// amount needed to keep the cursor inside that width, the way a
    /// native single-line text input does.
    pub fn diagnostics_destination_cursor_moved(&mut self, cursor_x: f32, viewport_width: f32) {
        self.state.diagnostics.destination_scroll_x = clamp_field_scroll(
            cursor_x,
            self.state.diagnostics.destination_scroll_x,
            viewport_width,
        );
    }

    /// The destination field's current horizontal scroll offset -- cheap to
    /// read after [`Self::diagnostics_destination_cursor_moved`].
    pub fn diagnostics_destination_scroll_offset(&self) -> f32 {
        self.state.diagnostics.destination_scroll_x
    }

    /// Builds the diagnostics panel's whole view from the last completed
    /// scan and the destination state above -- a pure read that touches no
    /// directory, so it costs nothing on the main thread. Yields no rows and
    /// the loading note while no scan has reported for the current
    /// generation. Also refreshes the cached content height the scrollbar
    /// methods read, re-clamping the scroll offset against it.
    pub fn diagnostics_view(&mut self) -> DiagnosticsView {
        let current_generation = self.state.diagnostics.scan_generation;
        let loading = !matches!(
            &self.state.diagnostics.scan,
            Some((generation, _)) if *generation == current_generation
        );

        let (entries, note): (Vec<DiagnosticsEntryView>, Option<String>) = if loading {
            (Vec::new(), Some("Collecting…".to_string()))
        } else {
            let scan = &self
                .state
                .diagnostics
                .scan
                .as_ref()
                .expect("loading is false only when a scan is stored")
                .1;
            let note = (!scan.has_logs && !scan.has_crash_evidence)
                .then(|| "No logs or crash evidence were found.".to_string());
            let entries = scan
                .entries
                .iter()
                .map(|entry| {
                    let not_redacted = !entry.redacted;
                    let value = if not_redacted {
                        format!("{} · not redacted", size_text(entry.bytes))
                    } else {
                        size_text(entry.bytes)
                    };
                    DiagnosticsEntryView {
                        name: entry.name.clone(),
                        value,
                        not_redacted,
                    }
                })
                .collect();
            (entries, note)
        };

        let (primary_label, message) = match self.state.diagnostics.status {
            DiagnosticsStatus::Idle => ("Create bundle", String::new()),
            DiagnosticsStatus::ExistingFile => (
                "Overwrite and create",
                "This file already exists. Creating the bundle will overwrite it.".to_string(),
            ),
            DiagnosticsStatus::Writing => ("Writing…", String::new()),
            DiagnosticsStatus::Written => {
                ("Create bundle", "Bundle created successfully.".to_string())
            }
            DiagnosticsStatus::Failed => (
                "Create bundle",
                "Could not write the bundle. Nothing was written.".to_string(),
            ),
        };

        let content_height = manifest_content_height(entries.len(), note.is_some());
        self.state.diagnostics.manifest_content_height = content_height;
        self.set_manifest_scroll(self.state.diagnostics.manifest_scroll_y);

        DiagnosticsView {
            entries,
            note,
            include_minidumps: self.state.diagnostics.include_minidumps,
            destination: self.state.diagnostics.destination.display().to_string(),
            status: self.state.diagnostics.status,
            message,
            primary_label: primary_label.to_string(),
            busy: self.state.diagnostics.status == DiagnosticsStatus::Writing,
            loading,
            manifest_scroll_offset: self.state.diagnostics.manifest_scroll_y,
            manifest_content_height: content_height,
            destination_scroll_offset: self.state.diagnostics.destination_scroll_x,
        }
    }
}

/// `ShellGeometry.tab-pitch` (`ui/geometry.slint`): one tab plus its gap.
pub const TAB_PITCH: f32 = 228.0;
/// `ShellGeometry.tab-width` (`ui/geometry.slint`).
pub const TAB_WIDTH: f32 = 224.0;
/// `ShellGeometry.strip-box-width` (`ui/geometry.slint`): each scroll arrow
/// and "+".
pub const STRIP_BOX_WIDTH: f32 = 30.0;
/// `ShellGeometry.strip-box-gap` (`ui/geometry.slint`, `Theme.space-1`).
pub const STRIP_BOX_GAP: f32 = 4.0;

/// Whether `tab_count` tabs plus the trailing "+" outrun a tabs-region
/// `region_width` wide. The overflow layout reserves more room than this
/// (arrows beside the viewport), so the decision is taken against the
/// no-arrow layout alone and cannot flicker at the threshold.
pub fn tabs_overflow(tab_count: usize, region_width: f32) -> bool {
    tab_count as f32 * TAB_PITCH + STRIP_BOX_WIDTH > region_width
}

/// The scrolling viewport's width inside a tabs-region `region_width` wide:
/// the whole region without overflow; on overflow, the region less the left
/// arrow, the right arrow and the pinned "+", each with its box gap
/// (`ui/shell/tab_strip.slint`'s `tabs-viewport`).
pub fn tab_viewport_width(tab_count: usize, region_width: f32) -> f32 {
    if tabs_overflow(tab_count, region_width) {
        (region_width - 3.0 * (STRIP_BOX_WIDTH + STRIP_BOX_GAP)).max(0.0)
    } else {
        region_width
    }
}

/// The scrolling content's width: the tabs alone, first tab's left edge to
/// last tab's right edge -- "+" is pinned outside the viewport on overflow.
pub fn tab_content_width(tab_count: usize) -> f32 {
    (tab_count as f32 * TAB_PITCH - STRIP_BOX_GAP).max(0.0)
}

/// The largest offset that still fills the viewport: `content - viewport`,
/// or 0 when the content fits.
pub fn max_tab_scroll(content_width: f32, viewport_width: f32) -> f32 {
    (content_width - viewport_width).max(0.0)
}

/// `offset` clamped to `0 ..= max_tab_scroll(content, viewport)`.
pub fn clamp_tab_scroll(offset: f32, content_width: f32, viewport_width: f32) -> f32 {
    offset.clamp(0.0, max_tab_scroll(content_width, viewport_width))
}

/// The smallest change to `offset` that shows tab `index` whole in a
/// viewport `viewport_width` wide. Returns a clamped offset.
pub fn tab_scroll_into_view(
    offset: f32,
    index: usize,
    tab_count: usize,
    viewport_width: f32,
) -> f32 {
    let max = max_tab_scroll(tab_content_width(tab_count), viewport_width);
    let left = index as f32 * TAB_PITCH;
    let right = left + TAB_WIDTH;
    let mut s = offset.clamp(0.0, max);
    if right > s + viewport_width {
        s = right - viewport_width;
    }
    if left < s {
        s = left;
    }
    s.clamp(0.0, max)
}

/// The `ParamValues` map `section` mutates on this tab.
fn tab_values_mut<'a>(tab: &'a mut ProjectTab, section: &str) -> &'a mut ParamValues {
    match section {
        "align" => &mut tab.align_values,
        "crop" => &mut tab.project_values,
        _ => &mut tab.fuse_values,
    }
}

/// `"frame"` for exactly one, `"frames"` otherwise -- every card meta line's
/// own pluralization.
fn frame_word(count: usize) -> &'static str {
    if count == 1 { "frame" } else { "frames" }
}

/// `"1 checkpoint"` / `"N checkpoints"` -- the open card's meta line and the
/// checkpoints panel share this wording for a non-zero count.
fn checkpoint_phrase(count: usize) -> String {
    let word = if count == 1 {
        "checkpoint"
    } else {
        "checkpoints"
    };
    format!("{count} {word}")
}

/// Removes each held checkpoint's own directory, and the tab's directory
/// above it once that is empty.
fn remove_held(held: &[job::HeldCheckpoint]) {
    for checkpoint in held {
        if let Some(dir) = checkpoint.composite.parent() {
            let _ = std::fs::remove_dir_all(dir);
            if let Some(tab_dir) = dir.parent() {
                // Fails, and so keeps the directory, while anything is left.
                let _ = std::fs::remove_dir(tab_dir);
            }
        }
    }
}

/// A path's own identity for a project or a card: an `Unsaved` tab's own
/// source folder, a sealed one's `.ffp` file -- what a later successful open
/// of the same path clears a failure card by.
fn project_identity_path(project: &GuiProject) -> PathBuf {
    match project {
        GuiProject::Unsaved(group) => group.dir.clone(),
        GuiProject::Opened(project) => project.path().to_path_buf(),
    }
}

/// The folder a `Recent` entry names for `project`: the source folder for
/// an `Unsaved` group, the sealed `.ffp`'s own parent directory once opened
/// -- a folder either way, never a file path.
fn recent_folder(project: &GuiProject) -> PathBuf {
    match project {
        GuiProject::Unsaved(group) => group.dir.clone(),
        GuiProject::Opened(project) => project
            .path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| project.path().to_path_buf()),
    }
}

/// The `Not a project` card for `path`: a skipped subfolder, or an
/// `Unreadable`/`NoReadableFrames` refusal.
fn not_a_project_card(path: PathBuf) -> FailureCard {
    let title = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    FailureCard {
        kind: FailureKind::NotAProject,
        path,
        title,
        stage: "LEFT ALONE".to_string(),
        reason: "This folder holds something else. Nothing was read and nothing was changed."
            .to_string(),
        meta: "not read · not changed".to_string(),
    }
}

/// An open card's second meta line: `phase_line` (already rendered, e.g.
/// `"aligning, 3 of 8"` or `"fusing"`) while a run is live against this tab,
/// otherwise the checkpoint count -- `"no checkpoint yet"` at zero.
fn open_card_meta(frame_count: u32, checkpoints: usize, phase_line: Option<&str>) -> String {
    let frames = frame_word(frame_count as usize);
    if let Some(phase) = phase_line {
        return format!("{frame_count} {frames} · {phase}");
    }
    if checkpoints == 0 {
        return format!("{frame_count} {frames} · no checkpoint yet");
    }
    format!(
        "{frame_count} {frames} · {}",
        checkpoint_phrase(checkpoints)
    )
}

/// The skipped-subfolder notice's text, `None` when nothing was skipped --
/// not an error, its toast's own copy (`ui/shell/dialogs.slint`'s
/// `FFToast`, `tone: "info"`). Short enough to stay on the toast's one
/// line regardless of how many subfolders it names.
fn notice_line(skipped: &[PathBuf]) -> Option<String> {
    if skipped.is_empty() {
        return None;
    }
    Some(format!(
        "{} subfolder{} skipped",
        skipped.len(),
        if skipped.len() == 1 { "" } else { "s" }
    ))
}
