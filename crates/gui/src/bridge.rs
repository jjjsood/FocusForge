// SPDX-License-Identifier: GPL-3.0-or-later

//! The Slint side of the seam, and the only file in this crate that names
//! `winit`. Owns the `AppController`, installs its observer sink, wires
//! every `AppWindow` callback as a one-line forward, and performs each
//! `ShellEvent` against the window.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::ComponentHandle;
use slint::Model as _;
use slint::winit_030::{CustomApplicationHandler, EventResult, WinitWindowAccessor, winit};
use winit::event_loop::ActiveEventLoop;
use winit::window::WindowId;

use crate::canvas::CanvasFeed;
use crate::controller::{
    AppController, ChromeKey, DockTab, GroupingQuestion, JobView, OpenAs, SettingsSection,
    ShellEvent, ShellMode,
};
use crate::diagnostics::DiagnosticsStatus;
use crate::generated::{
    AppWindow, FFManifestRow, ImageRowView as GeneratedImageRow,
    ProjectCardView as GeneratedProjectCard, ProjectTabView, RecentRowView as GeneratedRecentRow,
    SectionHeadView, SettingsRow as GeneratedSettingsRow,
};
use crate::gpu_canvas::CanvasRenderer;
use crate::input_map::{Button, Modifiers, PointerKind};
use crate::job;
use crate::settings_panel::{HeadingState, RowKind};
use crate::viewport::PaneRect;

/// `slint::platform::PointerEventButton` -> `input_map::Button` -- the only
/// place in the crate that names the Slint type.
fn button(b: slint::platform::PointerEventButton) -> Button {
    match b {
        slint::platform::PointerEventButton::Left => Button::Left,
        slint::platform::PointerEventButton::Middle => Button::Middle,
        slint::platform::PointerEventButton::Right => Button::Right,
        _ => Button::Other,
    }
}

fn modifiers(controller: &AppController, ctrl: bool, shift: bool, alt: bool) -> Modifiers {
    Modifiers {
        ctrl,
        shift,
        alt,
        space: controller.space_held(),
    }
}

/// `install`'s return: the controller handle so `src/main.rs` can read the
/// same state the window shows, and the GPU-owned renderer cell -- an
/// `Rc<RefCell<..>>`, not held by the `Send + Sync` sink,
/// because it holds an `Rc` and the GPU state `perform` (the sink's own
/// path) cannot touch.
pub struct Installed {
    pub controller: Arc<Mutex<AppController>>,
    /// The live run's own poll timer, recreated by every `Fuse stack`/`Retry`
    /// press (`start_job_timer`) and dropped -- which deactivates it --
    /// the moment a new one replaces it.
    pub job_timer: Rc<RefCell<Option<slint::Timer>>>,
    /// The failure toast's auto-dismiss timer, recreated every time the
    /// toast opens and dropped -- deactivating it -- the moment a new
    /// failure replaces it.
    pub toast_timer: Rc<RefCell<Option<slint::Timer>>>,
}

/// The single open renderer, and which tab's planes it holds -- `None`
/// before anything has loaded. Built once in `src/main.rs`, before any
/// window exists, and shared with the drop route as well as `install`'s own
/// callbacks, so every path that can load a project shares the one cell.
/// The renderer outlives a plane clear: an unfused tab still has one, for
/// its selected frame alone.
#[derive(Default)]
pub struct Canvas {
    renderer: Option<CanvasRenderer>,
    tab: Option<usize>,
    /// The frame reader's own refusal, kept until a different frame opens
    /// or the same one succeeds -- the left `FFPane`'s message line reads
    /// this.
    frame_error: Option<String>,
}

impl Canvas {
    /// Which tab's planes the renderer currently holds.
    pub fn tab(&self) -> Option<usize> {
        self.tab
    }

    /// The open frame reader's own refusal text, if the last [`Self::set_frame`]
    /// failed.
    pub fn frame_error(&self) -> Option<&str> {
        self.frame_error.as_deref()
    }

    /// Drops the planes and forgets the owning tab; the frame half is left
    /// alone.
    fn clear(&mut self) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.clear_planes();
        }
        self.tab = None;
    }

    /// The open renderer, creating one first when this tab has never had
    /// anything to draw. `None` only when the shared GPU device failed to
    /// open.
    fn renderer_mut(&mut self) -> Option<&mut CanvasRenderer> {
        if self.renderer.is_none() {
            let shared = focusforge_gpu::device::shared_device().ok()?;
            self.renderer = Some(CanvasRenderer::new(shared));
        }
        self.renderer.as_mut()
    }

    /// Opens `composite`/`zmap` as `tab`'s own planes, naming the failure on
    /// stderr rather than panicking: a plane the pipeline itself just wrote
    /// failing to reopen is an environment problem (a full disk, a
    /// permissions change), not a bug worth crashing the window over. `tab`
    /// is recorded even when the open fails, so an unreadable plane file is
    /// not retried on every gesture. Reuses the renderer's own frame half
    /// rather than replacing it, since it may already hold the tab's
    /// selected frame.
    fn load(&mut self, tab: usize, composite: &Path, zmap: &Path) {
        self.tab = Some(tab);
        let Some(renderer) = self.renderer_mut() else {
            return;
        };
        if let Err(err) = renderer.set_planes(composite, zmap) {
            eprintln!("failed to open the project's planes: {err}");
        }
    }

    /// Opens `path` as the left pane's source, creating the renderer first
    /// when this tab has never had planes to draw. A no-op when `path` is
    /// already open; `None` drops whatever frame was open. Keeps the
    /// reader's own refusal text on failure rather than a generic message.
    pub fn set_frame(&mut self, path: Option<&Path>) {
        let Some(renderer) = self.renderer_mut() else {
            return;
        };
        let Some(path) = path else {
            renderer.clear_frame();
            self.frame_error = None;
            return;
        };
        if renderer.frame_path() == Some(path) {
            return;
        }
        match renderer.set_frame(path) {
            Ok(()) => self.frame_error = None,
            Err(err) => self.frame_error = Some(err.to_string()),
        }
    }
}

