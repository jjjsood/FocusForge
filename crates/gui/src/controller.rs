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

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::brush;
use crate::canvas::{FrameView, OverlayDraw};
use crate::checkpoints;
use crate::diagnostics::{
    DiagnosticsEntryView, DiagnosticsScan, DiagnosticsScanEntry, DiagnosticsStatus,
    DiagnosticsView, clamp_manifest_scroll, manifest_content_height, measurement_scope, size_text,
};
use crate::entry;
use crate::export_settings::{self, ExportChoice, ExportView};
use crate::images;
use crate::input_config::Provenance;
use crate::input_map::{
    Action, ActionEvent, Button, InputRowView, InputTable, Modifiers, PointerKind, Trigger,
};
use crate::job::{self, ExportFormat, ExportOptions, params_for};
use crate::overlay::{self, CardView, FailureCard, FailureKind};
use crate::recent;
use crate::settings_panel::{
    self, PanelModel, ParamValue, ParamValues, ParamViolation, StackContext, slider_value,
};
use crate::viewport::{ImageSize, PaneDraw, PaneRect, Viewport};

pub use crate::entry::{
    EntryError, GroupingQuestion, GuiProject, ImportRefused, LocateTarget, OpenAs, OpenOutcome,
    Opened, PendingOpen, Proposal,
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
    /// A saved project's source content no longer matches the fingerprint
    /// it was recorded with -- the mismatch dialog, raised once.
    MismatchRaised,
    /// The settings panel's own state moved: a value, an algorithm choice,
    /// a reset or an open menu -- never a tab switch, which
    /// `ProjectsChanged` already rebuilds the panel for.
    SettingsChanged,
    /// Only the settings panel's scroll offset or viewport height moved; no
    /// row changed.
    SettingsScrolled,
    /// An open attempt was refused. Carries its already-rendered message:
    /// `perform` (`crate::bridge`) must not format user-facing text.
    EntryRefused(String),
    /// A fuse run's state or progress moved -- started, paused, resumed,
    /// cancelled, failed or completed.
    JobChanged,
    /// The `Images` panel's own state moved: a row's exclusion, the frame
    /// order, the sort selection, or a frame-set-change warning opened or
    /// closed -- never a tab switch, which `ProjectsChanged` already
    /// rebuilds the panel for.
    ImagesChanged,
    /// Only the `Images` list's scroll offset moved; no row changed.
    ImagesScrolled,
    /// The `Checkpoints` panel's own state moved: a row selected or the
    /// scroll offset -- never a tab switch, which `ProjectsChanged` already
    /// rebuilds the panel for.
    CheckpointsChanged,
    /// The editor toolbar's own state moved: the brush size, its softness,
    /// the pipette's armed state, or undo/redo availability.
    EditorChanged,
    /// The projects overlay's own state moved: opened, closed, its gallery
    /// scrolled, or its failure cards changed.
    OverlayChanged,
    /// Only the gallery's scroll offset or viewport height moved; no card
    /// changed.
    GalleryScrolled,
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
    /// A `Locate frames…` verification ([`AppController::locate_frames`]) is
    /// ready to settle ([`AppController::settle_locate`]).
    LocateReady,
    /// A save's worker has parked its outcome
    /// ([`AppController::settle_save`]).
    SaveReady,
    /// A recomposite worker has parked its gathered regions
    /// ([`AppController::settle_recomposites`]).
    RecompositeReady,
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
    /// Whether the active tab's session holds ops: what `Save checkpoint` keeps.
    pub can_save_checkpoint: bool,
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
    /// Every checkpoint kept in memory since the last save, oldest first --
    /// fused and op-bearing ones alike, in no `.ffp` until
    /// [`AppController::save_active_project`] writes them. A parent is an
    /// index into this list or a saved checkpoint's id.
    held: Vec<job::InMemoryCheckpoint>,
    /// Rows (saved or in memory) whose deletion waits for the next project
    /// save; the list already leaves them out.
    pending_deletes: Vec<job::ParentRef>,
    /// Saved checkpoint ids whose planes sit in the plane cache, newest last.
    viewed: Vec<String>,
    /// The checkpoint list's own current row, by its `checkpoints::
    /// CheckpointEntry` id -- seeded to the newest saved checkpoint on open,
    /// `None` for an `Unsaved` tab with nothing fused yet.
    selected_checkpoint: Option<String>,
    /// The row a checkpoint creation just added while this tab was in view,
    /// by list id, with the creation's generation. A save rewrites the id
    /// and keeps the generation.
    fresh_checkpoint: Option<(String, u64)>,
    /// The current saved row's planes once asked for; reset whenever the
    /// current row changes. Held rows never use it: they carry their own.
    row_planes: Option<RowPlanes>,
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
    /// Existence and capture time per source path, read when the path joins
    /// the tab and refreshed only by an explicit relink or a refused use.
    meta: HashMap<PathBuf, images::FrameMeta>,
    /// This tab's own card picture, decoded off the UI thread: by the load
    /// worker for an opened project, by [`AppController::request_thumbnail`]
    /// for a new tab and each fused composite. `Arc`-shared rather than
    /// cloned per push, since [`AppController::project_cards`] rebuilds far
    /// more often than a picture actually changes.
    thumbnail: Option<Arc<overlay::Thumbnail>>,
    /// The newest decode requested for [`Self::thumbnail`]; a result for an
    /// earlier one is dropped.
    thumbnail_generation: u64,
    /// This tab's own run: `Idle` until a fuse starts on it, and whatever it
    /// last settled to afterward -- read by every bottom-bar and card
    /// surface for the tab that owns it, regardless of which tab is active.
    run: job::RunState,
    /// The request behind this tab's own last run, kept for `Retry` --
    /// `None` until a run is started on this tab.
    request: Option<job::JobRequest>,
    /// This tab's own editor session, opened on a worker over the selected
    /// row's Z-Map and stroke chain when editor mode is entered or an
    /// op-bearing row is selected -- `None` until it lands, and cleared
    /// whenever the selected row or planes are replaced (a restore, a new
    /// checkpoint).
    edit: Option<job::EditState>,
    /// Bumped whenever the session this tab holds, or is opening, is dropped:
    /// an opening session lands only while its generation is still this one.
    session_generation: u64,
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

/// The missing-sources dialog's own payload: which tab it names, its
/// counts ([`overlay::sources_missing_body`]) and the project's file name
/// for its context line.
struct SourcesNotice {
    tab_id: u64,
    missing: usize,
    total: usize,
    file_name: String,
}

/// A saved row's planes, as far as they have got.
enum RowPlanes {
    /// A saved row whose planes are not cached yet; the canvas keeps what
    /// it showed until they are.
    Pending,
    Ready(PathBuf, PathBuf),
    /// The right pane's refusal text, one entry per line.
    Refused(Vec<String>),
}

/// What the active tab can show in the right pane right now.
enum TabPlanes {
    Ready(PathBuf, PathBuf),
    Pending,
    None,
}

/// The one checkpoint render in flight, op-bearing or fused, if any, and
/// where its worker parks the result -- [`LoadSlot`]'s shape.
#[derive(Default)]
struct RenderSlot {
    /// The tab (by id) and row the running worker renders.
    in_flight: Option<(u64, String)>,
    /// Set when a worker starts, until [`AppController::take_render_started`].
    started: bool,
    outcome: Arc<Mutex<Option<RenderOutcome>>>,
}

type RenderOutcome = Result<(PathBuf, PathBuf), Vec<String>>;

/// The card-picture decodes in flight and where their workers park the
/// results: tab id, the generation the decode was requested for, and the
/// picture (`None` for a failed decode).
#[derive(Default)]
struct ThumbnailSlot {
    in_flight: usize,
    /// Set when a worker starts, until [`AppController::take_thumbnails_started`].
    started: bool,
    parked: Arc<Mutex<Vec<ParkedPicture>>>,
}

type ParkedPicture = (u64, u64, Option<overlay::Thumbnail>);

/// The editor sessions being opened and where their workers park the
/// results: tab id, the session generation each was started for, and the
/// opened session or why it failed. Two tabs may open at once.
#[derive(Default)]
struct EditSlot {
    in_flight: Vec<(u64, u64)>,
    /// Set when a worker starts, until [`AppController::take_session_started`].
    started: bool,
    parked: Arc<Mutex<Vec<ParkedSession>>>,
}

type ParkedSession = (u64, u64, Result<job::EditState, String>);

impl EditSlot {
    fn opening(&self, tab_id: u64, generation: u64) -> bool {
        self.in_flight.contains(&(tab_id, generation))
    }
}

/// Whether the active tab has an editor session, is opening one, or cannot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionOpen {
    Open,
    Opening,
    Unavailable,
}

/// The export modal's own state: which tab it names, the context/what
/// lines already composed for its row (per-kind prose, not a separate
/// "kind" label -- the kind is named through this prose, never through its
/// own badge), and how far the write itself has got.
struct ExportDialog {
    tab_id: u64,
    context: String,
    what: String,
    stage: ExportStage,
    /// The chosen destination's own file name -- set once the picker
    /// returns a path, empty before then; the writing state's caption
    /// names it.
    destination_name: String,
    /// The chosen destination's full path, set alongside
    /// `destination_name` -- kept so a refused export can be retried at the
    /// same path once `Locate frames…` resolves its sources, without
    /// prompting the save dialog again.
    destination: Option<PathBuf>,
    /// The format and settings the modal shows; copied into
    /// `AppState::export_choice` and saved only once an export succeeds.
    choice: ExportChoice,
    /// The descriptor id whose list is open.
    open_menu: Option<String>,
}

/// Which position an export modal names -- the three kinds its own
/// context/what copy tells apart.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExportKind {
    Saved,
    Held,
    Live,
}

/// The export modal's own context/what copy for `kind`: a saved or live
/// row names its algorithm and local time, a held row does not -- it has
/// neither yet, only its own "just fused" state.
fn export_copy_for(
    kind: ExportKind,
    project_file: &str,
    algorithm: &str,
    timestamp: &str,
) -> (String, String) {
    match kind {
        ExportKind::Saved => (
            format!("{project_file} · {algorithm} · {timestamp}"),
            "Exporting the saved checkpoint you selected.".to_string(),
        ),
        ExportKind::Held => (
            format!("{project_file} · unsaved fuse result, held in this tab"),
            "Exporting the unsaved fuse result held in this tab.".to_string(),
        ),
        ExportKind::Live => (
            format!("{project_file} · unsaved edits on {algorithm} · {timestamp}"),
            "Exporting your current unsaved retouch edits.".to_string(),
        ),
    }
}

enum ExportStage {
    Ready,
    Writing,
    Refused { lines: Vec<String>, locate: bool },
}

/// An export write's own outcome, as [`ExportSlot`] parks it: written, or
/// the modal's own refusal lines.
type ExportOutcome = Result<(), job::ExportRefusal>;

/// The one export write in flight, if any, and where its worker parks the
/// result -- [`RenderSlot`]'s twin for [`job::export_position`].
#[derive(Default)]
struct ExportSlot {
    in_flight: Option<u64>,
    started: bool,
    outcome: Arc<Mutex<Option<ExportOutcome>>>,
}

/// What an in-memory row's planes and fuse state come from.
enum RowBase {
    /// The fused in-memory entry at this index.
    Held(usize),
    Saved(String),
}

/// The finished fuse `entry` holds, if it is one.
fn fused_of(entry: &job::InMemoryCheckpoint) -> Option<&job::HeldCheckpoint> {
    match &entry.kind {
        job::InMemoryKind::Fused(held) => Some(held),
        _ => None,
    }
}

/// A held checkpoint whose run diverged on `diverged`, by frame file name.
struct ParkedRun {
    held: job::HeldCheckpoint,
    diverged: Vec<String>,
}

impl ProjectTab {
    /// `thumbnail` is the picture the load worker already decoded, if any.
    /// `meta` is the frames' metadata when already read; `None` reads it here.
    fn new(
        project: GuiProject,
        id: u64,
        thumbnail: Option<Arc<overlay::Thumbnail>>,
        meta: Option<HashMap<PathBuf, images::FrameMeta>>,
    ) -> Self {
        let title = entry::tab_title(&project);
        let ctx = entry::tab_stack_context(&project);
        let image =
            entry::tab_image_size(&project).map(|(width, height)| ImageSize { width, height });
        let (fuse_id, fuse_values, align_id, align_values) =
            settings_panel::seed_algorithm_state(&project);
        let selected_checkpoint = match &project {
            GuiProject::Unsaved(_) => None,
            GuiProject::Opened(p) => p.history().visible().first().map(|c| c.id.clone()),
        };
        let frames = entry::project_frames(&project);
        let excluded = vec![false; frames.len()];
        let selected = images::first_included(&excluded);
        let meta = meta.unwrap_or_else(|| images::read_metadata(&frames));
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
            held: Vec::new(),
            pending_deletes: Vec::new(),
            viewed: Vec::new(),
            selected_checkpoint,
            fresh_checkpoint: None,
            row_planes: None,
            parked: None,
            frames,
            excluded,
            selected,
            frame_sort: None,
            meta,
            thumbnail,
            thumbnail_generation: 0,
            run: job::RunState::Idle,
            request: None,
            edit: None,
            session_generation: 0,
            failure: None,
        }
    }

    /// What closing this tab would lose besides a modified live: the
    /// in-memory checkpoints the list still shows, and the queued deletes of
    /// saved checkpoints.
    fn unsaved_parts(&self) -> (usize, usize) {
        let listed = (0..self.held.len())
            .filter(|&index| !self.pending_deletes.contains(&job::ParentRef::Entry(index)))
            .count();
        let deletes = self
            .pending_deletes
            .iter()
            .filter(|parent| matches!(parent, job::ParentRef::Saved(_)))
            .count();
        (listed, deletes)
    }

    /// Whether closing this tab loses something the user can see: a modified
    /// live, a listed in-memory checkpoint or a queued delete of a saved one.
    fn loses_on_close(&self) -> bool {
        let (listed, deletes) = self.unsaved_parts();
        self.live_modified() || listed > 0 || deletes > 0
    }

    /// Whether this tab holds work no `.ffp` contains: a never-saved
    /// project, or anything [`Self::loses_on_close`] names.
    fn unsaved(&self) -> bool {
        matches!(self.project, GuiProject::Unsaved(_)) || self.loses_on_close()
    }

    /// Whether quitting now would lose work of this tab: what
    /// [`Self::loses_on_close`] names, or a parked run still waiting for Keep
    /// or Discard.
    fn quit_loses_work(&self) -> bool {
        self.loses_on_close() || self.parked.is_some()
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

    /// This tab's own checkpoint count: every visible checkpoint in the
    /// project's history (`History::visible`, branch-complete rather than one
    /// lineage) plus every checkpoint fused since the last save. `0` for an
    /// `Unsaved` tab with nothing fused yet.
    fn checkpoint_count(&self) -> usize {
        let saved = match &self.project {
            GuiProject::Opened(project) => project.history().visible().len(),
            GuiProject::Unsaved(_) => 0,
        };
        saved + self.held.len()
    }

    /// The number of rows the checkpoint list shows: visible saved
    /// checkpoints less the queued deletes of saved ones, plus the listed
    /// in-memory ones.
    fn listed_checkpoint_count(&self) -> usize {
        let saved = match &self.project {
            GuiProject::Opened(project) => project.history().visible().len(),
            GuiProject::Unsaved(_) => 0,
        };
        let (listed, deletes) = self.unsaved_parts();
        saved.saturating_sub(deletes) + listed
    }

    /// Whether this tab can enter editor mode: at least one checkpoint, its
    /// own run neither live nor waiting in the queue, and `frames` still
    /// naming the frame set the fused planes were built from -- the newest
    /// held checkpoint's own `sources`, or a reopened project's own manifest
    /// when nothing is held.
    fn can_edit(&self) -> bool {
        if self.checkpoint_count() == 0 {
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

    /// Reads the metadata of every frame `meta` does not hold yet.
    fn read_missing_meta(&mut self) {
        let missing: Vec<PathBuf> = self
            .frames
            .iter()
            .filter(|path| !self.meta.contains_key(*path))
            .cloned()
            .collect();
        self.meta.extend(images::read_metadata(&missing));
    }

    /// The sources and registration behind this tab's *selected* row --
    /// a held row's own `sources`/`registration`, or a saved checkpoint's
    /// own fuse state (`Project::fuse_state`), never the project head's
    /// when a different row is selected. `None` for an unsaved, never-fused
    /// tab, or when the selected row does not resolve.
    fn fused(&self) -> Option<(Vec<PathBuf>, job::Registration)> {
        let key = self.selected_checkpoint.as_deref()?;
        if checkpoints::held_index(key).is_none() {
            return self.saved_fused(key);
        }
        match self.resolve_row(key)? {
            RowBase::Held(base) => {
                let held = fused_of(self.held.get(base)?)?;
                Some((held.sources.clone(), held.registration.clone()))
            }
            RowBase::Saved(id) => self.saved_fused(&id),
        }
    }

    /// [`Self::fused`] for saved checkpoint `id`: its fuse state's sources,
    /// resolved against the project's own location, and registration.
    fn saved_fused(&self, id: &str) -> Option<(Vec<PathBuf>, job::Registration)> {
        let GuiProject::Opened(project) = &self.project else {
            return None;
        };
        let record = project.fuse_state(id).ok()?;
        let sources = record
            .sources
            .iter()
            .map(|source| project.source_path(source))
            .collect();
        Some((sources, record.registration))
    }

    /// The nearest fused in-memory entry or saved checkpoint row `key`
    /// descends from, following parents; `None` when the chain does not
    /// resolve.
    fn resolve_row(&self, key: &str) -> Option<RowBase> {
        let Some(mut index) = checkpoints::held_index(key) else {
            return Some(RowBase::Saved(key.to_string()));
        };
        loop {
            let entry = self.held.get(index)?;
            if fused_of(entry).is_some() {
                return Some(RowBase::Held(index));
            }
            match entry.parent.as_ref()? {
                job::ParentRef::Entry(parent) => index = *parent,
                job::ParentRef::Saved(id) => return Some(RowBase::Saved(id.clone())),
            }
        }
    }

    /// The id list row `row` carries once a save wrote this tab's held
    /// entries as `ids`: a held row's saved id, a saved row's own. `None`
    /// for a held row the save did not write.
    fn saved_row_key(
        &self,
        row: &str,
        ids: &[String],
        keep: Option<&job::ParentRef>,
    ) -> Option<String> {
        let Some(entry) = checkpoints::held_index(row) else {
            return Some(row.to_string());
        };
        // `ids` holds one id per written entry.
        let kept = job::kept_entries(&self.held, &self.pending_deletes, keep);
        ids.get(kept[..entry].iter().filter(|written| **written).count())
            .cloned()
    }

    /// Where a checkpoint taken from the current row is recorded: under the
    /// row itself, a saved checkpoint or an in-memory entry. `None` with no
    /// current row.
    fn row_parent(&self) -> Option<job::ParentRef> {
        let key = self.selected_checkpoint.as_deref()?;
        Some(match checkpoints::held_index(key) {
            Some(index) => job::ParentRef::Entry(index),
            None => job::ParentRef::Saved(key.to_string()),
        })
    }

    /// The key the selected row's planes are rendered and cached under: an
    /// op-bearing in-memory row shows its nearest saved ancestor's planes,
    /// every other row its own.
    fn plane_key(&self) -> Option<String> {
        let key = self.selected_checkpoint.as_deref()?;
        let on_entry = checkpoints::held_index(key)
            .and_then(|index| self.held.get(index))
            .is_some_and(|entry| fused_of(entry).is_none());
        match self.resolve_row(key) {
            Some(RowBase::Saved(id)) if on_entry => Some(id),
            _ => Some(key.to_string()),
        }
    }

    /// Drops the installed session, if any, and starts a new session
    /// generation so one still opening for the old state is dropped when it
    /// lands. Returns whether an installed session was dropped.
    fn drop_session(&mut self) -> bool {
        self.session_generation += 1;
        self.edit.take().is_some()
    }

    /// Whether the selected row is an in-memory entry holding ops.
    fn on_ops_row(&self) -> bool {
        self.selected_checkpoint
            .as_deref()
            .and_then(checkpoints::held_index)
            .and_then(|index| self.held.get(index))
            .is_some_and(|entry| matches!(entry.kind, job::InMemoryKind::Ops { .. }))
    }

    /// Whether the edit session holds ops its origin does not.
    fn live_modified(&self) -> bool {
        self.edit
            .as_ref()
            .is_some_and(|edit| edit.session.op_count() > 0)
    }

    /// Removes the files of every fused in-memory entry a project save would
    /// not write: deleted, not the current row and no written entry's parent.
    fn free_unneeded_held(&self) {
        let kept = job::kept_entries(
            &self.held,
            &self.pending_deletes,
            self.row_parent().as_ref(),
        );
        for (entry, kept) in self.held.iter().zip(kept) {
            if let (false, job::InMemoryKind::Fused(held)) = (kept, &entry.kind) {
                remove_hold_dir_of(&held.composite);
            }
        }
    }

    /// Plane-cache directories kept per tab: the current row's and the last
    /// three viewed.
    const VIEWED_PLANES: usize = 4;

    /// Records that saved checkpoint `id`'s planes sit in the plane cache and
    /// removes the oldest viewed ones past [`Self::VIEWED_PLANES`].
    fn note_viewed(&mut self, scratch_root: &Path, id: &str) {
        self.viewed.retain(|viewed| viewed != id);
        self.viewed.push(id.to_string());
        while self.viewed.len() > Self::VIEWED_PLANES {
            let oldest = self.viewed.remove(0);
            self.remove_cached_planes(scratch_root, &oldest);
        }
    }

    /// Removes saved checkpoint `id`'s cached planes unless they are the
    /// current row's.
    fn remove_cached_planes(&mut self, scratch_root: &Path, id: &str) {
        if self.plane_key().as_deref() == Some(id) {
            return;
        }
        self.viewed.retain(|viewed| viewed != id);
        if let GuiProject::Opened(project) = &self.project {
            let _ = std::fs::remove_dir_all(job::checkpoint_plane_dir(
                scratch_root,
                project.path(),
                id,
            ));
        }
    }

    /// Whether a project save has anything to write: an in-memory
    /// checkpoint, a pending delete or a modified live.
    fn has_unsaved_write(&self) -> bool {
        !self.held.is_empty() || !self.pending_deletes.is_empty() || self.live_modified()
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

/// The press a fuse question interrupted -- what its answers start again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FuseEntry {
    Fuse,
    Retry,
    RunAgain,
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
    checkpoints_scroll_y: f32,
    checkpoints_viewport_height: f32,
    checkpoints_scrollbar_drag: Option<f32>,
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
    /// A fingerprint mismatch raised by [`AppController::open_paths`],
    /// blocking until [`AppController::open_anyway`] or
    /// [`AppController::cancel_mismatch`] -- no tab exists for it while
    /// this is `Some`.
    pending_mismatch: Option<PendingOpen>,
    /// The skipped-subfolder notice from the most recent open, if any.
    notice: Option<String>,
    /// The most recent refusal's already-rendered message, if any.
    last_refusal: Option<String>,
    /// The most recent refusal's own title, `Some` only for a project or
    /// locate refusal -- `None` keeps the folder-empty dialog's fixed
    /// "No frames here" title and `Pick another folder…` action.
    refusal_title: Option<String>,
    /// Whether the "No frames here" refusal dialog is open -- raised by
    /// [`AppController::refuse_message`], cleared by
    /// [`AppController::dismiss_refusal`].
    refusal_open: bool,
    /// A missing-sources notice: which tab it's about, its counted sentence
    /// and file name -- set by [`AppController::accept`] when an opened
    /// project's sources are missing and no mismatch was just accepted.
    /// Cleared by [`AppController::close_sources_notice`].
    sources_notice: Option<SourcesNotice>,
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
    /// raised by [`AppController::request_close`] only when closing that tab
    /// loses something ([`ProjectTab::loses_on_close`]).
    pending_close: Option<u64>,
    /// Whether a window quit waits on the "quit without saving?" answer --
    /// raised by [`AppController::window_close`] only when some tab would
    /// lose work ([`ProjectTab::quit_loses_work`]). Mutually exclusive with
    /// `pending_close`: each is a no-op while the other question is open.
    pending_quit: bool,
    /// The tab, by id, and the press a fuse waits on while the "save your
    /// edits first?" answer is open -- raised only when that tab's live holds
    /// ops.
    pending_fuse: Option<(u64, FuseEntry)>,
    /// The tab, by id, and the row key a click on a checkpoint waits to
    /// restore while the "save this state first?" answer is open -- raised
    /// only when that tab's live holds ops.
    pending_restore: Option<(u64, String)>,
    /// The tab, by id, and the row key whose delete waits on the quiet
    /// "Delete this checkpoint?" answer.
    pending_checkpoint_delete: Option<(u64, String)>,
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
    /// The directory every table's save writes `settings.toml` into,
    /// installed once at startup for every table alongside `[recent]`.
    config_dir: Option<PathBuf>,
    /// The active tab's planes changed underneath the open renderer cell --
    /// a background run settling on it or a close renumbering the tabs --
    /// so the next [`AppController::canvas_feed`] reloads even though the
    /// owner index still compares equal.
    canvas_dirty: bool,
    /// The in-flight background load, if any.
    load: LoadSlot,
    render: RenderSlot,
    thumbnails: ThumbnailSlot,
    edits: EditSlot,
    /// The open export modal, if any -- [`AppController::checkpoint_export_requested`].
    export_dialog: Option<ExportDialog>,
    /// The last confirmed export format and settings, which the next modal
    /// opens on.
    export_choice: ExportChoice,
    /// The in-flight export write, if any.
    export: ExportSlot,
    /// The in-flight additive import scan, if any.
    import: ImportSlot,
    /// The in-flight `Locate frames…` verification, if any.
    locate: LocateSlot,
    /// The running and queued saves, and what follows the last one.
    save: SaveSlot,
    /// The `Composite` pane's rainbow overlay -- window-wide (see
    /// [`OverlayState`]).
    overlay: OverlayState,
}

/// The one save worker allowed at a time, the saves waiting behind it and
/// the step that follows the last of them.
#[derive(Default)]
struct SaveSlot {
    running: Option<RunningSave>,
    /// Tabs (by id) to save next, each with its target, in order.
    queue: VecDeque<(u64, PathBuf)>,
    then: AfterSave,
    /// The running worker's result, parked until
    /// [`AppController::settle_save`] applies it.
    outcome: Arc<Mutex<Option<SaveResult>>>,
}

/// A save worker's result: the written project, opened, with the id written
/// per kept entry -- or why it could not be written.
type SaveResult = Result<job::SavedProject, String>;

/// A save on its worker: the tab it belongs to, where it writes and the
/// current row the write keeps stored.
struct RunningSave {
    tab_id: u64,
    target: PathBuf,
    keep: Option<job::ParentRef>,
    worker: Option<std::thread::JoinHandle<()>>,
}

/// What happens once every queued save has written.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum AfterSave {
    #[default]
    Nothing,
    /// Close the tab with this id.
    CloseTab(u64),
    Quit,
}

/// How [`AppController::begin_save`] ended.
enum SaveStart {
    Started,
    /// The tab is gone or has nothing to write.
    Skipped,
    /// The tab's unsaved edits could not be kept; the failure is reported.
    Failed,
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
    thumbnails: Vec<Preloaded>,
}

/// One project's thumbnail and frame metadata, read on a load worker.
type Preloaded = (
    Option<Arc<overlay::Thumbnail>>,
    HashMap<PathBuf, images::FrameMeta>,
);

/// Decodes one thumbnail and reads the frame metadata per project, in
/// order -- what [`AppController::open_paths`] and
/// [`AppController::answer_question`] both run on their own worker thread.
fn decode_thumbnails(projects: &mut [GuiProject]) -> Vec<Preloaded> {
    projects
        .iter_mut()
        .map(|project| {
            let frames = entry::project_frames(project);
            let thumbnail = overlay::render_thumbnail(project, &frames).map(Arc::new);
            (thumbnail, images::read_metadata(&frames))
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

/// One `Locate frames…` verification's own in-flight slot -- [`ImportSlot`]'s
/// twin for [`AppController::locate_frames`]: every content hash it runs
/// happens on this worker, never the event loop.
#[derive(Default)]
struct LocateSlot {
    in_flight: bool,
    outcome: Arc<Mutex<Option<LocateScan>>>,
}

/// [`AppController::locate_frames`]'s worker outcome: what it verified
/// against, the folder it verified and what `entry::verify_folder` found.
struct LocateScan {
    target: entry::LocateTarget,
    folder: PathBuf,
    result: Result<Vec<PathBuf>, entry::LocateRefused>,
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
    match (tab.held.iter().rev().find_map(fused_of), &tab.project) {
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
    /// The generation of the newest checkpoint creation on a tab in view.
    checkpoint_generation: u64,
    /// The newest generation [`Self::take_checkpoint_flash`] handed out.
    checkpoint_flash_played: u64,
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

    /// Test-only: the active tab's live editor state, to swap its frame
    /// reader.
    pub fn edit_state_mut_for_test(&mut self) -> Option<&mut job::EditState> {
        self.state.active_mut()?.edit.as_mut()
    }

    /// Installs the regions recomposite workers have parked, on the tab each
    /// was queued for -- active or not -- and dispatches what queued behind
    /// them. Returns `true` once no tab has a recomposite in flight.
    pub fn settle_recomposites(&mut self) -> bool {
        let wake = self.recomposite_wake();
        let mut idle = true;
        for tab in &mut self.state.projects {
            if let Some(edit) = tab.edit.as_mut() {
                idle &= edit.settle_recomposite(&wake);
            }
        }
        idle
    }

    /// What a recomposite worker runs once it has parked a batch: the
    /// [`ShellEvent::RecompositeReady`] on this controller's sink.
    fn recomposite_wake(&self) -> job::RecompositeWake {
        let sink = self.sink.clone();
        Arc::new(move || {
            if let Some(sink) = &sink {
                sink(ShellEvent::RecompositeReady);
            }
        })
    }

    /// Drains the active tab's log of every region installed into its
    /// resident display tiles since the last call -- a queued region is
    /// logged once [`Self::settle_recomposites`] installs it; empty with no
    /// edit session.
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
    /// [`Self::editor_enabled`] is `true` and the active tab's Z-Map has
    /// planes to open over. Entering editor mode switches at once and starts
    /// opening the session on a worker when the tab has none, as after a
    /// restore dropped it: strokes do nothing until
    /// [`Self::settle_edit_session`] installs it, and a session that fails to
    /// open drops the window back to stacking with one more `ModeChanged`.
    pub fn set_mode(&mut self, mode: ShellMode) -> bool {
        if mode == ShellMode::Editor
            && !(self.editor_enabled() && self.ensure_edit_session() != SessionOpen::Unavailable)
        {
            return false;
        }
        if mode != ShellMode::Editor {
            self.discard_live_stroke();
        }
        self.state.mode = mode;
        self.emit(ShellEvent::ModeChanged(mode));
        true
    }

    /// Opens the active tab's edit session over its row's Z-Map and its own
    /// frames on a worker unless the tab already holds or is opening one,
    /// replaying an op-bearing row's stroke chain there. `Opening` also
    /// while the row's planes still render, in which case the render's
    /// settle asks again; `Unavailable` with no active tab, no fused row or
    /// no planes to open over.
    fn ensure_edit_session(&mut self) -> SessionOpen {
        let Some(tab) = self.state.active() else {
            return SessionOpen::Unavailable;
        };
        if tab.edit.is_some() {
            return SessionOpen::Open;
        }
        if self.state.edits.opening(tab.id, tab.session_generation) {
            return SessionOpen::Opening;
        }
        let zmap = match self.active_tab_planes() {
            TabPlanes::Ready(_, zmap) => zmap,
            TabPlanes::Pending => return SessionOpen::Opening,
            TabPlanes::None => return SessionOpen::Unavailable,
        };
        let Some(tab) = self.state.active() else {
            return SessionOpen::Unavailable;
        };
        // `can_edit` guarantees `tab.frames` equals the fused sources, in
        // the same order, by the time editor mode is entered -- so indexing
        // `registration`'s per-frame transforms by position here matches.
        let Some((_, registration)) = tab.fused() else {
            return SessionOpen::Unavailable;
        };
        let frames = tab.frames.clone();
        let chain = tab
            .selected_checkpoint
            .as_deref()
            .and_then(checkpoints::held_index)
            .and_then(|index| checkpoints::stroke_chain(&tab.held, index))
            .unwrap_or_default();
        let (tab_id, generation) = (tab.id, tab.session_generation);
        let tile_size = self
            .resident_tile_size_for_test
            .unwrap_or(job::RESIDENT_TILE_SIZE);
        let slot = &mut self.state.edits;
        slot.in_flight.push((tab_id, generation));
        slot.started = true;
        let parked = Arc::clone(&slot.parked);
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(|| {
                job::open_edit_session(
                    &zmap,
                    &frames,
                    registration,
                    tile_size,
                    job::host_budget(),
                    chain,
                )
            })
            .map_err(|_| "the session worker panicked".to_string())
            .and_then(|opened| opened.map_err(|err| err.to_string()));
            parked
                .lock()
                .expect("no thread panicked while holding the parked sessions")
                .push((tab_id, generation, result));
        });
        SessionOpen::Opening
    }

    /// Installs the parked sessions without waiting on a worker. A session
    /// lands, with `EditorChanged`, only on a tab that still exists at the
    /// generation it was started for; a failed open is logged and, in the
    /// active tab in editor mode, drops the window to stacking. Returns
    /// `true` once no session is being opened -- the poll's cue to stop.
    pub fn settle_edit_session(&mut self) -> bool {
        let parked = std::mem::take(
            &mut *self
                .state
                .edits
                .parked
                .lock()
                .expect("no thread panicked while holding the parked sessions"),
        );
        for (tab_id, generation, result) in parked {
            let started = (tab_id, generation);
            self.state.edits.in_flight.retain(|open| *open != started);
            let Some(index) = self.tab_index(tab_id) else {
                continue;
            };
            let tab = &mut self.state.projects[index];
            if tab.session_generation != generation {
                continue;
            }
            self.state.canvas_dirty = true;
            match result {
                Ok(edit) => {
                    tab.edit = Some(edit);
                    self.emit(ShellEvent::EditorChanged);
                }
                Err(reason) => {
                    tracing::error!(error = %reason, "the editor session could not be opened");
                    if self.state.active == Some(index) && self.state.mode == ShellMode::Editor {
                        self.drop_to_stacking();
                    }
                }
            }
        }
        self.state.edits.in_flight.is_empty()
    }

    /// Whether an editor session started opening since the last call -- the
    /// bridge's cue to start polling [`Self::settle_edit_session`].
    pub fn take_session_started(&mut self) -> bool {
        std::mem::take(&mut self.state.edits.started)
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

    /// The active tab's own edit session, once its worker has opened and
    /// [`Self::settle_edit_session`] installed it.
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

    /// Whether the overlay can be drawn: the active tab's selected row
    /// resolves to a fuse. The bottom bar greys the controls otherwise.
    pub fn overlay_available(&self) -> bool {
        self.state.active().is_some_and(|tab| tab.fused().is_some())
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
    /// `focusforge-cli retouch --overlay --opacity`'s own blend, in either
    /// mode -- `None` with the overlay off or when the active tab's selected
    /// row has no fuse. The table spans that fuse's sources.
    pub fn overlay_draw(&self) -> Option<OverlayDraw> {
        if !self.state.overlay.on {
            return None;
        }
        let sources = self.state.active()?.fused()?.0.len();
        Some(OverlayDraw::for_opacity(
            sources as u32,
            self.state.overlay.opacity,
        ))
    }

    /// In editor mode, gives a newly active editable tab its own edit
    /// session; drops the window back to stacking, with exactly one
    /// `ModeChanged(Stacking)`, when the active tab no longer satisfies
    /// [`Self::editor_enabled`] or has no planes its session could open over
    /// -- a switch away from a fused tab, that tab's own run starting or
    /// being promoted, a confirmed frame-set change or a refused render. A
    /// tab whose current row still renders keeps editor mode; its session
    /// opens once the render settles.
    fn leave_editor_if_unfit(&mut self) {
        if self.state.mode == ShellMode::Editor
            && !(self.editor_enabled() && self.ensure_edit_session() != SessionOpen::Unavailable)
        {
            self.drop_to_stacking();
        }
    }

    /// Leaves editor mode for stacking with one `ModeChanged(Stacking)`.
    fn drop_to_stacking(&mut self) {
        self.discard_live_stroke();
        self.state.mode = ShellMode::Stacking;
        self.emit(ShellEvent::ModeChanged(ShellMode::Stacking));
    }

    /// The brush's source index: the active tab's `selected` position, or
    /// `None` with no active tab or a position past `u16::MAX`. Computed on
    /// every call from `selected`; no field holds a copy.
    pub fn brush_source(&self) -> Option<u16> {
        u16::try_from(self.state.active()?.selected).ok()
    }

    pub fn select_dock_tab(&mut self, tab: DockTab) {
        self.state.dock_tab = tab;
        self.emit(ShellEvent::DockTabSelected(tab));
    }

    /// Builds the whole `Checkpoints` panel for the active tab -- empty with
    /// no tab active.
    pub fn checkpoints_panel(&self) -> checkpoints::CheckpointsModel {
        let today = checkpoints::today();
        let viewport_height = self.state.checkpoints_viewport_height;
        let Some(tab) = self.state.active() else {
            return checkpoints::build_model(&[], &[], &[], today, None, 0.0);
        };
        let saved = match &tab.project {
            GuiProject::Opened(project) => project.history().all(),
            GuiProject::Unsaved(_) => Vec::new(),
        };
        let selected = tab.selected_checkpoint.as_deref();
        let pending = &tab.pending_deletes;
        let content_height =
            checkpoints::build_model(&saved, &tab.held, pending, today, selected, 0.0)
                .content_height;
        let scroll_y = settings_panel::clamp_panel_scroll(
            self.state.checkpoints_scroll_y,
            content_height,
            viewport_height,
        );
        let mut model =
            checkpoints::build_model(&saved, &tab.held, pending, today, selected, scroll_y);
        if let Some((key, generation)) = &tab.fresh_checkpoint {
            for entry in &mut model.entries {
                if entry.id.as_ref() == Some(key) {
                    entry.fresh = Some(*generation);
                }
            }
        }
        model
    }

    /// Restores flat entry `index` of [`Self::checkpoints_panel`]: it becomes
    /// the current row and live a copy of it. While live holds ops the restore
    /// waits on [`Self::pending_restore_copy`]'s answer, the current row
    /// included. A no-op on a separator, past the
    /// end, while a dialog is open, and on the current row when live has no
    /// ops (unless its planes were refused).
    pub fn select_checkpoint_row(&mut self, index: usize) {
        if self.modal_open() || self.active_history_frozen() {
            return;
        }
        let Some(id) = self
            .checkpoints_panel()
            .entries
            .get(index)
            .and_then(|entry| entry.id.clone())
        else {
            return;
        };
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        if checkpoints::held_index(&id).is_some_and(|index| {
            !matches!(
                tab.held.get(index).map(|entry| &entry.kind),
                Some(job::InMemoryKind::Fused(_) | job::InMemoryKind::Ops { .. })
            )
        }) {
            return;
        }
        if tab.live_modified() {
            self.state.pending_restore = Some((tab.id, id));
            self.emit(ShellEvent::ProjectsChanged);
            return;
        }
        if tab.selected_checkpoint.as_deref() == Some(id.as_str())
            && !matches!(tab.row_planes, Some(RowPlanes::Refused(_)))
        {
            return;
        }
        self.restore_row(id);
    }

    /// Makes row `key` of the active tab the current row, drops its session
    /// and, in editor mode or on an op-bearing row, opens it over the row's
    /// planes. The planes already known are kept when the row is unchanged.
    fn restore_row(&mut self, key: String) {
        let editing = self.state.mode == ShellMode::Editor;
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let unchanged = tab.selected_checkpoint.as_deref() == Some(key.as_str());
        let dropped_session = tab.drop_session();
        tab.selected_checkpoint = Some(key);
        tab.free_unneeded_held();
        let on_ops_row = tab.on_ops_row();
        if !unchanged || matches!(tab.row_planes, Some(RowPlanes::Refused(_))) {
            tab.row_planes = None;
        }
        self.state.canvas_dirty = true;
        if editing || on_ops_row {
            self.ensure_edit_session();
        }
        self.emit(ShellEvent::CheckpointsChanged);
        if dropped_session {
            self.emit(ShellEvent::EditorChanged);
        }
    }

    /// Picks up a settled background checkpoint render: the current row's
    /// planes or its refusal, dropped when the row changed while it ran.
    /// Returns `false` while the render is still running -- the poll's cue
    /// to keep going.
    pub fn settle_checkpoint_render(&mut self) -> bool {
        let Some(outcome) = self
            .state
            .render
            .outcome
            .lock()
            .expect("no thread panicked while holding the render outcome")
            .take()
        else {
            return self.state.render.in_flight.is_none();
        };
        let Some((tab_id, key)) = self.state.render.in_flight.take() else {
            return true;
        };
        let scratch_root = self.scratch_root();
        if let Some(tab) = self.state.projects.iter_mut().find(|tab| tab.id == tab_id)
            && tab.plane_key().as_deref() == Some(key.as_str())
            && matches!(tab.row_planes, Some(RowPlanes::Pending))
        {
            tab.row_planes = Some(match outcome {
                Ok((composite, zmap)) => {
                    tab.note_viewed(&scratch_root, &key);
                    RowPlanes::Ready(composite, zmap)
                }
                Err(lines) => RowPlanes::Refused(lines),
            });
        }
        self.state.canvas_dirty = true;
        if self.state.mode == ShellMode::Editor {
            self.leave_editor_if_unfit();
        } else if self.state.active().is_some_and(ProjectTab::on_ops_row) {
            self.ensure_edit_session();
        }
        true
    }

    /// Decodes the picture of tab `index` with `render` on a spawned worker,
    /// parking the result for [`Self::settle_thumbnails`]. A decode requested
    /// later for the same tab supersedes this one.
    fn request_thumbnail(
        &mut self,
        index: usize,
        render: impl FnOnce() -> Option<overlay::Thumbnail> + Send + 'static,
    ) {
        let tab = &mut self.state.projects[index];
        tab.thumbnail_generation += 1;
        let (tab_id, generation) = (tab.id, tab.thumbnail_generation);
        let slot = &mut self.state.thumbnails;
        slot.in_flight += 1;
        slot.started = true;
        let parked = Arc::clone(&slot.parked);
        std::thread::spawn(move || {
            let picture =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(render)).unwrap_or(None);
            parked
                .lock()
                .expect("no thread panicked while holding the parked pictures")
                .push((tab_id, generation, picture));
        });
    }

    /// Requests the picture of tab `index` from its own project: the stored
    /// composite of an opened one, the first frame of an unsaved one.
    fn request_tab_thumbnail(&mut self, index: usize) {
        let tab = &self.state.projects[index];
        match &tab.project {
            GuiProject::Opened(project) => {
                let path = project.path().to_path_buf();
                self.request_thumbnail(index, move || overlay::render_thumbnail_from_file(&path));
            }
            GuiProject::Unsaved(_) => {
                let first = tab.frames.first().cloned();
                self.request_thumbnail(index, move || {
                    overlay::render_thumbnail_from_frame(&first?)
                });
            }
        }
    }

    /// Applies the decoded pictures workers have parked: a picture lands only
    /// on a tab that still exists and whose newest request it answers; a
    /// failed decode keeps the previous picture and is not retried. Returns
    /// `true` once no decode is in flight -- the poll's cue to stop. Never
    /// waits on a worker.
    pub fn settle_thumbnails(&mut self) -> bool {
        let parked = std::mem::take(
            &mut *self
                .state
                .thumbnails
                .parked
                .lock()
                .expect("no thread panicked while holding the parked pictures"),
        );
        self.state.thumbnails.in_flight -= parked.len();
        let mut stored = false;
        for (tab_id, generation, picture) in parked {
            if let Some(tab) = self.state.projects.iter_mut().find(|tab| tab.id == tab_id)
                && tab.thumbnail_generation == generation
                && let Some(picture) = picture
            {
                tab.thumbnail = Some(Arc::new(picture));
                stored = true;
            }
        }
        if stored {
            self.emit(ShellEvent::OverlayChanged);
        }
        self.state.thumbnails.in_flight == 0
    }

    /// Whether a picture decode started since the last call -- the bridge's
    /// cue to start polling [`Self::settle_thumbnails`].
    pub fn take_thumbnails_started(&mut self) -> bool {
        std::mem::take(&mut self.state.thumbnails.started)
    }

    /// Whether a checkpoint render started since the last call -- the
    /// bridge's cue to start polling [`Self::settle_checkpoint_render`].
    pub fn take_render_started(&mut self) -> bool {
        std::mem::take(&mut self.state.render.started)
    }

    /// The right pane's refusal text for the active tab's current row, one
    /// entry per line; empty while it shows, or is rendering, an image.
    pub fn composite_message_lines(&self) -> Vec<String> {
        match self.state.active().and_then(|tab| tab.row_planes.as_ref()) {
            Some(RowPlanes::Refused(lines)) => lines.clone(),
            _ => Vec::new(),
        }
    }

    /// Whether the active tab has a current position to export: a saved
    /// row, a held row (including in a never-saved tab), or a tab with
    /// unsealed edits over one -- exactly when its own `ProjectTab::fused`
    /// resolves.
    pub fn export_enabled(&self) -> bool {
        self.state.active().is_some_and(|tab| tab.fused().is_some())
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

    /// Quits the window: with no tab holding unsaved work or a parked run
    /// waiting for Keep or Discard, closes it at once. Otherwise raises
    /// [`Self::pending_quit_copy`] and touches no tab. A no-op while a close,
    /// fuse or restore question is open or a `Save all and quit` is writing.
    pub fn window_close(&mut self) {
        if self.state.pending_close.is_some()
            || self.state.save.then == AfterSave::Quit
            || self.state.pending_fuse.is_some()
            || self.state.pending_restore.is_some()
            || self.state.pending_checkpoint_delete.is_some()
        {
            return;
        }
        if !self.state.projects.iter().any(ProjectTab::quit_loses_work) {
            self.emit(ShellEvent::WindowCloseRequested);
        } else {
            self.state.pending_quit = true;
            self.emit(ShellEvent::ProjectsChanged);
        }
    }

    /// Opens the export modal for the active tab's current position:
    /// `tab.edit` with unsealed ops, else its selected held row, else its
    /// selected saved row -- the same precedence row-selection locking
    /// already relies on (`ProjectTab::edit` is cleared on any row
    /// change). A no-op with nothing to export.
    pub fn checkpoint_export_requested(&mut self) {
        let Some(tab) = self.state.active() else {
            return;
        };
        let Some(dialog) = export_dialog_for(tab, &self.state.export_choice) else {
            return;
        };
        self.state.export_dialog = Some(dialog);
        self.emit(ShellEvent::CheckpointExportRequested);
    }

    /// Closes the export modal without writing anything -- its own Cancel.
    pub fn cancel_export(&mut self) {
        if self.state.export_dialog.take().is_some() {
            self.emit(ShellEvent::CheckpointExportRequested);
        }
    }

    /// Confirms the open export modal: asks `pick` for a destination (the
    /// OS save dialog, `picker::pick_save_image`'s own pattern, mirroring
    /// [`Self::save_active_project`]), then starts the write on a worker.
    /// A no-op with no dialog open; the dialog stays exactly as it was when
    /// `pick` returns `None` (the user cancelled the save dialog).
    pub fn export_confirmed(&mut self, pick: impl FnOnce(&Path) -> Option<PathBuf>) {
        let Some(dialog) = self.state.export_dialog.as_ref() else {
            return;
        };
        if !matches!(dialog.stage, ExportStage::Ready) {
            // Reached only for the writing state: its confirm button is
            // dimmed but still clickable, so this closes the modal rather
            // than starting a second write. The refused state's confirm is
            // routed to `export_locate_frames` before this method is ever
            // called.
            self.cancel_export();
            return;
        }
        let tab_id = dialog.tab_id;
        let choice = dialog.choice.clone();
        let Some(tab) = self.state.projects.iter().find(|t| t.id == tab_id) else {
            self.state.export_dialog = None;
            return;
        };
        let Some(source) = export_source_for(tab) else {
            self.state.export_dialog = None;
            return;
        };
        let proposed = export_proposed_path(tab, choice.format);
        let Some(path) = pick(&proposed) else {
            return;
        };
        let destination_name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        if let Some(dialog) = self.state.export_dialog.as_mut() {
            dialog.stage = ExportStage::Writing;
            dialog.destination_name = destination_name;
            dialog.destination = Some(path.clone());
        }
        self.start_export(tab_id, source, path, &choice);
        self.emit(ShellEvent::CheckpointExportRequested);
    }

    /// The refused export modal's own `Locate frames…`: verifies `folder`
    /// against the exporting tab's recorded sources the same way the
    /// missing-sources notice does ([`Self::locate_sources_notice`]), so
    /// [`Self::settle_locate`] can re-attempt the export once they resolve.
    /// `None` with no refused export dialog open, or while a previous
    /// verification is still running.
    pub fn export_locate_frames(&mut self, folder: PathBuf) -> Option<std::thread::JoinHandle<()>> {
        let dialog = self.state.export_dialog.as_ref()?;
        if !matches!(dialog.stage, ExportStage::Refused { locate: true, .. }) {
            return None;
        }
        let tab_id = dialog.tab_id;
        self.locate_frames(entry::LocateTarget::Tab(tab_id), folder)
    }

    /// Re-attempts a refused export at the same destination path once
    /// `Locate frames…` ([`Self::export_locate_frames`]) has resolved
    /// `tab_id`'s sources. Closes the modal instead if the tab, its current
    /// position or the destination it started from is gone by the time this
    /// runs.
    fn retry_export(&mut self, tab_id: u64) {
        let Some((destination, choice)) = self
            .state
            .export_dialog
            .as_ref()
            .and_then(|dialog| Some((dialog.destination.clone()?, dialog.choice.clone())))
        else {
            return;
        };
        let Some(tab) = self.state.projects.iter().find(|t| t.id == tab_id) else {
            self.state.export_dialog = None;
            return;
        };
        let Some(source) = export_source_for(tab) else {
            self.state.export_dialog = None;
            return;
        };
        if let Some(dialog) = self.state.export_dialog.as_mut() {
            dialog.stage = ExportStage::Writing;
        }
        self.start_export(tab_id, source, destination, &choice);
        self.emit(ShellEvent::CheckpointExportRequested);
    }

    /// Starts `source`'s export to `path` on a spawned worker, parking the
    /// result for [`Self::settle_export`]. Always starts a fresh outcome
    /// slot: a worker left running past its own dialog's Cancel keeps
    /// writing, but a later export's `settle_export` reads only the slot its
    /// own worker was handed, never a still-running one's stale result.
    fn start_export(
        &mut self,
        tab_id: u64,
        source: job::ExportSource,
        path: PathBuf,
        choice: &ExportChoice,
    ) {
        let (options, metadata) = (choice.options(), choice.metadata_options());
        self.state.export.in_flight = Some(tab_id);
        self.state.export.started = true;
        self.state.export.outcome = Arc::new(Mutex::new(None));
        let outcome = Arc::clone(&self.state.export.outcome);
        std::thread::spawn(move || {
            let result = job::export_outcome(std::panic::catch_unwind(|| {
                job::export_position(source, &path, &options, &metadata)
            }));
            *outcome
                .lock()
                .expect("no thread panicked while holding the export outcome") = Some(result);
        });
    }

    /// Picks up a settled export worker: closes the modal on success, or
    /// moves it into its refused state with the cause. `false` while still
    /// writing -- the poll's cue to keep going.
    pub fn settle_export(&mut self) -> bool {
        let Some(outcome) = self
            .state
            .export
            .outcome
            .lock()
            .expect("no thread panicked while holding the export outcome")
            .take()
        else {
            return self.state.export.in_flight.is_none();
        };
        self.state.export.in_flight = None;
        match outcome {
            Ok(()) => {
                if let Some(dialog) = self.state.export_dialog.take() {
                    self.remember_export_choice(dialog.choice);
                }
            }
            Err(job::ExportRefusal { lines, locate }) => {
                if let Some(dialog) = self.state.export_dialog.as_mut() {
                    dialog.stage = ExportStage::Refused { lines, locate };
                }
            }
        }
        self.emit(ShellEvent::CheckpointExportRequested);
        true
    }

    /// Whether an export write started since the last call -- the bridge's
    /// cue to start polling [`Self::settle_export`].
    pub fn take_export_started(&mut self) -> bool {
        std::mem::take(&mut self.state.export.started)
    }

    /// Whether a refused export modal offers `Locate frames…`.
    pub fn export_can_locate(&self) -> bool {
        matches!(
            self.state.export_dialog.as_ref().map(|d| &d.stage),
            Some(ExportStage::Refused { locate: true, .. })
        )
    }

    /// The open modal's format, TIFF while it is closed.
    pub fn export_format(&self) -> ExportFormat {
        self.state
            .export_dialog
            .as_ref()
            .map_or_else(|| ExportChoice::default().format, |d| d.choice.format)
    }

    /// The open modal's settings section; `None` when it is closed.
    pub fn export_view(&self) -> Option<ExportView> {
        let dialog = self.state.export_dialog.as_ref()?;
        Some(dialog.choice.view(dialog.open_menu.as_deref()))
    }

    /// Switches the ready modal to the format `label` names.
    pub fn export_select_format(&mut self, label: &str) {
        if let Some(dialog) = self.ready_export_dialog()
            && let Some(format) = export_settings::FORMATS
                .into_iter()
                .find(|f| f.id().eq_ignore_ascii_case(label))
        {
            dialog.choice.format = format;
            dialog.open_menu = None;
        }
    }

    /// Opens the list of descriptor `id`, or closes it when already open.
    pub fn export_toggle_menu(&mut self, id: &str) {
        if let Some(dialog) = self.ready_export_dialog() {
            let already = dialog.open_menu.as_deref() == Some(id);
            dialog.open_menu = (!already).then(|| id.to_string());
        }
    }

    /// Sets the enum descriptor `id` to its variant `variant_id`.
    pub fn export_choose(&mut self, id: &str, variant_id: &str) {
        self.set_export_value(id, ParamValue::Enum(variant_id.to_string()));
    }

    /// [`Self::export_choose`] for a control that names its variant by
    /// label.
    pub fn export_choose_label(&mut self, id: &str, label: &str) {
        let variant = self.export_view().and_then(|view| {
            let row = view.rows.into_iter().find(|r| r.id == id)?;
            let at = row.choices.iter().position(|c| c == label)?;
            row.choice_ids.get(at).cloned()
        });
        if let Some(variant) = variant {
            self.export_choose(id, &variant);
        }
    }

    /// Moves the JPEG quality slider to `fraction` of its range.
    pub fn export_set_quality_fraction(&mut self, fraction: f32) {
        let Some(desc) = params_for(ExportFormat::Jpeg)
            .iter()
            .find(|d| d.id == "jpeg-quality")
        else {
            return;
        };
        self.set_export_value("jpeg-quality", slider_value(fraction, &desc.kind));
    }

    /// Flips the `Metadata` switch.
    pub fn export_toggle_metadata(&mut self) {
        if let Some(dialog) = self.ready_export_dialog() {
            dialog.choice.metadata = !dialog.choice.metadata;
        }
    }

    fn ready_export_dialog(&mut self) -> Option<&mut ExportDialog> {
        self.state
            .export_dialog
            .as_mut()
            .filter(|d| matches!(d.stage, ExportStage::Ready))
    }

    /// Writes one descriptor value into the selected format's options
    /// through core's validation; a value core refuses changes nothing.
    fn set_export_value(&mut self, id: &str, value: ParamValue) {
        let Some(dialog) = self.ready_export_dialog() else {
            return;
        };
        let mut values = dialog.choice.options().values();
        values.insert(id.to_string(), value);
        if let Ok(options) = ExportOptions::from_values(dialog.choice.format, &values) {
            dialog.choice.set_options(options);
        }
        dialog.open_menu = None;
    }

    /// Keeps `choice` for the next modal and saves it when a config
    /// directory is installed; a failed save is logged, never an error.
    fn remember_export_choice(&mut self, choice: ExportChoice) {
        if let Some(dir) = &self.state.config_dir
            && let Err(e) = export_settings::save(dir, &choice)
        {
            tracing::warn!(error = %e, "the export choice could not be saved");
        }
        self.state.export_choice = choice;
    }

    /// Installs the export choice loaded at startup.
    pub fn install_export_choice(&mut self, choice: ExportChoice) {
        self.state.export_choice = choice;
    }

    /// Whether the export modal is open.
    pub fn export_open(&self) -> bool {
        self.state.export_dialog.is_some()
    }

    /// The open export modal's own context and what lines, already
    /// composed for its row -- `None` when it is closed.
    pub fn export_copy(&self) -> Option<(&str, &str)> {
        let dialog = self.state.export_dialog.as_ref()?;
        Some((dialog.context.as_str(), dialog.what.as_str()))
    }

    /// The open export modal's own stage: `"ready"` / `"writing"` /
    /// `"refused"` -- `"ready"` (the harmless default) when it is closed.
    pub fn export_stage(&self) -> &'static str {
        match self.state.export_dialog.as_ref().map(|d| &d.stage) {
            Some(ExportStage::Ready) | None => "ready",
            Some(ExportStage::Writing) => "writing",
            Some(ExportStage::Refused { .. }) => "refused",
        }
    }

    /// The chosen destination's own file name, named only while writing.
    pub fn export_destination_name(&self) -> &str {
        self.state
            .export_dialog
            .as_ref()
            .map_or("", |d| d.destination_name.as_str())
    }

    /// The export modal's refusal text, one entry per line; empty unless
    /// its last write refused.
    pub fn export_refusal_lines(&self) -> &[String] {
        match self.state.export_dialog.as_ref().map(|d| &d.stage) {
            Some(ExportStage::Refused { lines, .. }) => lines,
            _ => &[],
        }
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

    /// Applies `event` ([`Self::apply_action`]) and raises
    /// [`ShellEvent::Input`].
    fn emit_action(&mut self, event: ActionEvent) {
        self.apply_action(&event);
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

    /// The `BrushSoftness` wheel action: one detent moves `Soft` by 5 points,
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
            can_save_checkpoint: self.edit_session().is_some_and(|s| s.op_count() > 0),
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
    /// queues the region it returns for recompositing. Ignored while a
    /// stroke is live.
    pub fn undo(&mut self) {
        self.undo_or_redo(job::EditSession::undo);
    }

    /// `Ctrl + Shift + Z`: re-applies the most recently undone op and
    /// queues the region it returns for recompositing. Ignored while a
    /// stroke is live.
    pub fn redo(&mut self) {
        self.undo_or_redo(job::EditSession::redo);
    }

    /// Shared by [`Self::undo`]/[`Self::redo`]: `step` is whichever of the
    /// two returns `Some` (`None` inside meaning nothing visibly changed) or
    /// `None` when there was nothing to step. A no-op while a stroke is
    /// live.
    fn undo_or_redo(&mut self, step: fn(&mut job::EditSession) -> Option<Option<job::Rect>>) {
        let wake = self.recomposite_wake();
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let was_unsaved = tab.unsaved();
        let Some(edit) = tab.edit.as_mut() else {
            return;
        };
        if edit.stroke.is_some() {
            return;
        }
        let Some(region) = step(&mut edit.session) else {
            return;
        };
        if let Some(region) = region {
            edit.queue_recomposite(region, &wake);
        }
        if tab.unsaved() != was_unsaved {
            self.emit(ShellEvent::ProjectsChanged);
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
    /// nothing to push into). Otherwise, over either pane, opens a stroke at
    /// the pointer's source position, freezing the brush's source, radius and
    /// softness for the stroke. Both panes share one viewport, so the stroke
    /// is the same whichever pane the press lands in.
    fn paint_start(&mut self, x: f32, y: f32) {
        if self.state.brush.picking {
            let local = self.anchor();
            self.pick_source_at(local);
            self.state.brush.picking = false;
            self.emit(ShellEvent::EditorChanged);
            return;
        }
        let Some(pane) = self.pane_at(x, y) else {
            return;
        };
        let Some(source) = self.brush_source() else {
            return;
        };
        let (radius, softness) = self.brush_radius_softness();
        let local = (x - pane.x, y - pane.y);
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
    /// pick's drag, or a stroke that started outside both panes).
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
    /// queues exactly that segment's footprint for recompositing into the
    /// resident tiles -- one tick per repaint, never per motion event. A
    /// no-op with no live stroke or no motion since the last flush.
    pub fn flush_live_stroke(&mut self) {
        let wake = self.recomposite_wake();
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
        edit.queue_recomposite(region, &wake);
        edit.stroke
            .as_mut()
            .expect("the stroke that produced this tick is still live")
            .tick_rects
            .push(region);
    }

    /// `Paint`'s `End`: flushes remaining motion, discards the preview, then
    /// appends the stroke's resampled path unless its footprint is empty or
    /// off-image. Queues the tick rects (dilated) for recompositing, or the
    /// appended region itself when no tick fired (a press-release with no
    /// motion).
    fn paint_end(&mut self) {
        self.flush_live_stroke();
        let wake = self.recomposite_wake();
        let Some(tab) = self.state.active_mut() else {
            return;
        };
        let was_unsaved = tab.unsaved();
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
        // Only the stroke that lights the unsaved mark changes what the tab
        // strip and the project card show.
        let lit = !was_unsaved;
        let Some(region) = region else {
            if lit {
                self.emit(ShellEvent::ProjectsChanged);
            }
            return;
        };
        let map = edit.session.tile();
        if tick_rects.is_empty() {
            edit.queue_recomposite(region, &wake);
        } else {
            let margin = (radius / 8.0).ceil() as u32 + 2;
            for rect in tick_rects {
                let dilated = dilate_rect(rect, margin, map.width, map.height);
                edit.queue_recomposite(dilated, &wake);
            }
        }
        if lit {
            self.emit(ShellEvent::ProjectsChanged);
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
        let pane = self.pane_at(x, y).unwrap_or_else(|| {
            let mid = (left.x + left.width + right.x) / 2.0;
            if x < mid { left } else { right }
        });
        (
            x.clamp(pane.x, pane.x + pane.width) - pane.x,
            y.clamp(pane.y, pane.y + pane.height) - pane.y,
        )
    }

    /// The pane whose rect contains `(x, y)`, left first; `None` over the
    /// gutter, the divider, outside both, or with no panes installed.
    fn pane_at(&self, x: f32, y: f32) -> Option<PaneRect> {
        let (left, right) = self.state.panes?;
        [left, right]
            .into_iter()
            .find(|&pane| pane_contains(pane, x, y))
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

    fn set_space_held(&mut self, held: bool) {
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

    pub fn open_settings(&mut self) -> bool {
        self.state.settings_open = true;
        true
    }

    pub fn close_settings(&mut self) -> bool {
        self.state.settings_open = false;
        false
    }

    /// Whether one of the dialogs that block the chrome accelerators is
    /// open: Settings, the close/quit guard, the fuse and restore questions,
    /// the frame-set warning, the Keep/Discard divergence dialog, the
    /// grouping question or the refusal dialog.
    pub fn modal_open(&self) -> bool {
        self.state.settings_open
            || self.state.pending_close.is_some()
            || self.state.pending_quit
            || self.state.pending_fuse.is_some()
            || self.state.pending_restore.is_some()
            || self.state.pending_checkpoint_delete.is_some()
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
        let _scope = measurement_scope("gui_open_paths");
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
                OpenOutcome::Question(..) | OpenOutcome::Mismatch(_) | OpenOutcome::Refused(_) => {
                    Vec::new()
                }
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
        let _scope = measurement_scope("gui_settle_open");
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

    /// The active refusal's own title, `None` while the folder-empty
    /// refusal is showing -- it keeps its own fixed "No frames here" title
    /// and `Pick another folder…` action; every other refusal shows this
    /// title with a `Close` button instead.
    pub fn refusal_title(&self) -> Option<&str> {
        self.state.refusal_title.as_deref()
    }

    /// Closes the refusal dialog -- its own "Pick another folder…" button,
    /// or `Close` for every other refusal.
    pub fn dismiss_refusal(&mut self) {
        self.state.refusal_open = false;
    }

    /// `thumbnails` are the pictures the load's own worker already decoded
    /// for `outcome`'s own projects -- both [`Self::open_paths`] and
    /// [`Self::answer_question`] decode on their worker before parking the
    /// outcome here, so [`ProjectTab::new`] never decodes one of its own.
    fn resolve_open(&mut self, outcome: OpenOutcome, thumbnails: Vec<Preloaded>) -> OpenOutcome {
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
            OpenOutcome::Mismatch(pending_open) => {
                let echo = Box::new(pending_open.echo());
                self.state.pending_mismatch = Some(*pending_open);
                self.emit(ShellEvent::MismatchRaised);
                OpenOutcome::Mismatch(echo)
            }
            OpenOutcome::Refused(err) => self.refuse(err),
        }
    }

    /// The pending fingerprint mismatch, if [`Self::open_paths`] raised one
    /// -- what the mismatch dialog reads before the user chooses.
    pub fn pending_mismatch(&self) -> Option<&PendingOpen> {
        self.state.pending_mismatch.as_ref()
    }

    /// The pending mismatch's own copy, if one is raised: title, body and
    /// file name -- what the mismatch dialog reads.
    pub fn mismatch_copy(&self) -> Option<(&'static str, String, &str)> {
        self.state.pending_mismatch.as_ref().map(|p| {
            (
                overlay::MISMATCH_TITLE,
                overlay::mismatch_body(p.frame_count()),
                p.file_name.as_str(),
            )
        })
    }

    /// `Open anyway`: accepts the pending mismatch and opens the project
    /// unchanged, the same way a fresh open lands a tab. A no-op with
    /// nothing pending.
    pub fn open_anyway(&mut self) {
        let Some(pending) = self.state.pending_mismatch.take() else {
            return;
        };
        let project = pending.proceed();
        self.accept(
            Opened {
                projects: vec![project],
                skipped: Vec::new(),
            },
            Vec::new(),
        );
    }

    /// Dismisses the mismatch dialog with no project opened: `PendingProject`
    /// never proceeds without an explicit choice.
    pub fn cancel_mismatch(&mut self) {
        self.state.pending_mismatch = None;
        self.emit(ShellEvent::OverlayChanged);
    }

    /// The pending missing-sources notice's own copy, if accepting an open
    /// raised one: title, body and file name -- what the missing-sources
    /// dialog reads.
    pub fn sources_notice(&self) -> Option<(&'static str, String, &str)> {
        self.state.sources_notice.as_ref().map(|n| {
            (
                overlay::SOURCES_MISSING_TITLE,
                overlay::sources_missing_body(n.missing, n.total),
                n.file_name.as_str(),
            )
        })
    }

    /// The missing-sources dialog's own `Close`: dismisses the notice; the
    /// project stays open exactly as it is.
    pub fn close_sources_notice(&mut self) {
        self.state.sources_notice = None;
        self.emit(ShellEvent::OverlayChanged);
    }

    /// `Locate frames…` on the missing-sources dialog itself, for the tab
    /// [`Self::sources_notice`] names. A no-op with no notice pending.
    pub fn locate_sources_notice(
        &mut self,
        folder: PathBuf,
    ) -> Option<std::thread::JoinHandle<()>> {
        let tab_id = self.state.sources_notice.as_ref()?.tab_id;
        self.locate_frames(entry::LocateTarget::Tab(tab_id), folder)
    }

    /// Places every opened project in a tab, selects the first of them, and
    /// records the skipped-subfolder notice. New tabs always go at the end,
    /// whether "+" or a project tab was selected.
    /// Every skipped subfolder becomes a `Not a project` card; an opened
    /// project whose sources do not all resolve gets a `Sources missing`
    /// failure on its own tab; a path that opens successfully clears any
    /// `Not a project` card already held for it.
    fn accept(&mut self, opened: Opened, thumbnails: Vec<Preloaded>) {
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
                    project_identity_path(&project),
                    opened_at,
                );
                let missing = if let GuiProject::Opened(p) = &project {
                    let (missing, total) = entry::missing_sources(p);
                    (missing > 0)
                        .then(|| (missing, total, p.validation().accepted_mismatch.is_some()))
                } else {
                    None
                };
                let file_name = entry::tab_title(&project);
                let preloaded = thumbnails.next();
                let decode = preloaded.is_none();
                let (picture, meta) = match preloaded {
                    Some((decoded, meta)) => (decoded, Some(meta)),
                    None => (None, None),
                };
                self.next_tab_id += 1;
                let id = self.next_tab_id;
                let mut tab = ProjectTab::new(project, id, picture, meta);
                if let Some((missing, total, accepted_mismatch)) = missing {
                    tab.failure = Some(TabFailure {
                        kind: FailureKind::SourcesMissing,
                        stage: "NOTHING TO SHOW".to_string(),
                        reason: overlay::sources_missing_sentence(missing, total),
                    });
                    // A project that just landed through `Open anyway` shows no
                    // second modal for the same open -- its card still carries
                    // the sentence.
                    if !accepted_mismatch {
                        self.state.sources_notice = Some(SourcesNotice {
                            tab_id: id,
                            missing,
                            total,
                            file_name,
                        });
                    }
                }
                self.state.projects.push(tab);
                if decode {
                    self.request_tab_thumbnail(self.state.projects.len() - 1);
                }
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
        if matches!(&err, EntryError::ProjectRefused { .. }) {
            self.state.refusal_title = Some("This project cannot be opened".to_string());
        }
        OpenOutcome::Refused(err)
    }

    /// Refuses with an already-rendered message and no [`EntryError`] behind
    /// it -- what a re-fuse's own frame-reachability check uses, since
    /// nothing there is an entry-path failure. Clears any earlier refusal's
    /// own title, so a caller with one of its own sets it right after this
    /// call; every other refusal keeps the folder-empty dialog's fixed copy.
    pub fn refuse_message(&mut self, message: String) {
        self.state.last_refusal = Some(message.clone());
        self.state.refusal_title = None;
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
    /// acted on less than [`Self::CLOSE_DEBOUNCE`] before or the tab's save
    /// is running or queued. `now` is a parameter so tests step the clock
    /// without sleeping.
    pub fn close_project_at(&mut self, index: usize, now: Instant) {
        if let Some(tab) = self.state.projects.get(index)
            && !self.saving_tab(tab.id)
            && !self.close_debounced(now)
        {
            self.remove_tab(index, now);
        }
    }

    /// [`Self::request_close_at`] at the current time -- what every close
    /// path of the window calls.
    pub fn request_close(&mut self, index: usize) {
        self.request_close_at(index, Instant::now());
    }

    /// Asks to close tab `index` as of `now`: a tab with a modified live, an
    /// in-memory checkpoint or a queued delete raises [`Self::pending_close`]
    /// and stays open; any other tab closes at once. A no-op out of range,
    /// inside [`Self::CLOSE_DEBOUNCE`], while a quit, fuse or restore
    /// question is open, or for a tab whose save is running or queued.
    pub fn request_close_at(&mut self, index: usize, now: Instant) {
        if self.state.pending_quit
            || self.state.pending_fuse.is_some()
            || self.state.pending_restore.is_some()
            || self.state.pending_checkpoint_delete.is_some()
        {
            return;
        }
        let Some(tab) = self.state.projects.get(index) else {
            return;
        };
        if self.close_debounced(now) || self.saving_tab(tab.id) {
            return;
        }
        if !tab.loses_on_close() {
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
        if self
            .state
            .pending_fuse
            .is_some_and(|(id, _)| id == closed.id)
        {
            self.state.pending_fuse = None;
        }
        if self
            .state
            .pending_restore
            .as_ref()
            .is_some_and(|(id, _)| *id == closed.id)
        {
            self.state.pending_restore = None;
        }
        if self
            .state
            .pending_checkpoint_delete
            .as_ref()
            .is_some_and(|(id, _)| *id == closed.id)
        {
            self.state.pending_checkpoint_delete = None;
        }
        if let GuiProject::Opened(project) = &closed.project {
            let _ =
                std::fs::remove_dir_all(job::plane_cache_dir(&self.scratch_root(), project.path()));
        }
        remove_in_memory(&closed.held);
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

    /// Installs the `Recent` list loaded at startup and the config
    /// directory every table's save writes `settings.toml` into --
    /// `main.rs`'s own read, mirroring `install_input_table`; this crate's
    /// one exception to "no fs touch in `controller.rs`" is
    /// [`recent::save`]/[`recent::load`] themselves, which live in
    /// `crate::recent`, not here.
    pub fn install_recent(&mut self, entries: Vec<recent::RecentEntry>, config_dir: PathBuf) {
        self.state.recent = entries;
        self.state.config_dir = Some(config_dir);
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
    /// failure is logged but never a reason to interrupt an open: the
    /// `Recent` list stays correct in memory for the rest of the session.
    fn record_recent(&mut self, name: String, folder: PathBuf, path: PathBuf, opened_at: i64) {
        recent::record(&mut self.state.recent, name, folder, path, opened_at);
        if let Some(dir) = &self.state.config_dir
            && let Err(e) = recent::save(dir, &self.state.recent)
        {
            tracing::warn!(error = %e, "the Recent list could not be saved");
        }
    }

    /// Test-only wrapper to exercise a save failure without driving a
    /// whole project open.
    pub fn record_recent_for_test(
        &mut self,
        name: String,
        folder: PathBuf,
        path: PathBuf,
        opened_at: i64,
    ) {
        self.record_recent(name, folder, path, opened_at);
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
        let target = entry.path.clone();
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
    /// four-square button's own toggle, and `Esc`'s.
    pub fn toggle_projects_overlay(&mut self) {
        self.state.overlay_open = !self.state.overlay_open;
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
                meta_text: open_card_meta(
                    frame_count,
                    tab.listed_checkpoint_count(),
                    phase_line.as_deref(),
                ),
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
    /// no request kept for it. A tab whose live holds ops raises
    /// [`Self::pending_fuse_copy`] instead and returns `false`. Returns
    /// whether a run started.
    pub fn run_gallery_action(&mut self, index: usize) -> bool {
        self.run_gallery_action_at(index, false)
    }

    /// [`Self::run_gallery_action`]; `confirmed` skips the question about
    /// unsaved ops.
    fn run_gallery_action_at(&mut self, index: usize, confirmed: bool) -> bool {
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
        if !confirmed && self.ask_before_fuse(index, FuseEntry::RunAgain) {
            return false;
        }
        self.state.active = Some(index);
        self.run_request(index, request);
        true
    }

    /// Verifies `folder` against `target`'s recorded sources by content
    /// hash, off the event loop (a reference-scale stack is tens of
    /// gigabytes). [`Self::settle_locate`] applies a verified folder or
    /// raises the refusal dialog for a refused one. `None` with nothing to
    /// verify against, or while a previous verification is still running.
    pub fn locate_frames(
        &mut self,
        target: entry::LocateTarget,
        folder: PathBuf,
    ) -> Option<std::thread::JoinHandle<()>> {
        if self.state.locate.in_flight {
            return None;
        }
        let sources = match target {
            entry::LocateTarget::Mismatch => self.state.pending_mismatch.as_ref()?.sources.clone(),
            entry::LocateTarget::Tab(id) => {
                let index = self.tab_index(id)?;
                let tab = &self.state.projects[index];
                let GuiProject::Opened(project) = &tab.project else {
                    return None;
                };
                project.manifest().sources.clone()
            }
        };
        self.state.locate.in_flight = true;
        let sink = self.sink.clone();
        let outcome_slot = Arc::clone(&self.state.locate.outcome);
        Some(std::thread::spawn(move || {
            let result = entry::verify_folder(&sources, &folder);
            *outcome_slot
                .lock()
                .expect("no thread panicked while holding the locate outcome") = Some(LocateScan {
                target,
                folder,
                result,
            });
            if let Some(sink) = &sink {
                sink(ShellEvent::LocateReady);
            }
        }))
    }

    /// Whether a background `Locate frames…` verification is still running.
    pub fn locate_in_flight(&self) -> bool {
        self.state.locate.in_flight
    }

    /// Picks up the parked outcome of a settled [`Self::locate_frames`] and
    /// applies it: a verified folder relocates the target's frames and
    /// clears its failure and notice; a refused one raises the refusal
    /// dialog, leaving the card, frames and pending mismatch untouched.
    pub fn settle_locate(&mut self) {
        let Some(scan) = self
            .state
            .locate
            .outcome
            .lock()
            .expect("no thread panicked while holding the locate outcome")
            .take()
        else {
            return;
        };
        self.state.locate.in_flight = false;
        match scan.result {
            Ok(paths) => match scan.target {
                entry::LocateTarget::Mismatch => {
                    if let Some(pending) = self.state.pending_mismatch.take() {
                        match entry::reopen_relocated(&pending.path, scan.folder) {
                            Some(project) => {
                                self.accept(
                                    Opened {
                                        projects: vec![project],
                                        skipped: Vec::new(),
                                    },
                                    Vec::new(),
                                );
                            }
                            None => {
                                self.refuse_message(format!(
                                    "{} could not be reopened after its frames were located.",
                                    pending.file_name
                                ));
                                self.state.refusal_title =
                                    Some("This project cannot be opened".to_string());
                            }
                        }
                    }
                }
                entry::LocateTarget::Tab(id) => {
                    if let Some(index) = self.tab_index(id)
                        && let Some(tab) = self.state.projects.get_mut(index)
                    {
                        if let GuiProject::Opened(project) = &mut tab.project {
                            project.relocate_sources(scan.folder);
                        }
                        tab.meta.extend(images::read_metadata(&paths));
                        tab.excluded = vec![false; paths.len()];
                        tab.frames = paths;
                        tab.selected = images::first_included(&tab.excluded);
                        tab.failure = None;
                        if matches!(&self.state.sources_notice, Some(n) if n.tab_id == id) {
                            self.state.sources_notice = None;
                        }
                        self.emit(ShellEvent::OverlayChanged);
                    }
                    if matches!(&self.state.export_dialog, Some(d) if d.tab_id == id && matches!(d.stage, ExportStage::Refused { .. }))
                    {
                        self.retry_export(id);
                    }
                }
            },
            Err(refused) => {
                self.refuse_message(refused.message());
                self.state.refusal_title = Some("These are not the project's frames".to_string());
            }
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
            self.emit(ShellEvent::GalleryScrolled);
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
        self.emit(ShellEvent::GalleryScrolled);
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
    fn toggle_param_menu(&mut self, key: &str) {
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

    /// `toggle_param_menu`, composing the row key from its own
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
            self.emit(ShellEvent::SettingsScrolled);
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
        self.emit(ShellEvent::SettingsScrolled);
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
        let legend = self.mode() == ShellMode::Editor;
        let Some(tab) = self.state.active() else {
            return images::build_model(&[], &[], None, legend, &HashMap::new());
        };
        images::build_model(
            &tab.frames,
            &tab.excluded,
            Some(tab.selected),
            legend,
            &tab.meta,
        )
    }

    fn images_content_height(&self) -> f32 {
        self.state
            .active()
            .map_or(0.0, |tab| tab.frames.len() as f32 * images::ROW_HEIGHT)
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
        tab.read_missing_meta();
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
    /// convention `toggle_param_menu` uses for a settings row. A
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
    /// position. `Move` and `Up` belong to the drag in progress whatever
    /// their `index`. A no-op outside stacking mode or while this tab's own
    /// job is running; `Down` on an unresolved row or past the end starts
    /// nothing.
    pub fn frame_drag_pointer(&mut self, index: usize, kind: PointerKind, mouse_y: f32) {
        if self.mode() != ShellMode::Stacking || self.job_running() {
            return;
        }
        match kind {
            PointerKind::Down => {
                let draggable = self.state.active().is_some_and(|tab| {
                    tab.frames
                        .get(index)
                        .is_some_and(|path| tab.meta.get(path).is_some_and(|meta| !meta.unresolved))
                });
                if !draggable {
                    return;
                }
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
        let order = images::sort_order(&tab.frames, sort, &tab.meta);
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

    /// The close question still open, if any: which tab, what closing it
    /// loses -- strokes in live, in-memory checkpoints, queued deletes -- and
    /// where saving it would write.
    pub fn pending_close(&self) -> Option<CloseGuardCopy> {
        let id = self.state.pending_close?;
        let tab = self.state.projects.iter().find(|tab| tab.id == id)?;
        let (listed, deletes) = tab.unsaved_parts();
        let ops: Vec<&job::Op> = tab
            .edit
            .as_ref()
            .map(|edit| edit.session.ops().collect())
            .unwrap_or_default();
        let mut parts = Vec::new();
        if !ops.is_empty() {
            parts.push(unsaved_ops(ops.iter().copied()));
        }
        if listed > 0 {
            parts.push(format!(
                "{listed} unsaved checkpoint{}",
                if listed == 1 { "" } else { "s" }
            ));
        }
        if deletes > 0 {
            parts.push(format!(
                "{deletes} checkpoint{} to delete",
                if deletes == 1 { "" } else { "s" }
            ));
        }
        let them = if ops.len() + listed == 1 {
            "it"
        } else {
            "them"
        };
        let info = match &tab.project {
            GuiProject::Unsaved(_) => "Saving asks where to write the project file.".to_string(),
            GuiProject::Opened(project) => {
                let file = project
                    .path()
                    .file_name()
                    .unwrap_or(project.path().as_os_str())
                    .to_string_lossy();
                if deletes > 0 {
                    format!("Saving writes these changes to {file}.")
                } else {
                    format!("Saving adds {them} to {file}.")
                }
            }
        };
        Some(CloseGuardCopy {
            title: "Save this state first?".to_string(),
            context: format!("Closing {}", tab.title),
            body: format!("{}.", parts.join(", ")),
            info,
            secondary_label: "Save and close".to_string(),
            confirm_label: "Close anyway".to_string(),
        })
    }

    /// The fuse question still open, if any: the ops a fuse would lose and
    /// what each answer does.
    pub fn pending_fuse_copy(&self) -> Option<CloseGuardCopy> {
        let (id, _) = self.state.pending_fuse?;
        let tab = self.state.projects.iter().find(|tab| tab.id == id)?;
        let ops: Vec<&job::Op> = tab.edit.as_ref()?.session.ops().collect();
        Some(CloseGuardCopy {
            title: "Save your edits first?".to_string(),
            context: format!("Fusing {}", tab.title),
            body: format!("{} will be lost.", unsaved_ops(ops)),
            info: String::new(),
            secondary_label: "Save and fuse".to_string(),
            confirm_label: "Fuse anyway".to_string(),
        })
    }

    /// Clears the fuse question, returning its tab's index -- `None` when the
    /// tab is gone -- and the press it interrupted.
    fn take_pending_fuse(&mut self) -> Option<(Option<usize>, FuseEntry)> {
        let (id, entry) = self.state.pending_fuse.take()?;
        self.emit(ShellEvent::ProjectsChanged);
        let index = self.state.projects.iter().position(|tab| tab.id == id);
        Some((index, entry))
    }

    /// Repeats `entry` on tab `index` without asking again. Returns whether
    /// a run started or was enqueued.
    fn resume_fuse(&mut self, index: usize, entry: FuseEntry) -> bool {
        match entry {
            FuseEntry::Fuse => self.start_fuse_at(index, true),
            FuseEntry::Retry => self.retry_fuse_at(index, true),
            FuseEntry::RunAgain => self.run_gallery_action_at(index, true),
        }
    }

    /// The restore question still open, if any: the ops it would drop, the
    /// row it would restore and what each answer does.
    pub fn pending_restore_copy(&self) -> Option<CloseGuardCopy> {
        let (id, key) = self.state.pending_restore.as_ref()?;
        let tab = self.state.active().filter(|tab| tab.id == *id)?;
        let ops: Vec<&job::Op> = tab.edit.as_ref()?.session.ops().collect();
        let panel = self.checkpoints_panel();
        let target = panel
            .entries
            .iter()
            .find(|entry| entry.id.as_deref() == Some(key.as_str()))?;
        let (happened, belongs, them) = if ops.len() == 1 {
            ("has", "It belongs", "it")
        } else {
            ("have", "They belong", "them")
        };
        Some(CloseGuardCopy {
            title: "Save this state first?".to_string(),
            context: format!("Restoring {} · {}", target.algorithm, target.time_text),
            body: format!(
                "{} {happened} happened since the last checkpoint. {belongs} to no saved \
                 state.\n\nSave {them} as a checkpoint first, then restore — or restore \
                 and discard your changes, which loses {them}.",
                checkpoints::op_summary(ops)
            ),
            info: "A saved state lands at the top of the list, like any other.".to_string(),
            secondary_label: "Save first, then restore".to_string(),
            confirm_label: "Discard and restore".to_string(),
        })
    }

    /// Clears the restore question, returning the index of its tab -- `None`
    /// when that tab is no longer the active one, so no answer restores
    /// anything -- and its row key.
    fn take_pending_restore(&mut self) -> Option<(Option<usize>, String)> {
        let (id, key) = self.state.pending_restore.take()?;
        self.emit(ShellEvent::ProjectsChanged);
        let index = self
            .state
            .active
            .filter(|&i| self.state.projects.get(i).is_some_and(|tab| tab.id == id));
        Some((index, key))
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

    /// `Save and close`: clears the question and starts saving the asking
    /// tab as `save_project_at` does. The tab closes once that write
    /// has settled and nothing is unsaved on it -- otherwise the question is
    /// raised again; a failed write leaves the tab open. Returns whether a
    /// save started; with another save running nothing happens and the
    /// question stays.
    pub fn start_save_and_close(&mut self, pick: impl FnOnce(&Path) -> Option<PathBuf>) -> bool {
        if self.save_in_flight() {
            return false;
        }
        let Some(index) = self.take_pending_close() else {
            self.emit(ShellEvent::ProjectsChanged);
            return false;
        };
        if !self.save_project_at(index, pick) {
            self.emit(ShellEvent::ProjectsChanged);
            return false;
        }
        self.state.save.then = AfterSave::CloseTab(self.state.projects[index].id);
        true
    }

    /// [`Self::start_save_and_close`] run to completion on the calling
    /// thread: the entry for headless runs and tests. Returns whether the
    /// save started and wrote.
    pub fn save_and_close(&mut self, pick: impl FnOnce(&Path) -> Option<PathBuf>) -> bool {
        let started = self.start_save_and_close(pick);
        self.finish_saves() && started
    }

    /// The quit question still open, if any: how many tabs hold unsaved work
    /// -- a modified live, an in-memory checkpoint, a queued delete -- or a
    /// parked run nobody decided on.
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

    /// `Save all and quit`: asks `pick` now for each tab that would lose work
    /// on close, then saves them in turn. The window closes after the last
    /// write unless a tab holds unsaved work, then the quit question
    /// returns. A declined pick or failed write cancels the quit, keeping
    /// what saved. Returns whether every pick was answered and the first
    /// save started.
    pub fn start_save_all_and_quit(&mut self, pick: impl Fn(&Path) -> Option<PathBuf>) -> bool {
        if self.save_in_flight() {
            return false;
        }
        let mut queue = VecDeque::new();
        let mut declined = false;
        for tab in self
            .state
            .projects
            .iter()
            .filter(|tab| tab.loses_on_close())
        {
            match save_target(tab, &pick) {
                Some(target) => queue.push_back((tab.id, target)),
                None => {
                    declined = true;
                    break;
                }
            }
        }
        self.state.pending_quit = false;
        self.state.save.queue = queue;
        if !declined {
            self.state.save.then = AfterSave::Quit;
        }
        let advanced = self.advance_saves();
        self.emit(ShellEvent::ProjectsChanged);
        !declined && advanced
    }

    /// [`Self::start_save_all_and_quit`] run to completion on the calling
    /// thread: the entry for headless runs and tests. Returns whether every
    /// save started and wrote.
    pub fn save_all_and_quit(&mut self, pick: impl Fn(&Path) -> Option<PathBuf>) -> bool {
        let started = self.start_save_all_and_quit(pick);
        self.finish_saves() && started
    }

    /// The close guard's `Cancel`, whichever question is open -- the fuse or
    /// restore question, a close question, otherwise a quit question. A no-op
    /// with none open.
    pub fn close_guard_cancelled(&mut self) {
        if self.state.pending_fuse.is_some() {
            self.take_pending_fuse();
        } else if self.state.pending_restore.is_some() {
            self.take_pending_restore();
        } else if self.state.pending_close.is_some() {
            self.cancel_close();
        } else if self.state.pending_quit {
            self.cancel_quit();
        }
    }

    /// [`Self::start_close_guard_secondary`] run to completion on the calling
    /// thread: the entry for headless runs and tests.
    pub fn close_guard_secondary_pressed(
        &mut self,
        pick: impl Fn(&Path) -> Option<PathBuf>,
    ) -> bool {
        let started = self.start_close_guard_secondary(pick);
        self.finish_saves() && started
    }

    /// The close guard's secondary answer, whichever question is open --
    /// [`Self::snapshot_live`] then the interrupted fuse for the fuse
    /// question, [`Self::start_save_and_close`] for a close question,
    /// [`Self::start_save_all_and_quit`] for a quit question. `false` with
    /// none open, and for a fuse question when no run started or was queued.
    pub fn start_close_guard_secondary(&mut self, pick: impl Fn(&Path) -> Option<PathBuf>) -> bool {
        if let Some((index, entry)) = self.take_pending_fuse() {
            match index {
                Some(index) if self.snapshot_live(index) => self.resume_fuse(index, entry),
                _ => false,
            }
        } else if let Some((tab, key)) = self.take_pending_restore() {
            if let Some(index) = tab
                && self.snapshot_live(index)
            {
                self.restore_row(key);
            }
            false
        } else if self.state.pending_close.is_some() {
            self.start_save_and_close(pick)
        } else if self.state.pending_quit {
            self.start_save_all_and_quit(pick)
        } else {
            false
        }
    }

    /// The close guard's destructive answer, whichever question is open --
    /// the interrupted fuse for the fuse question,
    /// [`Self::close_without_saving`] for a close question,
    /// [`Self::quit_without_saving`] for a quit question. A no-op with none
    /// open.
    pub fn close_guard_confirmed(&mut self) {
        if let Some((index, entry)) = self.take_pending_fuse() {
            if let Some(index) = index {
                self.resume_fuse(index, entry);
            }
        } else if let Some((tab, key)) = self.take_pending_restore() {
            if tab.is_some() {
                self.restore_row(key);
            }
        } else if self.state.pending_close.is_some() {
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
            None,
            Some(tab.meta.clone()),
        );
        new_tab.fuse_id = tab.fuse_id.clone();
        new_tab.align_id = tab.align_id.clone();
        new_tab.fuse_values = tab.fuse_values.clone();
        new_tab.align_values = tab.align_values.clone();
        self.state.projects.push(new_tab);
        let index = self.state.projects.len() - 1;
        self.request_tab_thumbnail(index);
        self.set_active_tab(Some(index));
        self.emit(ShellEvent::ProjectsChanged);
        true
    }

    fn set_images_scroll(&mut self, offset_px: f32) {
        if self.images_viewport_scrolled(offset_px) {
            self.emit(ShellEvent::ImagesScrolled);
        }
    }

    /// Mirrors the `Images` list's own scroll offset, clamped, with no event
    /// -- the list already shows it. Returns whether the offset moved.
    pub fn images_viewport_scrolled(&mut self, offset_px: f32) -> bool {
        let offset = settings_panel::clamp_panel_scroll(
            offset_px,
            self.images_content_height(),
            self.state.images_viewport_height,
        );
        let moved = offset != self.state.images_scroll_y;
        self.state.images_scroll_y = offset;
        moved
    }

    /// The `Images` panel scrollbar thumb's current `(top, height)` in
    /// track-pixel space -- [`Self::settings_scrollbar_thumb`]'s twin.
    fn images_scrollbar_thumb(&self) -> (f32, f32) {
        let content_height = self.images_content_height();
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
                    let content_height = self.images_content_height();
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
        let content_height = self.images_content_height();
        let target_top = mouse_y - grab_offset;
        self.set_images_scroll(target_top * content_height / viewport_height);
    }

    /// The `Images` panel viewport's measured height -- pushed on install
    /// and on every resize; re-clamps the scroll offset.
    pub fn set_images_viewport_height(&mut self, height: f32) {
        self.state.images_viewport_height = height.max(0.0);
        self.images_viewport_scrolled(self.state.images_scroll_y);
        self.emit(ShellEvent::ImagesScrolled);
    }

    /// The `Images` list's scroll offset, clamped to the active tab's rows.
    pub fn images_scroll_y(&self) -> f32 {
        settings_panel::clamp_panel_scroll(
            self.state.images_scroll_y,
            self.images_content_height(),
            self.state.images_viewport_height,
        )
    }

    /// Scrolls the `Checkpoints` list by `delta_px`, clamped to its current
    /// content/viewport -- the Images list's own scroll handler's twin.
    pub fn scroll_checkpoints_panel(&mut self, delta_px: f32) {
        self.set_checkpoints_scroll(self.state.checkpoints_scroll_y + delta_px);
    }

    /// [`Self::set_images_scroll`]'s twin.
    fn set_checkpoints_scroll(&mut self, offset_px: f32) {
        let content_height = self.checkpoints_panel().content_height;
        let viewport_height = self.state.checkpoints_viewport_height;
        let offset = settings_panel::clamp_panel_scroll(offset_px, content_height, viewport_height);
        if offset != self.state.checkpoints_scroll_y {
            self.state.checkpoints_scroll_y = offset;
            self.emit(ShellEvent::CheckpointsChanged);
        }
    }

    /// The `Checkpoints` panel scrollbar thumb's current `(top, height)` in
    /// track-pixel space -- [`Self::images_scrollbar_thumb`]'s twin.
    fn checkpoints_scrollbar_thumb(&self) -> (f32, f32) {
        let content_height = self.checkpoints_panel().content_height;
        let viewport_height = self.state.checkpoints_viewport_height;
        if content_height <= 0.0 {
            return (0.0, viewport_height);
        }
        let height = (viewport_height * viewport_height / content_height).max(16.0);
        let top = self.state.checkpoints_scroll_y * viewport_height / content_height;
        (top, height)
    }

    /// A press on the `Checkpoints` scrollbar -- [`Self::images_scrollbar_pointer`]'s
    /// twin.
    pub fn checkpoints_scrollbar_pointer(&mut self, kind: PointerKind, mouse_y: f32) {
        match kind {
            PointerKind::Down => {
                let (thumb_top, thumb_height) = self.checkpoints_scrollbar_thumb();
                if mouse_y >= thumb_top && mouse_y < thumb_top + thumb_height {
                    self.state.checkpoints_scrollbar_drag = Some(mouse_y - thumb_top);
                } else {
                    let viewport_height = self.state.checkpoints_viewport_height.max(1.0);
                    let content_height = self.checkpoints_panel().content_height;
                    let target_top = mouse_y - thumb_height / 2.0;
                    self.set_checkpoints_scroll(target_top * content_height / viewport_height);
                    self.state.checkpoints_scrollbar_drag = Some(thumb_height / 2.0);
                }
            }
            PointerKind::Up => self.state.checkpoints_scrollbar_drag = None,
            PointerKind::Move => {}
        }
    }

    /// The live `Checkpoints` scrollbar drag, if any -- [`Self::images_scrollbar_dragged`]'s
    /// twin.
    pub fn checkpoints_scrollbar_dragged(&mut self, mouse_y: f32) {
        let Some(grab_offset) = self.state.checkpoints_scrollbar_drag else {
            return;
        };
        let viewport_height = self.state.checkpoints_viewport_height.max(1.0);
        let content_height = self.checkpoints_panel().content_height;
        let target_top = mouse_y - grab_offset;
        self.set_checkpoints_scroll(target_top * content_height / viewport_height);
    }

    /// The `Checkpoints` panel viewport's measured height -- pushed on
    /// install and on every resize; re-clamps the scroll offset --
    /// [`Self::set_images_viewport_height`]'s twin.
    pub fn set_checkpoints_viewport_height(&mut self, height: f32) {
        self.state.checkpoints_viewport_height = height.max(0.0);
        let content_height = self.checkpoints_panel().content_height;
        self.state.checkpoints_scroll_y = settings_panel::clamp_panel_scroll(
            self.state.checkpoints_scroll_y,
            content_height,
            self.state.checkpoints_viewport_height,
        );
        self.emit(ShellEvent::CheckpointsChanged);
    }

    /// [`Self::images_scroll_y`]'s twin.
    pub fn checkpoints_scroll_y(&self) -> f32 {
        self.state.checkpoints_scroll_y
    }

    /// The current index of the tab carrying id `id`, if it is still open --
    /// what lets the live-run slot and the queue outlive a close that shifts
    /// every later index down.
    fn tab_index(&self, id: u64) -> Option<usize> {
        self.state.projects.iter().position(|tab| tab.id == id)
    }

    /// The id of the tab currently at tab-strip index `index`, if any --
    /// resolved once, before starting a `Locate frames…` verification whose
    /// own worker thread must survive a later close or reorder shifting
    /// that index.
    pub fn tab_id_at(&self, index: usize) -> Option<u64> {
        self.state.projects.get(index).map(|tab| tab.id)
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
    fn would_queue(&self, index: usize) -> bool {
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
        let scratch_root = self.scratch_root();
        let (staging_key, hold_dir) = hold_dir_for(tab, &scratch_root);
        Some(job::JobRequest {
            frames,
            hold_dir,
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
    /// promotion [`Self::settle_job`] runs. Every tab holding that frame
    /// shows it unresolved from then on.
    fn refuse_missing_frame(&mut self, missing: &Path) {
        self.refuse_message(format!(
            "{} is no longer on disk. This project cannot be fused again until it is back.",
            missing.display()
        ));
        for tab in &mut self.state.projects {
            if let Some(meta) = tab.meta.get_mut(missing) {
                meta.unresolved = true;
            }
        }
        self.emit(ShellEvent::ImagesChanged);
    }

    /// Whether tab `id` holds the live run or a place in the queue.
    fn holds_run_slot(&self, id: u64) -> bool {
        self.any_job_running() && (self.job.tab_id == Some(id) || self.job.queue.contains(&id))
    }

    /// Raises the fuse question when a press of `entry` on tab `index` would
    /// start or queue a run over a modified live. `true` when the press is
    /// handled -- asked, or swallowed while another question is open -- and
    /// nothing may start.
    fn ask_before_fuse(&mut self, index: usize, entry: FuseEntry) -> bool {
        let Some(tab) = self.state.projects.get(index) else {
            return false;
        };
        if !tab.live_modified() || self.holds_run_slot(tab.id) {
            return false;
        }
        if !self.modal_open() {
            self.state.pending_fuse = Some((tab.id, entry));
            self.emit(ShellEvent::ProjectsChanged);
        }
        true
    }

    /// If another run is live, enqueues tab `id` unless it already holds
    /// the live run or a queue slot -- the shared tail of [`Self::start_fuse`]
    /// and [`Self::retry_fuse`]. `None` when no run is live, so the caller
    /// falls through to starting immediately.
    fn try_enqueue(&mut self, id: u64) -> Option<bool> {
        if !self.any_job_running() {
            return None;
        }
        if self.holds_run_slot(id) {
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
    /// the tab instead. A tab whose live holds ops raises
    /// [`Self::pending_fuse_copy`] instead and returns `false`; its answers
    /// start the run. Returns whether a run started or was enqueued.
    pub fn start_fuse(&mut self) -> bool {
        match self.state.active {
            Some(active) => self.start_fuse_at(active, false),
            None => false,
        }
    }

    /// [`Self::start_fuse`] on tab `index`; `confirmed` skips the question
    /// about unsaved ops.
    fn start_fuse_at(&mut self, index: usize, confirmed: bool) -> bool {
        let Some(tab) = self.state.projects.get(index) else {
            return false;
        };
        if !tab.errors.is_empty() {
            return false;
        }
        let id = tab.id;
        let frames = tab.included_frames();
        if let Some(missing) = frames.iter().find(|path| !path.is_file()) {
            self.refuse_missing_frame(missing);
            return false;
        }
        if !confirmed && self.ask_before_fuse(index, FuseEntry::Fuse) {
            return false;
        }
        if let Some(enqueued) = self.try_enqueue(id) {
            return enqueued;
        }
        let Some(request) = self.fuse_request_for(index) else {
            return false;
        };
        self.run_request(index, request)
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
    /// started once. A tab whose live holds ops raises
    /// [`Self::pending_fuse_copy`] instead and returns `false`.
    pub fn retry_fuse(&mut self) -> bool {
        match self.state.active {
            Some(active) => self.retry_fuse_at(active, false),
            None => false,
        }
    }

    /// [`Self::retry_fuse`] on tab `index`; `confirmed` skips the question
    /// about unsaved ops.
    fn retry_fuse_at(&mut self, index: usize, confirmed: bool) -> bool {
        let Some(tab) = self.state.projects.get(index) else {
            return false;
        };
        let Some(request) = tab.request.clone() else {
            return false;
        };
        let id = tab.id;
        if !confirmed && self.ask_before_fuse(index, FuseEntry::Retry) {
            return false;
        }
        if let Some(enqueued) = self.try_enqueue(id) {
            return enqueued;
        }
        self.run_request(index, request)
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
        self.state.active().map(ProjectTab::checkpoint_count)
    }

    /// The checkpoints panel's own empty-state line, shown in place of the
    /// row list while no tab is active or it holds no checkpoint yet.
    pub fn checkpoint_panel_line(&self) -> String {
        "None yet — fuse a stack".to_string()
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
        // A run finished on the tab being saved commits once the save settles.
        let saving = self.job.tab_id.is_some_and(|id| self.is_saving(id));
        if saving || !handle.is_finished() {
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
        let parent = tab.row_parent();
        if let Some(request) = &tab.request {
            tab.frames = request.frames.clone();
        }
        tab.excluded = vec![false; tab.frames.len()];
        tab.selected = images::first_included(&tab.excluded);
        tab.frame_sort = None;
        tab.read_missing_meta();
        tab.drop_session();
        tab.failure = None;
        let planes = (held.composite.clone(), held.zmap.clone());
        tab.selected_checkpoint = Some(checkpoints::held_key(tab.held.len()));
        tab.row_planes = None;
        tab.held.push(job::InMemoryCheckpoint {
            kind: job::InMemoryKind::Fused(held),
            parent,
        });
        tab.free_unneeded_held();
        // The new checkpoint's own extent may differ from what the viewport
        // was fitted to (a first fuse's crop, a re-fuse over a changed frame
        // set) -- refit on the next `fit_if_unfitted` rather than keep
        // drawing the old extent's zoom/pan.
        tab.viewport = None;
        self.mark_fresh_checkpoint(index);
        self.enter_editor_if_viewing(index);
        let composite = planes.0.clone();
        self.request_thumbnail(index, move || {
            overlay::render_thumbnail_from_frame(&composite)
        });
        planes
    }

    /// Marks tab `index`'s selected row as just created, with a generation
    /// above every earlier one. Does nothing for a tab not in view, which
    /// then never flashes.
    fn mark_fresh_checkpoint(&mut self, index: usize) {
        if self.state.active != Some(index) {
            return;
        }
        let Some(key) = self.state.projects[index].selected_checkpoint.clone() else {
            return;
        };
        self.checkpoint_generation += 1;
        self.state.projects[index].fresh_checkpoint = Some((key, self.checkpoint_generation));
    }

    /// The generation of the active tab's fresh row when the panel has not
    /// shown it yet, once: every later call returns `None` until a new
    /// checkpoint is created.
    pub fn take_checkpoint_flash(&mut self) -> Option<u64> {
        let (_, generation) = self.state.active()?.fresh_checkpoint.as_ref()?;
        let generation = *generation;
        if generation <= self.checkpoint_flash_played {
            return None;
        }
        self.checkpoint_flash_played = generation;
        Some(generation)
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
        if self.history_frozen(index) {
            return None;
        }
        let parked = self.state.projects.get_mut(index)?.parked.take()?;
        let at = checkpoints::local_hhmm(parked.held.created_at);
        let planes = self.commit_held(index, parked.held);
        self.state.projects[index].run = job::RunState::Fused { at };
        self.state.canvas_dirty = true;
        self.emit(ShellEvent::JobChanged);
        Some(planes)
    }

    /// `Discard` on the active tab's parked run: deletes its held files and
    /// leaves the tab `Idle` with its checkpoint count unchanged.
    pub fn discard_fused(&mut self) {
        if self.active_history_frozen() {
            return;
        }
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

    /// The active tab's current row's composite and Z-Map paths: a held
    /// row's own files, a saved row's pair in the plane cache. `None` with
    /// no current row, while a saved row's cache is not fresh, or once it
    /// refused.
    pub fn materialize_active_tab_planes(&mut self) -> Option<(PathBuf, PathBuf)> {
        match self.active_tab_planes() {
            TabPlanes::Ready(composite, zmap) => Some((composite, zmap)),
            TabPlanes::Pending | TabPlanes::None => None,
        }
    }

    /// [`Self::materialize_active_tab_planes`], telling a pending render
    /// apart. A saved row whose cache pair is fresh answers at once; any
    /// other starts the one background render, or waits for the running one
    /// to settle first. Never writes a plane.
    fn active_tab_planes(&mut self) -> TabPlanes {
        let scratch_root = self.scratch_root();
        let render_free = self.state.render.in_flight.is_none();
        let Some(tab) = self.state.active_mut() else {
            return TabPlanes::None;
        };
        let Some(mut key) = tab.selected_checkpoint.clone() else {
            return TabPlanes::None;
        };
        if let Some(index) = checkpoints::held_index(&key) {
            let Some(entry) = tab.held.get(index) else {
                return TabPlanes::None;
            };
            if let Some(held) = fused_of(entry) {
                return TabPlanes::Ready(held.composite.clone(), held.zmap.clone());
            }
            // An op-bearing row shows the planes it was taken
            // from; the session's resident tiles carry its edits.
            if let Some(RowPlanes::Ready(composite, zmap)) = &tab.row_planes {
                return TabPlanes::Ready(composite.clone(), zmap.clone());
            }
            match tab.resolve_row(&key) {
                Some(RowBase::Held(base)) => {
                    return tab
                        .held
                        .get(base)
                        .and_then(fused_of)
                        .map_or(TabPlanes::None, |held| {
                            TabPlanes::Ready(held.composite.clone(), held.zmap.clone())
                        });
                }
                Some(RowBase::Saved(id)) => key = id,
                None => return TabPlanes::None,
            }
        }
        match &tab.row_planes {
            Some(RowPlanes::Ready(composite, zmap)) => {
                return TabPlanes::Ready(composite.clone(), zmap.clone());
            }
            Some(RowPlanes::Pending) => return TabPlanes::Pending,
            Some(RowPlanes::Refused(_)) => return TabPlanes::None,
            None => {}
        }
        let GuiProject::Opened(project) = &mut tab.project else {
            return TabPlanes::None;
        };
        match job::cached_checkpoint_planes(project, &scratch_root, &key) {
            Ok(job::CheckpointPlanes::Ready(composite, zmap)) => {
                tab.note_viewed(&scratch_root, &key);
                tab.row_planes = Some(RowPlanes::Ready(composite.clone(), zmap.clone()));
                TabPlanes::Ready(composite, zmap)
            }
            Err(err) => {
                tab.row_planes = Some(RowPlanes::Refused(job::render_refusal_lines(&err)));
                TabPlanes::None
            }
            Ok(job::CheckpointPlanes::NeedsRender) => {
                if render_free {
                    let project_path = project.path().to_path_buf();
                    tab.row_planes = Some(RowPlanes::Pending);
                    let tab_id = tab.id;
                    self.start_checkpoint_render(tab_id, key, project_path, scratch_root);
                }
                TabPlanes::Pending
            }
        }
    }

    /// Renders `key` of the project at `project_path` on a spawned worker,
    /// parking the result for [`Self::settle_checkpoint_render`].
    fn start_checkpoint_render(
        &mut self,
        tab_id: u64,
        key: String,
        project_path: PathBuf,
        scratch_root: PathBuf,
    ) {
        self.state.render.in_flight = Some((tab_id, key.clone()));
        self.state.render.started = true;
        let outcome = Arc::clone(&self.state.render.outcome);
        std::thread::spawn(move || {
            let result = job::render_outcome(std::panic::catch_unwind(|| {
                job::render_checkpoint_planes(&project_path, &scratch_root, &key)
            }));
            *outcome
                .lock()
                .expect("no thread panicked while holding the render outcome") = Some(result);
        });
    }

    /// The active tab's fused checkpoints kept in memory since its last
    /// save, oldest first; empty with no active tab.
    pub fn held_checkpoints(&self) -> Vec<job::HeldCheckpoint> {
        self.active_project()
            .and_then(|i| self.state.projects.get(i))
            .map_or_else(Vec::new, |tab| {
                tab.held.iter().filter_map(fused_of).cloned().collect()
            })
    }

    /// Whether the active tab has something a project save would write -- an
    /// in-memory checkpoint, a pending delete or a modified live -- and no
    /// save is running: what enables `Save project` and its `Ctrl+S`.
    pub fn save_enabled(&self) -> bool {
        !self.save_in_flight()
            && self
                .active_project()
                .and_then(|i| self.state.projects.get(i))
                .is_some_and(ProjectTab::has_unsaved_write)
    }

    /// Whether a save worker is running. Saves run one at a time; a second
    /// waits in the queue only for `Save all and quit`.
    pub fn save_in_flight(&self) -> bool {
        self.state.save.running.is_some()
    }

    /// Whether tab `id` has a save running.
    fn is_saving(&self, id: u64) -> bool {
        self.state
            .save
            .running
            .as_ref()
            .is_some_and(|running| running.tab_id == id)
    }

    /// Whether tab `id` is being saved or waits for its save.
    fn saving_tab(&self, id: u64) -> bool {
        self.is_saving(id)
            || self
                .state
                .save
                .queue
                .iter()
                .any(|&(queued, _)| queued == id)
    }

    /// Whether tab `index` is being saved: its checkpoint list, pending
    /// deletes and parked run stay as the worker reads them until the save
    /// settles.
    fn history_frozen(&self, index: usize) -> bool {
        self.state
            .projects
            .get(index)
            .is_some_and(|tab| self.is_saving(tab.id))
    }

    fn active_history_frozen(&self) -> bool {
        self.state
            .active
            .is_some_and(|index| self.history_frozen(index))
    }

    /// Keeps tab `index`'s live -- its origin's Z-Map with the session's ops
    /// -- as an op-bearing checkpoint in memory under the current row. The
    /// session is rebased onto it and it becomes the current row. `false`,
    /// changing nothing, when live is unmodified or the checkpoint cannot be
    /// taken.
    pub fn snapshot_live(&mut self, index: usize) -> bool {
        if self.history_frozen(index) {
            return false;
        }
        let shown = if self.state.active == Some(index) {
            match self.active_tab_planes() {
                TabPlanes::Ready(composite, zmap) => Some(RowPlanes::Ready(composite, zmap)),
                TabPlanes::Pending | TabPlanes::None => None,
            }
        } else {
            None
        };
        let Some(tab) = self.state.projects.get_mut(index) else {
            return false;
        };
        let Some(edit) = tab.edit.as_ref() else {
            return false;
        };
        if edit.session.op_count() == 0 {
            return false;
        }
        let Some(parent) = tab.row_parent() else {
            return false;
        };
        let kind = job::InMemoryKind::Ops {
            ops: edit.session.ops().cloned().collect(),
            created_at: job::now_unix(),
        };
        if let Some(edit) = tab.edit.as_mut() {
            edit.session.rebase();
        }
        tab.held.push(job::InMemoryCheckpoint {
            kind,
            parent: Some(parent),
        });
        tab.selected_checkpoint = Some(checkpoints::held_key(tab.held.len() - 1));
        tab.row_planes = shown;
        self.mark_fresh_checkpoint(index);
        self.emit(ShellEvent::CheckpointsChanged);
        self.emit(ShellEvent::EditorChanged);
        true
    }

    /// Queues the deletion of listed row `id` (a saved id or `held:<n>`) on
    /// the active tab, removing exactly that row from the list; the next
    /// project save applies it. `false`, changing nothing, for a key the list
    /// does not show (unknown, hidden or already queued) and for the last
    /// listed row.
    pub fn queue_checkpoint_delete(&mut self, id: &str) -> bool {
        if self.active_history_frozen() {
            return false;
        }
        let panel = self.checkpoints_panel();
        if panel.row_count <= 1 || !panel.entries.iter().any(|e| e.id.as_deref() == Some(id)) {
            return false;
        }
        let Some(tab) = self.state.active_mut() else {
            return false;
        };
        tab.pending_deletes.push(match checkpoints::held_index(id) {
            Some(index) => job::ParentRef::Entry(index),
            None => job::ParentRef::Saved(id.to_string()),
        });
        self.emit(ShellEvent::CheckpointsChanged);
        true
    }

    /// Opens the delete question for flat entry `index` of
    /// [`Self::checkpoints_panel`]. A no-op on a separator, past the end,
    /// with one row listed and while a dialog is open.
    pub fn request_checkpoint_delete(&mut self, index: usize) {
        if self.modal_open() || self.active_history_frozen() {
            return;
        }
        let panel = self.checkpoints_panel();
        let Some(id) = panel.entries.get(index).and_then(|entry| entry.id.clone()) else {
            return;
        };
        let Some(tab) = self.state.active() else {
            return;
        };
        if panel.row_count <= 1 {
            return;
        }
        self.state.pending_checkpoint_delete = Some((tab.id, id));
        self.emit(ShellEvent::ProjectsChanged);
    }

    /// The delete question still open, if any: the row's `algorithm · time`
    /// and the sentence on what stays.
    pub fn pending_checkpoint_delete_copy(&self) -> Option<(String, String)> {
        let (id, key) = self.state.pending_checkpoint_delete.as_ref()?;
        self.state.active().filter(|tab| tab.id == *id)?;
        let panel = self.checkpoints_panel();
        let row = panel
            .entries
            .iter()
            .find(|entry| entry.id.as_deref() == Some(key.as_str()))?;
        let others = panel.row_count - 1;
        let body = if others == 1 {
            "The other checkpoint is untouched, and the stack is not fused again.".to_string()
        } else {
            format!(
                "The other {others} checkpoints are untouched, and the stack is not fused again."
            )
        };
        Some((format!("{} · {}", row.algorithm, row.time_text), body))
    }

    /// Closes the delete question without changing anything.
    pub fn cancel_checkpoint_delete(&mut self) {
        if self.state.pending_checkpoint_delete.take().is_some() {
            self.emit(ShellEvent::ProjectsChanged);
        }
    }

    /// Answers the delete question: queues the row's deletion and frees what
    /// nothing needs any more. The canvas, the edit session and the render
    /// are left alone.
    pub fn confirm_checkpoint_delete(&mut self) {
        let Some((tab_id, key)) = self.state.pending_checkpoint_delete.take() else {
            return;
        };
        self.emit(ShellEvent::ProjectsChanged);
        if self.state.active().is_none_or(|tab| tab.id != tab_id)
            || !self.queue_checkpoint_delete(&key)
        {
            return;
        }
        let scratch_root = self.scratch_root();
        let tab = self
            .state
            .active_mut()
            .expect("the delete was queued on the active tab");
        tab.free_unneeded_held();
        if checkpoints::held_index(&key).is_none() {
            tab.remove_cached_planes(&scratch_root, &key);
        }
    }

    /// [`Self::snapshot_live`] on the active tab; `false` with none active.
    pub fn save_checkpoint(&mut self) -> bool {
        self.active_project()
            .is_some_and(|index| self.snapshot_live(index))
    }

    /// `save_project_at` on the active tab; `false` with none active.
    pub fn start_save_active_project(
        &mut self,
        pick: impl FnOnce(&Path) -> Option<PathBuf>,
    ) -> bool {
        self.active_project()
            .is_some_and(|index| self.save_project_at(index, pick))
    }

    /// [`Self::start_save_active_project`] run to completion on the calling
    /// thread: the entry for headless runs and tests. Returns whether the
    /// save started and wrote.
    pub fn save_active_project(&mut self, pick: impl FnOnce(&Path) -> Option<PathBuf>) -> bool {
        let started = self.start_save_active_project(pick);
        self.finish_saves() && started
    }

    /// Starts writing tab `index`'s in-memory checkpoints and pending deletes
    /// into one `.ffp` on a worker, after keeping a modified live as a
    /// checkpoint. A never-saved tab asks `pick` (proposing
    /// `<frames dir>/<group name>.ffp`); an opened one rewrites its own file.
    /// [`Self::settle_save`] applies the result. `false` with nothing to
    /// write, a declined pick, a refused snapshot or another save running.
    fn save_project_at(
        &mut self,
        index: usize,
        pick: impl FnOnce(&Path) -> Option<PathBuf>,
    ) -> bool {
        let _scope = measurement_scope("gui_save_project");
        if self.save_in_flight() {
            return false;
        }
        let Some(tab) = self.state.projects.get(index) else {
            return false;
        };
        if !tab.has_unsaved_write() {
            return false;
        }
        let Some(target) = save_target(tab, pick) else {
            return false;
        };
        let id = tab.id;
        matches!(self.begin_save(id, target), SaveStart::Started)
    }

    /// Spawns the worker that writes tab `id` to `target` over a snapshot
    /// taken now. The tab counts as saving from here on, so its history
    /// stays as the worker reads it.
    fn begin_save(&mut self, id: u64, target: PathBuf) -> SaveStart {
        let Some(index) = self.tab_index(id) else {
            return SaveStart::Skipped;
        };
        let tab = &self.state.projects[index];
        if !tab.has_unsaved_write() {
            return SaveStart::Skipped;
        }
        if tab.live_modified() && !self.snapshot_live(index) {
            self.report_failed_save(
                index,
                &target,
                "the unsaved edits could not be kept as a checkpoint".to_string(),
            );
            return SaveStart::Failed;
        }
        let tab = &self.state.projects[index];
        let held = tab.held.clone();
        let deletes = tab.pending_deletes.clone();
        let keep = tab.row_parent();
        let onto_existing = matches!(tab.project, GuiProject::Opened(_));
        let outcome = Arc::clone(&self.state.save.outcome);
        let sink = self.sink.clone();
        let (path, worker_keep) = (target.clone(), keep.clone());
        let worker = std::thread::spawn(move || {
            let result =
                job::save_in_memory(&path, &held, &deletes, worker_keep.as_ref(), onto_existing)
                    .map_err(|err| err.to_string());
            *outcome
                .lock()
                .expect("no thread panicked while holding the save outcome") = Some(result);
            if let Some(sink) = &sink {
                sink(ShellEvent::SaveReady);
            }
        });
        self.state.save.running = Some(RunningSave {
            tab_id: id,
            target,
            keep,
            worker: Some(worker),
        });
        SaveStart::Started
    }

    /// Starts the next queued save that has something to write; `Skipped`
    /// when none is left.
    fn start_next_save(&mut self) -> SaveStart {
        while let Some((id, target)) = self.state.save.queue.pop_front() {
            match self.begin_save(id, target) {
                SaveStart::Skipped => {}
                started_or_failed => return started_or_failed,
            }
        }
        SaveStart::Skipped
    }

    /// Starts the next queued save or -- with none left -- carries out what
    /// the last save was for. `false` when a queued save could not start; the
    /// rest of the queue and the close or quit that waited are dropped.
    fn advance_saves(&mut self) -> bool {
        match self.start_next_save() {
            SaveStart::Started => return true,
            SaveStart::Failed => {
                self.drop_pending_saves();
                return false;
            }
            SaveStart::Skipped => {}
        }
        match std::mem::take(&mut self.state.save.then) {
            AfterSave::Nothing => {}
            AfterSave::CloseTab(id) => {
                let Some(index) = self.tab_index(id) else {
                    return true;
                };
                if self.state.projects[index].loses_on_close() {
                    if !self.state.pending_quit {
                        self.state.pending_close = Some(id);
                    }
                    self.emit(ShellEvent::ProjectsChanged);
                } else {
                    self.remove_tab(index, Instant::now());
                }
            }
            AfterSave::Quit => {
                if self.state.projects.iter().any(ProjectTab::loses_on_close) {
                    if self.state.pending_close.is_none() {
                        self.state.pending_quit = true;
                    }
                    self.emit(ShellEvent::ProjectsChanged);
                } else {
                    self.remove_parked_runs();
                    self.emit(ShellEvent::WindowCloseRequested);
                }
            }
        }
        true
    }

    /// Forgets the queued saves and the close or quit that waited on them.
    fn drop_pending_saves(&mut self) {
        self.state.save.queue.clear();
        self.state.save.then = AfterSave::Nothing;
    }

    /// Waits for the running save's worker to finish, without applying its
    /// result. The event loop never calls it: [`Self::finish_saves`] and
    /// tests do.
    pub fn join_save(&mut self) {
        if let Some(worker) = self
            .state
            .save
            .running
            .as_mut()
            .and_then(|running| running.worker.take())
        {
            worker.join().expect("the save worker did not panic");
        }
    }

    /// Joins and settles the running save and every save queued behind it,
    /// then returns whether each one wrote. Blocks: the way headless runs,
    /// tests and the end of a session wait out a write.
    pub fn finish_saves(&mut self) -> bool {
        let mut wrote = true;
        while self.save_in_flight() {
            self.join_save();
            wrote &= self
                .settle_save()
                .expect("a joined save worker parked its outcome")
        }
        wrote
    }

    /// Applies the result a save worker parked, never blocking: a write that
    /// succeeded opens the tab on its file and clears what it wrote; a failed
    /// one leaves everything in memory, notes why, and drops the saves and
    /// the close or quit that waited on it. `None` while no result is
    /// parked; otherwise whether the write succeeded and the save queued
    /// behind it, if any, started.
    pub fn settle_save(&mut self) -> Option<bool> {
        let result = self
            .state
            .save
            .outcome
            .lock()
            .expect("no thread panicked while holding the save outcome")
            .take()?;
        let running = self
            .state
            .save
            .running
            .take()
            .expect("a parked save result belongs to a running save");
        let index = self.tab_index(running.tab_id);
        let (project, ids) = match result {
            Ok(saved) => saved,
            Err(reason) => {
                if let Some(index) = index {
                    self.report_failed_save(index, &running.target, reason);
                    self.emit(ShellEvent::CheckpointsChanged);
                }
                self.drop_pending_saves();
                return Some(false);
            }
        };
        if let Some(index) = index {
            let keep = running.keep;
            let tab = &mut self.state.projects[index];
            let next = tab
                .selected_checkpoint
                .as_deref()
                .and_then(|row| tab.saved_row_key(row, &ids, keep.as_ref()));
            if let Some((row, generation)) = tab.fresh_checkpoint.clone()
                && let Some(saved) = tab.saved_row_key(&row, &ids, keep.as_ref())
            {
                tab.fresh_checkpoint = Some((saved, generation));
            }
            tab.selected_checkpoint =
                next.or_else(|| project.history().visible().first().map(|c| c.id.clone()));
            tab.row_planes = None;
            tab.project = GuiProject::Opened(Box::new(project));
            tab.title = entry::tab_title(&tab.project);
            remove_in_memory(&tab.held);
            tab.held.clear();
            tab.pending_deletes.clear();
            if tab.edit.is_none() {
                self.state.canvas_dirty = true;
            }
            // A session still opening read held files this save removed.
            let reopen = self.state.edits.opening(tab.id, tab.session_generation);
            if reopen {
                tab.session_generation += 1;
            }
            self.emit(ShellEvent::ProjectsChanged);
            if reopen && self.state.active == Some(index) {
                self.leave_editor_if_unfit();
            }
        }
        Some(self.advance_saves())
    }

    /// Records a failed save of tab `index` to `target` as a notification
    /// and the settled failure.
    fn report_failed_save(&mut self, index: usize, target: &Path, cause: String) {
        let tab = &self.state.projects[index];
        let notification = job::Notification {
            operation: job::FailedOperation::Save,
            message: format!("Could not save {}: {cause}", target.display()),
            project_title: Some(tab.title.clone()),
            project_path: Some(project_identity_path(&tab.project)),
            target: Some(target.to_path_buf()),
        };
        self.notifications.push(notification.clone());
        self.settled_failure = Some(notification);
    }

    /// What the open renderer cell should do for the frame about to draw,
    /// given which tab it currently holds (`None` before anything has
    /// loaded). `Keep` when the owner already matches the active tab and
    /// nothing changed underneath it since; otherwise `Load` at once when
    /// the active row's cached planes are fresh. A saved row not cached yet
    /// renders on the `RenderSlot` worker meanwhile, and an op-bearing row
    /// waits for its session the same way: `Keep` over that tab's previous
    /// image, or `Clear` when it has none (no tab active, `+` selected,
    /// nothing fused yet, or its current row refused). The feed never writes
    /// planes.
    pub fn canvas_feed(&mut self, loaded: Option<usize>) -> crate::canvas::CanvasFeed {
        let _scope = measurement_scope("gui_canvas_feed");
        if loaded == self.state.active && !self.state.canvas_dirty {
            return crate::canvas::CanvasFeed::Keep;
        }
        self.state.canvas_dirty = false;
        let Some(tab) = self.state.active else {
            return crate::canvas::CanvasFeed::Clear;
        };
        // An op-bearing row's pane waits for its session, so the base
        // pictures never show under the edited row's label.
        let opening_ops_row = self.state.active().is_some_and(|tab| {
            tab.on_ops_row() && self.state.edits.opening(tab.id, tab.session_generation)
        });
        let planes = if opening_ops_row {
            TabPlanes::Pending
        } else {
            self.active_tab_planes()
        };
        match planes {
            TabPlanes::Ready(composite, zmap) => crate::canvas::CanvasFeed::Load {
                tab,
                composite,
                zmap,
            },
            TabPlanes::Pending if loaded == Some(tab) => crate::canvas::CanvasFeed::Keep,
            TabPlanes::Pending | TabPlanes::None => crate::canvas::CanvasFeed::Clear,
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

/// `"1 checkpoint"` / `"N checkpoints"` -- the open card's meta line's own
/// wording for a non-zero count.
fn checkpoint_phrase(count: usize) -> String {
    let word = if count == 1 {
        "checkpoint"
    } else {
        "checkpoints"
    };
    format!("{count} {word}")
}

/// The staging key and the directory under `scratch_root` where the next
/// fused in-memory checkpoint on `tab` holds its files.
fn hold_dir_for(tab: &ProjectTab, scratch_root: &Path) -> (String, PathBuf) {
    let project_path = match &tab.project {
        GuiProject::Unsaved(group) => group.dir.join(format!("{}.ffp", group.name)),
        GuiProject::Opened(project) => project.path().to_path_buf(),
    };
    let staging_key = format!("{}-{:x}", tab.title, path_digest(&project_path));
    let hold_dir = scratch_root
        .join("held")
        .join(&staging_key)
        .join(format!("fused-{}", tab.checkpoint_count()));
    (staging_key, hold_dir)
}

/// Removes the directory `file` sits in, and the tab's directory above it
/// once that is empty.
fn remove_hold_dir_of(file: &Path) {
    if let Some(dir) = file.parent() {
        let _ = std::fs::remove_dir_all(dir);
        if let Some(tab_dir) = dir.parent() {
            // Fails, and so keeps the directory, while anything is left.
            let _ = std::fs::remove_dir(tab_dir);
        }
    }
}

/// Removes each held checkpoint's own directory.
fn remove_held(held: &[job::HeldCheckpoint]) {
    for checkpoint in held {
        remove_hold_dir_of(&checkpoint.composite);
    }
}

/// [`remove_held`] for every in-memory entry that keeps files: a fused
/// entry's planes.
fn remove_in_memory(entries: &[job::InMemoryCheckpoint]) {
    for entry in entries {
        match &entry.kind {
            job::InMemoryKind::Fused(held) => remove_hold_dir_of(&held.composite),
            job::InMemoryKind::Ops { .. } => {}
        }
    }
}

/// The selected row's own kind, algorithm, created-at, EXIF/ICC reference
/// index and composite description -- everything [`export_dialog_for`] and
/// [`export_source_for`] both need before either branches on whether the
/// tab has unsealed edits on top of it.
struct RowExportInfo {
    held: bool,
    /// An op-bearing in-memory row: exported from the edit
    /// session, never from a file.
    live: bool,
    algorithm: String,
    created_at: i64,
    reference: usize,
    description: String,
}

fn row_export_info(tab: &ProjectTab) -> Option<RowExportInfo> {
    let key = tab.selected_checkpoint.as_deref()?;
    let Some(index) = checkpoints::held_index(key) else {
        return saved_row_export_info(tab, key);
    };
    let entry = tab.held.get(index)?;
    let created_at = entry.created_at();
    if let Some(held) = fused_of(entry) {
        return Some(RowExportInfo {
            held: true,
            live: false,
            algorithm: held.algorithm.clone(),
            created_at,
            reference: held.align.as_ref().map_or(0, |a| a.reference),
            description: held.composite_description.clone(),
        });
    }
    let live = |algorithm: String, reference: usize, description: String| RowExportInfo {
        held: false,
        live: true,
        algorithm,
        created_at,
        reference,
        description,
    };
    match tab.resolve_row(key)? {
        RowBase::Held(base) => {
            let held = fused_of(tab.held.get(base)?)?;
            Some(live(
                held.algorithm.clone(),
                held.align.as_ref().map_or(0, |a| a.reference),
                held.composite_description.clone(),
            ))
        }
        RowBase::Saved(id) => {
            let info = saved_row_export_info(tab, &id)?;
            Some(live(info.algorithm, info.reference, info.description))
        }
    }
}

fn saved_row_export_info(tab: &ProjectTab, id: &str) -> Option<RowExportInfo> {
    let GuiProject::Opened(project) = &tab.project else {
        return None;
    };
    let checkpoint = project.history().get(id).ok()?;
    let record = project.fuse_state(id).ok()?;
    Some(RowExportInfo {
        held: false,
        live: false,
        algorithm: checkpoint.algorithm.clone(),
        created_at: checkpoint.created_at,
        reference: record.align.as_ref().map_or(0, |a| a.reference),
        description: record.composite.description.clone(),
    })
}

/// The export modal's own state for `tab`'s current position, `None` when
/// it has none.
fn export_dialog_for(tab: &ProjectTab, choice: &ExportChoice) -> Option<ExportDialog> {
    let editing = tab
        .edit
        .as_ref()
        .is_some_and(|edit| edit.session.op_count() > 0);
    let info = row_export_info(tab)?;
    let kind = if editing || info.live {
        ExportKind::Live
    } else if info.held {
        ExportKind::Held
    } else {
        ExportKind::Saved
    };
    let project_file = export_project_label(&tab.project);
    let algorithm = checkpoints::algorithm_label(&info.algorithm);
    let timestamp = checkpoints::local_date_and_time(info.created_at);
    let (context, what) = export_copy_for(kind, &project_file, &algorithm, &timestamp);
    Some(ExportDialog {
        tab_id: tab.id,
        context,
        what,
        stage: ExportStage::Ready,
        destination_name: String::new(),
        destination: None,
        choice: choice.clone(),
        open_menu: None,
    })
}

/// The export modal's own project-file name: an opened project's `.ffp`
/// file name with its extension, or a never-saved tab's own group name (no
/// extension -- there is no file yet).
fn export_project_label(project: &GuiProject) -> String {
    match project {
        GuiProject::Unsaved(group) => group.name.clone(),
        GuiProject::Opened(project) => project
            .path()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| project.path().display().to_string()),
    }
}

/// The worker's own export source for `tab`'s current position -- unsealed
/// edits over the selected row's Z-Map when the edit session holds any op,
/// otherwise the row itself, held or saved.
fn export_source_for(tab: &ProjectTab) -> Option<job::ExportSource> {
    let info = row_export_info(tab)?;
    if let Some(edit) = tab
        .edit
        .as_ref()
        .filter(|edit| edit.session.op_count() > 0 || info.live)
    {
        let (sources, registration) = tab.fused()?;
        let tile = edit.session.tile();
        return Some(job::ExportSource::Live {
            width: tile.width,
            height: tile.height,
            indices: edit.session.indices().to_vec(),
            sources,
            registration,
            reference: info.reference,
            description: info.description,
        });
    }
    if info.live {
        return None;
    }
    let key = tab.selected_checkpoint.as_deref()?;
    if info.held {
        let index = checkpoints::held_index(key)?;
        return Some(job::ExportSource::Held(
            fused_of(tab.held.get(index)?)?.clone(),
        ));
    }
    let GuiProject::Opened(project) = &tab.project else {
        return None;
    };
    Some(job::ExportSource::Saved {
        project_path: project.path().to_path_buf(),
        id: key.to_string(),
    })
}

/// The export save dialog's own starting point: `<project dir>/<tab
/// title>.<format's extension>` -- built from the tab's own title, not the
/// project file's name, so no extension of the identity path is ever
/// touched.
fn export_proposed_path(tab: &ProjectTab, format: ExportFormat) -> PathBuf {
    let dir = match &tab.project {
        GuiProject::Unsaved(group) => group.dir.clone(),
        GuiProject::Opened(project) => project
            .path()
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
    };
    dir.join(format!(
        "{}.{}",
        tab.title,
        crate::picker::default_extension(format)
    ))
}

/// Where tab `tab` saves to: an opened project's own file, or for a
/// never-saved one whatever `pick` answers to the proposed path.
fn save_target(tab: &ProjectTab, pick: impl FnOnce(&Path) -> Option<PathBuf>) -> Option<PathBuf> {
    match &tab.project {
        GuiProject::Opened(project) => Some(project.path().to_path_buf()),
        GuiProject::Unsaved(group) => pick(&group.dir.join(format!("{}.ffp", group.name))),
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

/// [`checkpoints::op_summary`] with "unsaved" after the first count:
/// "2 unsaved brush strokes".
fn unsaved_ops<'a>(ops: impl IntoIterator<Item = &'a job::Op>) -> String {
    let summary = checkpoints::op_summary(ops);
    match summary.split_once(' ') {
        Some((count, rest)) => format!("{count} unsaved {rest}"),
        None => summary,
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