/// Reconciles the open renderer cell with the active tab
/// ([`AppController::canvas_feed`]) and the selected frame, then draws both
/// panes: the left pane's re-rendered frame with its own label and refusal
/// text, the right pane's re-rendered composite/Z-Map pair, each with its
/// own placement, plus `zoom-readout` and `project-loaded`. The one place a
/// gesture, an active-tab change or a completed run becomes a repaint. With
/// nothing to draw, a pane's source and placement are cleared rather than
/// left showing the previous tab.
pub fn repaint(
    app: &AppWindow,
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Canvas>>,
) {
    let feed = {
        let loaded = canvas.borrow().tab();
        controller.lock().unwrap().canvas_feed(loaded)
    };
    match feed {
        CanvasFeed::Keep => {}
        CanvasFeed::Clear => canvas.borrow_mut().clear(),
        CanvasFeed::Load {
            tab,
            composite,
            zmap,
        } => canvas.borrow_mut().load(tab, &composite, &zmap),
    }

    let (frame_path, frame_label, frame_draw, plane_draw, readout, project_loaded) = {
        let controller = controller.lock().unwrap();
        (
            controller.selected_frame_path(),
            controller.frame_pane_label(),
            controller.frame_draw(),
            controller.plane_draw(),
            controller.zoom_readout(),
            controller.active_project().is_some(),
        )
    };
    canvas.borrow_mut().set_frame(frame_path.as_deref());

    app.set_zoom_readout(readout.into());
    app.set_project_loaded(project_loaded);
    app.set_frame_label(frame_label.into());
    app.set_frame_message(canvas.borrow().frame_error().unwrap_or("").into());

    let frame_redrawn = frame_draw.and_then(|draw| {
        let mut canvas = canvas.borrow_mut();
        let renderer = canvas.renderer.as_mut()?;
        Some((draw, renderer.redraw_frame(&draw)?))
    });
    match frame_redrawn {
        Some((draw, frame_image)) => {
            app.set_frame_source(frame_image);
            app.set_frame_image_x(draw.dst.0);
            app.set_frame_image_y(draw.dst.1);
            app.set_frame_image_w(draw.dst.2);
            app.set_frame_image_h(draw.dst.3);
            app.set_pixelated(draw.pixelated);
        }
        None => {
            app.set_frame_source(slint::Image::default());
            app.set_frame_image_x(0.0);
            app.set_frame_image_y(0.0);
            app.set_frame_image_w(0.0);
            app.set_frame_image_h(0.0);
        }
    }

    let redrawn = plane_draw.and_then(|draw| {
        let mut canvas = canvas.borrow_mut();
        let renderer = canvas.renderer.as_mut()?;
        Some((draw, renderer.redraw_planes(&draw)?))
    });
    match redrawn {
        Some((draw, (composite_image, zmap_image))) => {
            app.set_composite_source(composite_image);
            app.set_zmap_source(zmap_image);
            app.set_image_x(draw.dst.0);
            app.set_image_y(draw.dst.1);
            app.set_image_w(draw.dst.2);
            app.set_image_h(draw.dst.3);
            app.set_pixelated(draw.pixelated);
        }
        None => {
            app.set_composite_source(slint::Image::default());
            app.set_zmap_source(slint::Image::default());
            app.set_image_x(0.0);
            app.set_image_y(0.0);
            app.set_image_w(0.0);
            app.set_image_h(0.0);
        }
    }
}

/// Locks `controller`, runs `mutate` against it, then repaints if the
/// window is still alive -- the "gesture becomes a redraw" tail shared by
/// every viewport callback below (`on_canvas_scrolled`, `on_canvas_pointer`,
/// `on_canvas_dragged`, `on_zoom_in_requested`, `on_zoom_out_requested`,
/// `on_zoom_fit_requested`).
fn dispatch(
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Canvas>>,
    app_weak: &slint::Weak<AppWindow>,
    mutate: impl FnOnce(&mut AppController),
) {
    mutate(&mut controller.lock().unwrap());
    if let Some(app) = app_weak.upgrade() {
        repaint(&app, controller, canvas);
    }
}

/// Reads `FFCanvas`'s five geometry `out` properties (mirrored at the root)
/// and pushes them into the controller as the two panes' current rects.
/// `pub` (the `main.rs` binary is a separate crate from this lib target) so
/// `src/main.rs`'s startup push reads the same properties through the same
/// function rather than re-listing them.
pub fn read_pane_geometry(app: &AppWindow) -> (PaneRect, PaneRect) {
    let y = app.get_canvas_pane_left_y();
    let width = app.get_canvas_pane_left_width();
    let height = app.get_canvas_pane_left_height();
    (
        PaneRect {
            x: app.get_canvas_pane_left_x(),
            y,
            width,
            height,
        },
        PaneRect {
            x: app.get_canvas_pane_right_x(),
            y,
            width,
            height,
        },
    )
}

/// Locks `controller`, runs `mutate` against it, then pushes the resulting
/// manifest scroll offset into the window if it's still alive -- the tail
/// shared by every manifest-scrollbar callback below.
fn dispatch_manifest_scroll(
    controller: &Arc<Mutex<AppController>>,
    app_weak: &slint::Weak<AppWindow>,
    mutate: impl FnOnce(&mut AppController),
) {
    let offset = {
        let mut controller = controller.lock().unwrap();
        mutate(&mut controller);
        controller.manifest_scroll_offset()
    };
    if let Some(app) = app_weak.upgrade() {
        app.set_diagnostics_manifest_scroll_offset(offset);
    }
}

/// Rebuilds the diagnostics panel's whole view from the controller and
/// pushes it into the window -- the panel carries no state of its own, so
/// every change that could affect it re-runs this rather than patching one
/// property.
pub fn push_diagnostics_view(app: &AppWindow, controller: &Arc<Mutex<AppController>>) {
    let view = controller.lock().unwrap().diagnostics_view();
    let rows: Vec<FFManifestRow> = view
        .entries
        .iter()
        .map(|entry| FFManifestRow {
            name: entry.name.clone().into(),
            value: entry.value.clone().into(),
            not_redacted: entry.not_redacted,
        })
        .collect();
    app.set_diagnostics_rows(Rc::new(slint::VecModel::from(rows)).into());
    app.set_diagnostics_note(view.note.unwrap_or_default().into());
    app.set_diagnostics_include_minidumps(view.include_minidumps);
    app.set_diagnostics_destination(view.destination.into());
    let status = match view.status {
        DiagnosticsStatus::Idle => "idle",
        DiagnosticsStatus::ExistingFile => "existing-file",
        DiagnosticsStatus::Writing => "writing",
        DiagnosticsStatus::Written => "written",
        DiagnosticsStatus::Failed => "failed",
    };
    app.set_diagnostics_status(status.into());
    app.set_diagnostics_message(view.message.into());
    app.set_diagnostics_primary_label(view.primary_label.into());
    app.set_diagnostics_busy(view.busy);
    app.set_diagnostics_loading(view.loading);
    app.set_diagnostics_manifest_scroll_offset(view.manifest_scroll_offset);
    app.set_diagnostics_manifest_content_height(view.manifest_content_height);
    app.set_diagnostics_destination_scroll_offset(view.destination_scroll_offset);
}

/// Pushes the crash notice's own two properties into the window -- mirrors
/// [`push_diagnostics_view`] for the toast.
pub fn push_crash_notice(app: &AppWindow, controller: &Arc<Mutex<AppController>>) {
    let controller = controller.lock().unwrap();
    app.set_crash_notice_visible(controller.crash_notice_visible());
    app.set_crash_notice_title(controller.crash_notice_text().into());
}

/// Scans for unreported crash evidence and pushes the notice, then --
/// only when it is showing -- arms its auto-dismiss timeout. The timer is
/// armed before `run()`, the same precedent `src/main.rs`'s own
/// `--exit-after` timer sets.
pub fn install_crash_notice(controller: &Arc<Mutex<AppController>>, app: &AppWindow) {
    controller.lock().unwrap().scan_crash_notice();
    push_crash_notice(app, controller);
    if !controller.lock().unwrap().crash_notice_visible() {
        return;
    }
    let timer_controller = controller.clone();
    let app_weak = app.as_weak();
    slint::Timer::single_shot(Duration::from_secs(10), move || {
        timer_controller.lock().unwrap().crash_notice_timed_out();
        if let Some(app) = app_weak.upgrade() {
            push_crash_notice(&app, &timer_controller);
        }
    });
}

/// Pushes the controller's project-tab state (`project_tabs`,
/// `tab_scroll_x`, the two scroll-enabled flags, `show_empty_state`,
/// `project_loaded`, `plus_selected`) into
/// `app` -- the one place `ShellEvent::ProjectsChanged` is performed. `pub`
/// so a test can call it directly without a real event loop (`install`'s
/// sink needs one; this function does not).
pub fn push_projects(app: &AppWindow, controller: &Arc<Mutex<AppController>>) {
    let controller = controller.lock().unwrap();
    push_tab_rows(app, &controller);
    push_tab_scroll(app, &controller);
    app.set_show_empty_state(controller.active_project().is_none());
    // The grouping question closes itself here rather than in its own
    // answer handler -- answering opens a project, which raises this same
    // `ProjectsChanged` push, so the dialog only needs to track whether a
    // question is still pending.
    app.set_question_open(controller.pending_question().is_some());
    // The dock's own gate for the tab row, the settings panel and the
    // "No project open." note (`ui/shell/dock.slint`) -- otherwise stuck at
    // whatever a canvas gesture (`repaint`, above) last set it to, since a
    // tab switch or close never fires one of its own.
    app.set_project_loaded(controller.active_project().is_some());
    app.set_plus_selected(controller.plus_selected());
    app.set_skipped_notice(controller.notice().unwrap_or_default().into());
    // The active tab may have changed with no `JobChanged`/poll event of its
    // own in between -- push its own run onto the bar right here rather than
    // leaving the words the previously active tab last wrote.
    push_job(app, &controller.job_view());
    push_settings_panel(app, &controller);
    push_images_panel(app, &controller);
    push_projects_overlay(app, &controller);
}

/// Pushes the tab strip's own rows -- title, unsaved marker, selection and
/// each tab's own run mark -- so a mark appears or clears the moment that
/// tab's run state changes, not only when the project list itself does.
/// Also pushes the toolbar's own preview of what a `Fuse stack` press on the
/// active tab would do right now, read off that tab's own row.
fn push_tab_rows(app: &AppWindow, controller: &AppController) {
    let rows = controller.project_tabs();
    let active = controller.active_project();
    let would_queue = active
        .and_then(|i| rows.get(i))
        .is_some_and(|row| row.would_queue);
    app.set_toolbar_would_queue(would_queue);
    let tabs: Vec<ProjectTabView> = rows
        .into_iter()
        .map(|t| ProjectTabView {
            title: t.title.into(),
            unsaved: t.unsaved,
            selected: t.selected,
            run_mark_icon: t.run_mark_icon.into(),
        })
        .collect();
    app.set_project_tabs(std::rc::Rc::new(slint::VecModel::from(tabs)).into());
}

/// Pushes the tab strip's scroll offset, overflow layout and two arrow
/// flags -- the one place `ShellEvent::TabScrollChanged` is performed.
fn push_tab_scroll(app: &AppWindow, controller: &AppController) {
    app.set_tab_scroll_x(controller.tab_scroll_x());
    app.set_tabs_overflow(controller.tabs_overflow());
    app.set_can_scroll_left(controller.can_scroll_left());
    app.set_can_scroll_right(controller.can_scroll_right());
}

/// How long the failure toast stays up before closing itself.
const TOAST_AUTO_DISMISS: Duration = Duration::from_secs(6);

/// Opens the failure toast with `message` and arms its auto-dismiss timer,
/// replacing any timer already running -- dropping the old one deactivates
/// it, so a second failure while the first toast is still up restarts the
/// count rather than closing early.
fn open_job_toast(
    app: &AppWindow,
    message: String,
    toast_timer: &Rc<RefCell<Option<slint::Timer>>>,
) {
    app.set_job_toast_message(message.into());
    app.set_job_toast_open(true);
    let timer = slint::Timer::default();
    let app_weak = app.as_weak();
    timer.start(
        slint::TimerMode::SingleShot,
        TOAST_AUTO_DISMISS,
        move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_job_toast_open(false);
            }
        },
    );
    *toast_timer.borrow_mut() = Some(timer);
}

/// Pushes the run's current view onto the toolbar/bottom-bar properties --
/// the one place `ShellEvent::JobChanged` and the poll timer both write.
fn push_job(app: &AppWindow, view: &JobView) {
    app.set_toolbar_run_state(view.toolbar_run_state.as_str().into());
    app.set_stage_word(view.stage_word.as_str().into());
    app.set_step_text(view.step_text.as_str().into());
    app.set_bar_fraction(view.fraction);
    app.set_bar_tone(view.bar_tone.as_str().into());
    app.set_eta_text(view.eta_text.as_str().into());
    app.set_fuse_enabled(view.fuse_enabled);
}

/// Installs a repeating timer, on the window's own clock ([`job::POLL`]),
/// that polls the live run's progress onto the toolbar/bottom-bar
/// properties, opens the failure toast the moment a run fails, and stops
/// itself once the run settles -- what `Fuse stack`/`Retry` starts, and the
/// only place [`AppController::settle_job`] is called from. A completed run
/// pushes the projects and repaints regardless of which tab it ran against;
/// `repaint`'s own reconcile decides whether its planes belong on screen.
fn start_job_timer(
    app_weak: slint::Weak<AppWindow>,
    controller: Arc<Mutex<AppController>>,
    canvas: Rc<RefCell<Canvas>>,
    job_timer: Rc<RefCell<Option<slint::Timer>>>,
    toast_timer: Rc<RefCell<Option<slint::Timer>>>,
) {
    let timer = slint::Timer::default();
    let timer_slot = job_timer.clone();
    timer.start(slint::TimerMode::Repeated, job::POLL, move || {
        let Some(app) = app_weak.upgrade() else {
            return;
        };
        let settled = controller.lock().unwrap().settle_job();
        if settled.is_some() {
            push_projects(&app, &controller);
            repaint(&app, &controller, &canvas);
        } else if let Some(message) = controller.lock().unwrap().take_settled_failure() {
            open_job_toast(&app, message, &toast_timer);
        }
        let view = controller.lock().unwrap().job_view();
        let live = controller.lock().unwrap().work_pending();
        push_job(&app, &view);
        push_projects_overlay(&app, &controller.lock().unwrap());
        if !live {
            // The run just settled -- the moment the images/settings panels'
            // own whole-panel disable must lift again.
            let controller = controller.lock().unwrap();
            push_settings_panel(&app, &controller);
            push_images_panel(&app, &controller);
        }
        if !live && let Some(t) = timer_slot.borrow().as_ref() {
            t.stop();
        }
    });
    *job_timer.borrow_mut() = Some(timer);
}

/// `RowKind`'s own string reading -- the settings panel's `.slint` side
/// branches on this, never on a `ParamKind` or an algorithm id.
fn row_kind_str(kind: RowKind) -> &'static str {
    match kind {
        RowKind::SectionHeading => "heading",
        RowKind::Picker => "picker",
        RowKind::LinearFloat => "linear-float",
        RowKind::LogFloat => "log-float",
        RowKind::Int => "int",
        RowKind::Bool => "bool",
        RowKind::Enum => "enum",
        RowKind::Error => "error",
        RowKind::ResetAll => "reset-all",
    }
}

/// `HeadingState`'s own string reading, matching `FFSectionHeading`'s
/// `provenance` property.
fn heading_state_str(state: HeadingState) -> &'static str {
    match state {
        HeadingState::Default => "default",
        HeadingState::Explicit => "explicit",
        HeadingState::Pending => "pending",
    }
}

/// Updates the settings panel's row model with `set_row_data` when the row
/// count did not change, instead of handing `app` a freshly built
/// `VecModel` every time: a fresh model resets the `for` loop's whole
/// element tree, which drops a `TouchArea` mid-gesture and kills a slider
/// drag after its first tick, where `set_row_data` keeps every element's
/// identity. A row count change (a tab switch, an algorithm pick) still
/// needs the fresh model, since the tree itself must grow or shrink. The
/// one place `ShellEvent::SettingsChanged` is performed; also called from
/// `push_projects` so a tab switch or close rebuilds the panel for the
/// newly active tab. `pub` so a test can push a real, unmodified row set
/// and then edit one row's own text without going through a live event.
pub fn push_settings_panel(app: &AppWindow, controller: &AppController) {
    let panel = controller.settings_panel();
    let rows: Vec<GeneratedSettingsRow> = panel
        .rows
        .iter()
        .map(|row| GeneratedSettingsRow {
            kind: row_kind_str(row.kind).into(),
            section: row.section.clone().into(),
            id: row.id.clone().into(),
            label: row.label.clone().into(),
            value_text: row.value_text.clone().into(),
            fraction: row.fraction,
            choices: std::rc::Rc::new(slint::VecModel::from(
                row.choices
                    .iter()
                    .map(|s| s.clone().into())
                    .collect::<Vec<slint::SharedString>>(),
            ))
            .into(),
            choice_ids: std::rc::Rc::new(slint::VecModel::from(
                row.choice_ids
                    .iter()
                    .map(|s| s.clone().into())
                    .collect::<Vec<slint::SharedString>>(),
            ))
            .into(),
            selected: row.selected.clone().into(),
            menu_open: row.menu_open,
            heading_state: heading_state_str(row.heading_state).into(),
            top: row.top,
        })
        .collect();
    let existing = app.get_settings_rows();
    if existing.row_count() == rows.len() {
        for (i, row) in rows.into_iter().enumerate() {
            existing.set_row_data(i, row);
        }
    } else {
        app.set_settings_rows(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    }
    app.set_settings_content_height(panel.content_height);
    app.set_settings_scroll_y(panel.scroll_y);

    let sticky = panel.sticky.map(|i| &panel.rows[i]);
    app.set_settings_sticky(SectionHeadView {
        visible: sticky.is_some(),
        section: sticky
            .map(|r| r.section.as_str())
            .unwrap_or_default()
            .into(),
        label: sticky.map(|r| r.label.as_str()).unwrap_or_default().into(),
        provenance: sticky
            .map(|r| heading_state_str(r.heading_state))
            .unwrap_or("default")
            .into(),
        push: panel.sticky_push,
    });
    app.set_settings_enabled(!controller.job_running());
}

/// Pushes the `Images` panel's whole state -- [`push_settings_panel`]'s
/// twin.
pub fn push_images_panel(app: &AppWindow, controller: &AppController) {
    let panel = controller.images_panel();
    let rows: Vec<GeneratedImageRow> = panel
        .rows
        .iter()
        .map(|row| GeneratedImageRow {
            chip: slint::Color::from_rgb_u8(row.chip[0], row.chip[1], row.chip[2]),
            name: row.name.clone().into(),
            time_text: row.time_text.clone().into(),
            excluded: row.excluded,
            unresolved: row.unresolved,
            top: row.top,
            selected: row.selected,
        })
        .collect();
    let existing = app.get_images_rows();
    if existing.row_count() == rows.len() {
        for (i, row) in rows.into_iter().enumerate() {
            existing.set_row_data(i, row);
        }
    } else {
        app.set_images_rows(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    }
    app.set_images_header_text(panel.header_text.into());
    app.set_images_content_height(panel.content_height);
    app.set_images_scroll_y(panel.scroll_y);
    let sort = match controller.frame_sort() {
        Some(crate::images::FrameSort::Name) => "name",
        Some(crate::images::FrameSort::CaptureTime) => "capture-time",
        None => "",
    };
    app.set_images_sort(sort.into());
    app.set_images_sort_menu_open(controller.images_sort_menu_open());
    app.set_images_dragging_row(controller.dragging_row().map_or(-1, |i| i as i32));
    app.set_images_drag_offset(controller.drag_offset());
    app.set_images_drag_target_row(controller.drag_target_row().map_or(-1, |i| i as i32));
    app.set_images_enabled(!controller.job_running());

    match controller.pending_frame_change() {
        Some(copy) => {
            let secondary = copy.secondary_label();
            app.set_frame_warning_open(true);
            app.set_frame_warning_title(copy.title.into());
            app.set_frame_warning_body(copy.body.into());
            app.set_frame_warning_info(copy.info.into());
            app.set_frame_warning_confirm_label(copy.confirm_label.into());
            app.set_frame_warning_secondary_label(secondary.into());
        }
        None => app.set_frame_warning_open(false),
    }
}

/// A card picture, converted into an actual Slint image -- the one place
/// `crate::overlay::Thumbnail` crosses into `slint::Image`. A blank 1x1
/// image with no picture yet -- `FFProjectCard` only draws it when
/// `has-picture` is set, so its content never shows.
fn thumbnail_image(thumbnail: Option<&crate::overlay::Thumbnail>) -> slint::Image {
    let Some(thumbnail) = thumbnail else {
        return slint::Image::default();
    };
    let mut buffer =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(thumbnail.width, thumbnail.height);
    buffer.make_mut_bytes().copy_from_slice(&thumbnail.rgba);
    slint::Image::from_rgba8(buffer)
}

/// Pushes the projects overlay's whole state -- the one place
/// `ShellEvent::OverlayChanged` is performed.
pub fn push_projects_overlay(app: &AppWindow, controller: &AppController) {
    app.set_projects_overlay_open(controller.projects_overlay_open());
    app.set_gallery_heading(controller.gallery_heading().into());
    app.set_gallery_count_text(controller.gallery_count_text().into());
    let cards: Vec<GeneratedProjectCard> = controller
        .project_cards()
        .into_iter()
        .map(|c| GeneratedProjectCard {
            kind: c.kind.into(),
            current: c.current,
            unsaved: c.unsaved,
            fusing: c.fusing,
            fusing_fraction: c.fusing_fraction,
            project_name: c.project_name.into(),
            meta_text: c.meta_text.into(),
            headline: c.headline.into(),
            stage: c.stage.into(),
            reason: c.reason.into(),
            action_label: c.action_label.into(),
            has_picture: c.picture.is_some(),
            picture: thumbnail_image(c.picture.as_deref()),
            x: c.x,
            y: c.y,
        })
        .collect();
    app.set_project_cards(std::rc::Rc::new(slint::VecModel::from(cards)).into());
    app.set_gallery_content_height(controller.gallery_content_height_px());
    app.set_gallery_scroll_y(controller.gallery_scroll_y());

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let recent: Vec<GeneratedRecentRow> = controller
        .recent_rows(now)
        .into_iter()
        .map(|r| GeneratedRecentRow {
            name: r.name.into(),
            folder: r.folder.into(),
            date_text: r.date_text.into(),
        })
        .collect();
    app.set_recent_rows(std::rc::Rc::new(slint::VecModel::from(recent)).into());
}

/// Pushes a raised [`GroupingQuestion`] into `app`'s dialog properties.
fn push_question(app: &AppWindow, question: &GroupingQuestion) {
    app.set_question_open(true);
    app.set_question_frame_count(question.frame_count as i32);
    app.set_question_project_count(question.project_count as i32);
}

/// Winit-level drop observer, installed via
/// `BackendSelector::with_winit_custom_application_handler` -- the only
/// route to a file-manager drop. Buffers every `WindowEvent::DroppedFile`
/// and flushes the buffer into one
/// `open_paths` call in `about_to_wait`: one gesture is one open, whether it
/// delivered one folder path or three file paths.
pub struct DropHandler {
    controller: Arc<Mutex<AppController>>,
    canvas: Rc<RefCell<Canvas>>,
    app_handle: Rc<RefCell<Option<slint::Weak<AppWindow>>>>,
    pending: Vec<PathBuf>,
}

impl DropHandler {
    /// `controller` and `canvas` are cloned into this handler before any
    /// window exists (`src/main.rs`), and `app_handle` is filled in once
    /// `AppWindow::new` runs -- the handler is installed on the
    /// `BackendSelector` chain before that, so it cannot hold an
    /// `AppWindow` of its own at construction time.
    pub fn new(
        controller: Arc<Mutex<AppController>>,
        canvas: Rc<RefCell<Canvas>>,
        app_handle: Rc<RefCell<Option<slint::Weak<AppWindow>>>>,
    ) -> Self {
        DropHandler {
            controller,
            canvas,
            app_handle,
            pending: Vec::new(),
        }
    }
}

impl CustomApplicationHandler for DropHandler {
    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _winit_window: Option<&winit::window::Window>,
        _slint_window: Option<&slint::Window>,
        event: &winit::event::WindowEvent,
    ) -> EventResult {
        if let winit::event::WindowEvent::DroppedFile(path) = event {
            self.pending.push(path.clone());
        }
        EventResult::Propagate
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) -> EventResult {
        if !self.pending.is_empty() {
            let paths = std::mem::take(&mut self.pending);
            let _ = self.controller.lock().unwrap().open_paths(&paths);
            // `repaint` reconciles the canvas with whatever became active --
            // the same tail every other open path ends in, so a dropped
            // project's own planes replace whatever the canvas held before,
            // with no further gesture needed.
            if let Some(app) = self
                .app_handle
                .borrow()
                .as_ref()
                .and_then(slint::Weak::upgrade)
            {
                repaint(&app, &self.controller, &self.canvas);
            }
        }
        EventResult::Propagate
    }
}

/// Opens `path`, if the picker returned one, closes the projects overlay on
/// a successful open, and repaints -- the rail's `New project`/`Open
/// project…` actions and their `Ctrl+N`/`Ctrl+O` chrome-key twins share this
/// one path.
fn open_from_picker(
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Canvas>>,
    app_weak: &slint::Weak<AppWindow>,
    path: Option<PathBuf>,
) {
    let Some(path) = path else { return };
    dispatch(controller, canvas, app_weak, |controller| {
        if matches!(
            controller.open_paths(&[path]),
            crate::OpenOutcome::Opened(_)
        ) {
            controller.close_projects_overlay();
        }
    });
}

/// Wires every callback in `app` to `controller`, and returns [`Installed`]
/// so `main.rs` can read the same state the window shows. `controller` and
/// `canvas` are both parameters, not created here: `main.rs` builds both
/// first and clones them into the winit-level [`DropHandler`], which must
/// be installed on the `BackendSelector` chain before any window exists.
/// The sink wraps `slint::invoke_from_event_loop` so a worker thread's own
/// push and a callback's own event take the same path.
pub fn install(
    app: &AppWindow,
    controller: Arc<Mutex<AppController>>,
    canvas: Rc<RefCell<Canvas>>,
) -> Installed {
    let job_timer: Rc<RefCell<Option<slint::Timer>>> = Rc::new(RefCell::new(None));
    let toast_timer: Rc<RefCell<Option<slint::Timer>>> = Rc::new(RefCell::new(None));

    {
        let app_weak = app.as_weak();
        let controller_for_sink = controller.clone();
        controller.lock().unwrap().set_sink(Arc::new(move |event| {
            let app_weak = app_weak.clone();
            let controller = controller_for_sink.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                perform(&app, &controller, event);
            });
        }));
    }

    app.on_window_drag_requested({
        let controller = controller.clone();
        move || controller.lock().unwrap().tab_strip_pressed()
    });
    app.on_window_minimize_requested({
        let controller = controller.clone();
        move || controller.lock().unwrap().window_minimize()
    });
    app.on_window_maximize_toggled({
        let controller = controller.clone();
        move || controller.lock().unwrap().window_maximize_toggle()
    });
    app.on_window_close_requested({
        let controller = controller.clone();
        move || controller.lock().unwrap().window_close()
    });
    app.on_mode_selected({
        let controller = controller.clone();
        move |mode| {
            let mode = if mode.as_str() == "editor" {
                ShellMode::Editor
            } else {
                ShellMode::Stacking
            };
            controller.lock().unwrap().set_mode(mode);
        }
    });
    app.on_dock_tab_selected({
        let controller = controller.clone();
        move |tab| {
            let tab = if tab.as_str() == "settings" {
                DockTab::Settings
            } else {
                DockTab::Images
            };
            controller.lock().unwrap().select_dock_tab(tab);
        }
    });
    app.on_checkpoint_export_requested({
        let controller = controller.clone();
        move || controller.lock().unwrap().checkpoint_export_requested()
    });
    // The run's own transport. `fuse-requested`/`retry-requested` start the
    // poll timer that feeds the rest; `pause-or-resume-requested` and
    // `cancel-requested` need nothing extra -- the timer already running for
    // a live job keeps polling through a pause or a cancel.
    app.on_fuse_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let job_timer = job_timer.clone();
        let toast_timer = toast_timer.clone();
        let app_weak = app.as_weak();
        move || {
            let started = controller.lock().unwrap().start_fuse();
            if started {
                start_job_timer(
                    app_weak.clone(),
                    controller.clone(),
                    canvas.clone(),
                    job_timer.clone(),
                    toast_timer.clone(),
                );
            }
        }
    });
    app.on_pause_or_resume_requested({
        let controller = controller.clone();
        move || controller.lock().unwrap().pause_or_resume()
    });
    app.on_cancel_requested({
        let controller = controller.clone();
        move || controller.lock().unwrap().cancel_fuse()
    });
    app.on_retry_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let job_timer = job_timer.clone();
        let toast_timer = toast_timer.clone();
        let app_weak = app.as_weak();
        move || {
            let started = controller.lock().unwrap().retry_fuse();
            if started {
                start_job_timer(
                    app_weak.clone(),
                    controller.clone(),
                    canvas.clone(),
                    job_timer.clone(),
                    toast_timer.clone(),
                );
            }
        }
    });
    app.on_composite_undock_requested({
        let controller = controller.clone();
        move || {
            controller
                .lock()
                .unwrap()
                .undock_requested(crate::canvas::PaneSource::Composite)
        }
    });
    app.on_zmap_undock_requested({
        let controller = controller.clone();
        move || {
            controller
                .lock()
                .unwrap()
                .undock_requested(crate::canvas::PaneSource::ZMap)
        }
    });
    app.on_canvas_scrolled({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |delta_x, delta_y, x, y, ctrl, shift, alt| {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                let mods = modifiers(controller, ctrl, shift, alt);
                controller.wheel(delta_x, delta_y, x, y, mods);
            });
        }
    });
    app.on_canvas_pointer({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |btn, kind, x, y, ctrl, shift, alt| {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                let mods = modifiers(controller, ctrl, shift, alt);
                // `kind` is `ui/shell/canvas.slint`'s fold of `PointerEventKind`:
                // 0 = Down, 1 = Up or Cancel, 2 = Move. `Move` fires on every
                // mouse movement over the area, before `moved`, and carries no
                // motion of its own -- forwarded as `PointerKind::Move` rather
                // than folded into `Up`, which would end a live drag on the
                // first movement.
                let kind = match kind {
                    0 => PointerKind::Down,
                    2 => PointerKind::Move,
                    _ => PointerKind::Up,
                };
                controller.pointer(kind, button(btn), x, y, mods);
            });
        }
    });
    app.on_canvas_dragged({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |x, y| {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.pointer_moved(x, y);
            });
        }
    });
    app.on_canvas_resized({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            if let Some(app) = app_weak.upgrade() {
                let (left, right) = read_pane_geometry(&app);
                controller.lock().unwrap().set_pane_geometry(left, right);
                repaint(&app, &controller, &canvas);
            }
        }
    });
    app.on_zoom_in_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || dispatch(&controller, &canvas, &app_weak, AppController::zoom_in)
    });
    app.on_zoom_out_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || dispatch(&controller, &canvas, &app_weak, AppController::zoom_out)
    });
    app.on_zoom_fit_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || dispatch(&controller, &canvas, &app_weak, AppController::fit)
    });
    // The five chrome keys are consulted before the rebindable table, so
    // none of them can be shadowed by a user's `input.toml`
    // (`AppController::chrome_key`).
    app.on_canvas_key_pressed({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |text, ctrl, shift, alt| {
            let mods = {
                let controller = controller.lock().unwrap();
                modifiers(&controller, ctrl, shift, alt)
            };
            let chrome = controller.lock().unwrap().chrome_key(text.as_str(), mods);
            match chrome {
                ChromeKey::None => controller.lock().unwrap().key(true, text.as_str(), mods),
                ChromeKey::Consumed => {}
                ChromeKey::NewProject => {
                    open_from_picker(&controller, &canvas, &app_weak, crate::picker::pick_entry())
                }
                ChromeKey::OpenProject => open_from_picker(
                    &controller,
                    &canvas,
                    &app_weak,
                    crate::picker::pick_project(),
                ),
            }
        }
    });
    app.on_canvas_key_released({
        let controller = controller.clone();
        move |text, ctrl, shift, alt| {
            let mut controller = controller.lock().unwrap();
            let mods = modifiers(&controller, ctrl, shift, alt);
            controller.key(false, text.as_str(), mods);
        }
    });
    app.on_canvas_focus_changed({
        let controller = controller.clone();
        move || controller.lock().unwrap().clear_input_modifiers()
    });
    app.on_settings_open_requested({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move || {
            let open = controller.lock().unwrap().open_settings();
            if let Some(app) = app_weak.upgrade() {
                app.set_settings_open(open);
            }
        }
    });
    app.on_settings_close_requested({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move || {
            let open = controller.lock().unwrap().close_settings();
            if let Some(app) = app_weak.upgrade() {
                app.set_settings_open(open);
            }
        }
    });
    app.on_settings_section_selected({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |section| {
            let parsed = match section.as_str() {
                "general" => SettingsSection::General,
                "performance" => SettingsSection::Performance,
                "storage" => SettingsSection::Storage,
                "diagnostics" => SettingsSection::Diagnostics,
                "about" => SettingsSection::About,
                _ => SettingsSection::InputMap,
            };
            controller.lock().unwrap().select_settings_section(parsed);
            if let Some(app) = app_weak.upgrade() {
                app.set_settings_section(section);
            }
        }
    });
    app.on_diagnostics_minidumps_toggled({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |include| {
            // Detached: the rescan's own outcome reaches the window through
            // the sink, the same way `on_diagnostics_primary_clicked`'s
            // write does below.
            let _ = controller.lock().unwrap().set_include_minidumps(include);
            if let Some(app) = app_weak.upgrade() {
                push_diagnostics_view(&app, &controller);
            }
        }
    });
    app.on_diagnostics_destination_edited({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |text| {
            controller.lock().unwrap().set_destination(text.as_str());
            if let Some(app) = app_weak.upgrade() {
                push_diagnostics_view(&app, &controller);
            }
        }
    });
    app.on_diagnostics_destination_cursor_moved({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |cursor_x, viewport_width| {
            controller
                .lock()
                .unwrap()
                .diagnostics_destination_cursor_moved(cursor_x, viewport_width);
            if let Some(app) = app_weak.upgrade() {
                push_diagnostics_view(&app, &controller);
            }
        }
    });
    app.on_diagnostics_manifest_scrollbar_pointer({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |kind, y| {
            // `kind` is `ff_manifest_list.slint`'s fold of `PointerEventKind`,
            // the same convention `canvas-pointer` uses: 0 = Down, 1 = Up or
            // Cancel, 2 = Move.
            let kind = match kind {
                0 => PointerKind::Down,
                2 => PointerKind::Move,
                _ => PointerKind::Up,
            };
            dispatch_manifest_scroll(&controller, &app_weak, |c| {
                c.manifest_scrollbar_pointer(kind, y)
            });
        }
    });
    app.on_diagnostics_manifest_scrollbar_dragged({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |y| {
            dispatch_manifest_scroll(&controller, &app_weak, |c| c.manifest_scrollbar_dragged(y));
        }
    });
    app.on_diagnostics_manifest_viewport_height_changed({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |height| {
            dispatch_manifest_scroll(&controller, &app_weak, |c| {
                c.set_manifest_viewport_height(height)
            });
        }
    });
    app.on_diagnostics_manifest_scrolled({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |delta| {
            dispatch_manifest_scroll(&controller, &app_weak, |c| c.manifest_scrolled(delta));
        }
    });
    app.on_diagnostics_primary_clicked({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move || {
            // Detached: the write's own outcome reaches the window through
            // the sink, not through this handle. Every click starts a write
            // -- a finished bundle re-derives a fresh timestamped
            // destination first, so exporting again needs no extra click.
            let _ = controller.lock().unwrap().start_bundle_write();
            if let Some(app) = app_weak.upgrade() {
                push_diagnostics_view(&app, &controller);
            }
        }
    });

    app.on_crash_notice_dismissed({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move || {
            controller.lock().unwrap().dismiss_crash_notice();
            if let Some(app) = app_weak.upgrade() {
                push_crash_notice(&app, &controller);
            }
        }
    });
    app.on_crash_notice_review_requested({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move || {
            let open = controller.lock().unwrap().review_crashes();
            if let Some(app) = app_weak.upgrade() {
                app.set_settings_open(open);
                app.set_settings_section("diagnostics".into());
                push_crash_notice(&app, &controller);
                push_diagnostics_view(&app, &controller);
            }
        }
    });

    // The entry path: the picker is a one-line forward, and `open_paths`'s
    // own sink pushes every property this raises (`ShellEvent::ProjectsChanged`/
    // `QuestionRaised`/`EntryRefused`); `dispatch`'s own repaint is what feeds
    // the canvas for whichever tab the open landed on.
    app.on_open_folder_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            if let Some(path) = crate::picker::pick_entry() {
                dispatch(&controller, &canvas, &app_weak, |controller| {
                    let _ = controller.open_paths(&[path]);
                });
            }
        }
    });
    app.on_plus_clicked({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || dispatch(&controller, &canvas, &app_weak, AppController::select_plus)
    });
    app.on_tab_selected({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |i| {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.select_project(i as usize);
            })
        }
    });
    app.on_tab_closed({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |i| {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.close_project(i as usize);
            })
        }
    });
    app.on_tab_scroll_left({
        let controller = controller.clone();
        move || controller.lock().unwrap().scroll_tabs(-1)
    });
    app.on_tab_scroll_right({
        let controller = controller.clone();
        move || controller.lock().unwrap().scroll_tabs(1)
    });
    app.on_tab_region_width_changed({
        let controller = controller.clone();
        move |width| controller.lock().unwrap().set_tab_region_width(width)
    });
    // A `changed` handler never fires for the initial value, so the
    // viewport's first measurement is read here once.
    controller
        .lock()
        .unwrap()
        .set_tab_region_width(app.get_tab_region_width());
    app.on_question_open_as_one({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                let _ = controller.answer_question(OpenAs::OneProject);
            });
        }
    });
    app.on_question_open_as_many({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                let _ = controller.answer_question(OpenAs::NProjects);
            });
        }
    });
    app.on_question_cancelled({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move || {
            controller.lock().unwrap().cancel_question();
            if let Some(app) = app_weak.upgrade() {
                app.set_question_open(false);
            }
        }
    });
    app.on_refusal_pick_another_folder({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_refusal_open(false);
            }
            if let Some(path) = crate::picker::pick_entry() {
                dispatch(&controller, &canvas, &app_weak, |controller| {
                    let _ = controller.open_paths(&[path]);
                });
            }
        }
    });
    app.on_skipped_notice_dismissed({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move || {
            controller.lock().unwrap().dismiss_notice();
            if let Some(app) = app_weak.upgrade() {
                app.set_skipped_notice("".into());
            }
        }
    });
    app.on_job_toast_dismissed({
        let app_weak = app.as_weak();
        move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_job_toast_open(false);
            }
        }
    });

    // The settings panel: every callback is a one-line forward,
    // `AppController` alone decides what a row's own kind, section and id
    // mean.
    app.on_settings_param_changed({
        let controller = controller.clone();
        move |section, id, fraction| {
            controller
                .lock()
                .unwrap()
                .set_param_fraction(section.as_str(), id.as_str(), fraction);
        }
    });
    app.on_settings_param_text_edited({
        let controller = controller.clone();
        move |section, id, text| {
            controller
                .lock()
                .unwrap()
                .set_param_text(section.as_str(), id.as_str(), text.as_str());
        }
    });
    app.on_settings_algorithm_selected({
        let controller = controller.clone();
        move |section, id| {
            let mut controller = controller.lock().unwrap();
            if section.as_str() == "align" {
                controller.set_align_algorithm(id.as_str());
            } else {
                controller.set_fuse_algorithm(id.as_str());
            }
        }
    });
    app.on_settings_reset_all({
        let controller = controller.clone();
        move || controller.lock().unwrap().reset_all_sections()
    });
    app.on_settings_menu_toggled({
        let controller = controller.clone();
        move |section, id| {
            controller
                .lock()
                .unwrap()
                .toggle_param_menu_for(section.as_str(), id.as_str());
        }
    });
    app.on_settings_scrolled({
        let controller = controller.clone();
        move |delta| controller.lock().unwrap().scroll_settings_panel(delta)
    });
    app.on_settings_scrollbar_pointer({
        let controller = controller.clone();
        move |kind, y| {
            // `ui/components/ff_scrollbar.slint`'s own fold of
            // `PointerEventKind`, the same convention as `canvas.slint`'s:
            // 0 = Down, 1 = Up or Cancel, 2 = Move.
            let kind = match kind {
                0 => PointerKind::Down,
                2 => PointerKind::Move,
                _ => PointerKind::Up,
            };
            controller.lock().unwrap().scrollbar_pointer(kind, y);
        }
    });
    app.on_settings_scrollbar_dragged({
        let controller = controller.clone();
        move |y| controller.lock().unwrap().scrollbar_dragged(y)
    });
    app.on_settings_viewport_height_changed({
        let controller = controller.clone();
        move |height| {
            controller
                .lock()
                .unwrap()
                .set_settings_viewport_height(height);
        }
    });

    // The `Images` panel's own scroll -- `FFScrollbar`'s twin wiring above.
    app.on_images_scrolled({
        let controller = controller.clone();
        move |delta| controller.lock().unwrap().scroll_images_panel(delta)
    });
    app.on_images_scrollbar_pointer({
        let controller = controller.clone();
        move |kind, y| {
            let kind = match kind {
                0 => PointerKind::Down,
                2 => PointerKind::Move,
                _ => PointerKind::Up,
            };
            controller.lock().unwrap().images_scrollbar_pointer(kind, y);
        }
    });
    app.on_images_scrollbar_dragged({
        let controller = controller.clone();
        move |y| controller.lock().unwrap().images_scrollbar_dragged(y)
    });
    app.on_images_viewport_height_changed({
        let controller = controller.clone();
        move |height| {
            controller
                .lock()
                .unwrap()
                .set_images_viewport_height(height)
        }
    });
    app.on_images_add_frames_requested({
        let controller = controller.clone();
        move || {
            if let Some(paths) = crate::picker::pick_frames() {
                controller.lock().unwrap().add_frames(&paths);
            }
        }
    });
    app.on_images_sort_menu_toggled({
        let controller = controller.clone();
        move || controller.lock().unwrap().toggle_images_sort_menu()
    });
    app.on_images_sort_selected({
        let controller = controller.clone();
        move |id| {
            let sort = match id.as_str() {
                "capture-time" => crate::images::FrameSort::CaptureTime,
                _ => crate::images::FrameSort::Name,
            };
            controller.lock().unwrap().set_frame_sort(sort);
        }
    });
    app.on_images_frame_excluded_toggled({
        let controller = controller.clone();
        move |i| controller.lock().unwrap().toggle_frame_excluded(i as usize)
    });
    app.on_images_frame_selected({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |i| {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.select_frame(i as usize);
            });
        }
    });
    app.on_images_frame_remove_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |i| {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.remove_frame(i as usize);
            });
        }
    });
    app.on_frame_warning_cancelled({
        let controller = controller.clone();
        move || controller.lock().unwrap().cancel_frame_change()
    });
    app.on_frame_warning_confirmed({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.confirm_frame_change();
            });
        }
    });
    app.on_frame_warning_secondary_pressed({
        let controller = controller.clone();
        move || {
            let mut controller = controller.lock().unwrap();
            let exclude = controller
                .pending_frame_change()
                .is_some_and(|copy| copy.exclude_instead_button);
            if exclude {
                controller.exclude_instead();
            } else {
                controller.new_project_instead();
            }
        }
    });
    // The projects overlay: the tab strip's own toggle, the gallery's
    // scroll -- `FFScrollbar`'s twin wiring above -- and its cards' own
    // click-through.
    app.on_projects_overlay_toggled({
        let controller = controller.clone();
        move || controller.lock().unwrap().toggle_projects_overlay()
    });
    // The rail's own actions: New/Open reuse the picker any other open path
    // uses; Close closes whichever tab is active, the same as `Ctrl+W` would.
    app.on_projects_new_project_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || open_from_picker(&controller, &canvas, &app_weak, crate::picker::pick_entry())
    });
    app.on_projects_open_project_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            open_from_picker(
                &controller,
                &canvas,
                &app_weak,
                crate::picker::pick_project(),
            )
        }
    });
    app.on_projects_close_project_requested({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move || {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                if let Some(i) = controller.active_project() {
                    controller.close_project(i);
                }
            });
        }
    });
    app.on_projects_card_clicked({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |i| {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.select_gallery_card(i as usize);
            })
        }
    });
    app.on_projects_card_close_clicked({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_weak = app.as_weak();
        move |i| {
            dispatch(&controller, &canvas, &app_weak, |controller| {
                controller.close_project(i as usize);
            })
        }
    });
    // `Run again` re-runs that one card's own kept request off the worker
    // thread the same way `fuse-requested`/`retry-requested` do; `Locate
    // frames…` needs the folder picker, which only this event-loop-thread
    // callback can call.
    app.on_projects_card_action_clicked({
        let controller = controller.clone();
        let canvas = canvas.clone();
        let job_timer = job_timer.clone();
        let toast_timer = toast_timer.clone();
        let app_weak = app.as_weak();
        move |i| {
            let i = i as usize;
            let kind = controller.lock().unwrap().gallery_failure_kind(i);
            match kind {
                Some("sources-missing") => {
                    if let Some(folder) = crate::picker::pick_entry() {
                        controller.lock().unwrap().locate_missing_sources(i, folder);
                    }
                }
                Some("failed") | Some("cancelled") => {
                    let started = controller.lock().unwrap().run_gallery_action(i);
                    if started {
                        start_job_timer(
                            app_weak.clone(),
                            controller.clone(),
                            canvas.clone(),
                            job_timer.clone(),
                            toast_timer.clone(),
                        );
                    }
                }
                _ => {}
            }
        }
    });
    app.on_gallery_scrolled({
        let controller = controller.clone();
        move |delta| controller.lock().unwrap().scroll_gallery(delta)
    });
    app.on_gallery_scrollbar_pointer({
        let controller = controller.clone();
        move |kind, y| {
            let kind = match kind {
                0 => PointerKind::Down,
                2 => PointerKind::Move,
                _ => PointerKind::Up,
            };
            controller
                .lock()
                .unwrap()
                .gallery_scrollbar_pointer(kind, y);
        }
    });
    app.on_gallery_scrollbar_dragged({
        let controller = controller.clone();
        move |y| controller.lock().unwrap().gallery_scrollbar_dragged(y)
    });
    app.on_gallery_viewport_height_changed({
        let controller = controller.clone();
        move |height| {
            controller
                .lock()
                .unwrap()
                .set_gallery_viewport_height(height)
        }
    });
    app.on_gallery_width_changed({
        let controller = controller.clone();
        let app_weak = app.as_weak();
        move |width| {
            controller.lock().unwrap().set_gallery_width(width);
            if let Some(app) = app_weak.upgrade() {
                push_projects_overlay(&app, &controller.lock().unwrap());
            }
        }
    });
    // A `changed` handler never fires for the initial value -- the
    // gallery's first measurement is read here once, the same reason
    // `tab_region_width` is (above).
    controller
        .lock()
        .unwrap()
        .set_gallery_width(app.get_gallery_width());

    app.on_images_frame_grip_pointer({
        let controller = controller.clone();
        move |i, kind, y| {
            let kind = match kind {
                0 => PointerKind::Down,
                2 => PointerKind::Move,
                _ => PointerKind::Up,
            };
            controller
                .lock()
                .unwrap()
                .frame_drag_pointer(i as usize, kind, y);
        }
    });

    Installed {
        controller,
        job_timer,
        toast_timer,
    }
}

/// Performs one [`ShellEvent`] against the window. The only function in the
/// crate that names `winit`.
fn perform(app: &AppWindow, controller: &Arc<Mutex<AppController>>, event: ShellEvent) {
    match event {
        ShellEvent::WindowDragRequested => {
            // Both results discarded deliberately: `with_winit_window`
            // returns `None` off the winit backend or an inactive event
            // loop, and a refused compositor move is a no-op, not an error
            // to surface. Never `unwrap` either.
            app.window().with_winit_window(|w| {
                let _ = w.drag_window();
            });
        }
        // `minimized`/`maximized` are native-window state, reached through
        // the runtime `Window` API rather than as generated root-property
        // accessors (`i-slint-core` 1.17.1's `Window` runtime API).
        ShellEvent::WindowMinimizeRequested => app.window().set_minimized(true),
        ShellEvent::WindowMaximizeToggled(maximized) => {
            app.window().set_maximized(maximized);
            // `AppWindow::window-maximized` is a plain `in` property, not the
            // builtin (shadowable) `Window.maximized` -- see step 8's Notes
            // on why native window state needs `app.window()` while chrome
            // state the shell *draws from* needs a custom property. This is
            // what the maximize glyph binding (`ui/shell/tab_strip.slint`)
            // reads to swap `square.svg` for `copy.svg`.
            app.set_window_maximized(maximized);
        }
        ShellEvent::WindowCloseRequested => {
            let _ = app.window().hide();
        }
        ShellEvent::ModeChanged(mode) => {
            let mode = match mode {
                ShellMode::Stacking => "stacking",
                ShellMode::Editor => "editor",
            };
            app.set_mode(mode.into());
        }
        ShellEvent::DockTabSelected(tab) => {
            let tab = match tab {
                DockTab::Images => "images",
                DockTab::Settings => "settings",
            };
            app.set_dock_tab(tab.into());
        }
        // Never reachable while `export_enabled()` is always `false` --
        // but the window has nothing to do with it either way.
        ShellEvent::CheckpointExportRequested => {}
        // Never reachable while `undock_enabled()` is always `true` with
        // both panes docked.
        ShellEvent::PaneUndockRequested(_) => {}
        // A no-op here regardless of `Action::implemented()`: the viewport
        // mutation itself already happened synchronously inside
        // `AppController::emit_action` before this event was even raised,
        // and the repaint it needs runs on the
        // *callback* path (`repaint`, above), not through the sink --
        // `perform` runs off a `Send + Sync` closure that can hold neither
        // an `Rc` nor the GPU state `CanvasRenderer` needs.
        ShellEvent::Input(_) => {}
        // A write's outcome updates the controller's own state first -- the
        // thread that produced it holds no lock on `self` -- then rebuilds
        // the view the same way every other diagnostics mutation does.
        ShellEvent::DiagnosticsWriteFinished(status) => {
            controller.lock().unwrap().finish_bundle_write(status);
            push_diagnostics_view(app, controller);
        }
        // A scan's outcome updates the controller's stored manifest first,
        // the same way a write's outcome does above, then rebuilds the view
        // -- the one place `diagnostics_view` stops reporting `loading`.
        ShellEvent::DiagnosticsScanFinished { generation, scan } => {
            controller
                .lock()
                .unwrap()
                .finish_manifest_scan(generation, scan);
            push_diagnostics_view(app, controller);
        }
        // The three entry-path events. `EntryRefused`'s
        // message is already rendered by `AppController::refuse`, never
        // formatted here.
        ShellEvent::ProjectsChanged => push_projects(app, controller),
        ShellEvent::TabScrollChanged => push_tab_scroll(app, &controller.lock().unwrap()),
        ShellEvent::QuestionRaised(question) => push_question(app, &question),
        ShellEvent::EntryRefused(_) => app.set_refusal_open(true),
        ShellEvent::SettingsChanged => push_settings_panel(app, &controller.lock().unwrap()),
        ShellEvent::JobChanged => {
            let controller = controller.lock().unwrap();
            push_job(app, &controller.job_view());
            // A tab's own run mark changes at the same moments: it starts,
            // queues, is promoted, or settles.
            push_tab_rows(app, &controller);
            // A run starting or ending is also the moment the images/settings
            // panels' own whole-panel disable must flip -- neither panel
            // otherwise re-reads `job_running()` on its own.
            push_settings_panel(app, &controller);
            push_images_panel(app, &controller);
            push_projects_overlay(app, &controller);
        }
        ShellEvent::ImagesChanged => push_images_panel(app, &controller.lock().unwrap()),
        ShellEvent::OverlayChanged => push_projects_overlay(app, &controller.lock().unwrap()),
    }
}
