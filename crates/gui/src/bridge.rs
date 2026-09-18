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

use crate::controller::{
    AppController, DockTab, GroupingQuestion, JobView, OpenAs, ShellEvent, ShellMode,
};
use crate::generated::{
    AppWindow, ProjectTabView, SectionHeadView, SettingsRow as GeneratedSettingsRow,
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
    pub canvas: Rc<RefCell<Option<CanvasRenderer>>>,
    /// The live run's own poll timer, recreated by every `Fuse stack`/`Retry`
    /// press (`start_job_timer`) and dropped -- which deactivates it --
    /// the moment a new one replaces it.
    pub job_timer: Rc<RefCell<Option<slint::Timer>>>,
    /// The failure toast's auto-dismiss timer, recreated every time the
    /// toast opens and dropped -- deactivating it -- the moment a new
    /// failure replaces it.
    pub toast_timer: Rc<RefCell<Option<slint::Timer>>>,
}

/// Reads `controller.pane_draw()`, asks `canvas` for the two re-rendered
/// images, and sets `composite-source`/`zmap-source`, the five placement
/// properties, `zoom-readout` and `project-loaded`. The one place a gesture
/// becomes a repaint -- every gesture callback below ends in a call to
/// this. A no-op while `canvas` is `None` (no project open yet).
pub fn repaint(
    app: &AppWindow,
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Option<CanvasRenderer>>>,
) {
    let (draw, readout, project_loaded) = {
        let controller = controller.lock().unwrap();
        (
            controller.pane_draw(),
            controller.zoom_readout(),
            controller.active_project().is_some(),
        )
    };
    app.set_zoom_readout(readout.into());
    app.set_project_loaded(project_loaded);

    let Some(draw) = draw else {
        return;
    };
    let mut canvas = canvas.borrow_mut();
    let Some(renderer) = canvas.as_mut() else {
        return;
    };
    let (composite_image, zmap_image) = renderer.redraw(&draw);
    app.set_composite_source(composite_image);
    app.set_zmap_source(zmap_image);
    app.set_image_x(draw.dst.0);
    app.set_image_y(draw.dst.1);
    app.set_image_w(draw.dst.2);
    app.set_image_h(draw.dst.3);
    app.set_pixelated(draw.pixelated);
}

/// Locks `controller`, runs `mutate` against it, then repaints if the
/// window is still alive -- the "gesture becomes a redraw" tail shared by
/// every viewport callback below (`on_canvas_scrolled`, `on_canvas_pointer`,
/// `on_canvas_dragged`, `on_zoom_in_requested`, `on_zoom_out_requested`,
/// `on_zoom_fit_requested`).
fn dispatch(
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Option<CanvasRenderer>>>,
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

/// Pushes the controller's project-tab state (`project_tabs`,
/// `tab_scroll_x`, the two scroll-enabled flags, `show_empty_state`,
/// `project_loaded`, `plus_selected`) into
/// `app` -- the one place `ShellEvent::ProjectsChanged` is performed. `pub`
/// so a test can call it directly without a real event loop (`install`'s
/// sink needs one; this function does not).
pub fn push_projects(app: &AppWindow, controller: &Arc<Mutex<AppController>>) {
    let controller = controller.lock().unwrap();
    let tabs: Vec<ProjectTabView> = controller
        .project_tabs()
        .into_iter()
        .map(|t| ProjectTabView {
            title: t.title.into(),
            unsaved: t.unsaved,
            selected: t.selected,
        })
        .collect();
    app.set_project_tabs(std::rc::Rc::new(slint::VecModel::from(tabs)).into());
    push_tab_scroll(app, &controller);
    app.set_show_empty_state(controller.active_project().is_none());
    // The dock's own gate for the tab row, the settings panel and the
    // "No project open." note (`ui/shell/dock.slint`) -- otherwise stuck at
    // whatever a canvas gesture (`repaint`, above) last set it to, since a
    // tab switch or close never fires one of its own.
    app.set_project_loaded(controller.active_project().is_some());
    app.set_plus_selected(controller.plus_selected());
    app.set_skipped_notice(controller.notice().unwrap_or_default().into());
    push_settings_panel(app, &controller);
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
}

/// Opens `CanvasRenderer` on a project's two materialized planes, naming the
/// failure on stderr rather than panicking: a plane the pipeline itself just
/// wrote failing to reopen is an environment problem (a full disk, a
/// permissions change), not a bug worth crashing the window over.
fn open_canvas(canvas: &Rc<RefCell<Option<CanvasRenderer>>>, composite: &Path, zmap: &Path) {
    let Ok(shared) = focusforge_gpu::device::shared_device() else {
        return;
    };
    match CanvasRenderer::open(shared, composite, zmap) {
        Ok(renderer) => *canvas.borrow_mut() = Some(renderer),
        Err(err) => eprintln!("failed to open the project's planes: {err}"),
    }
}

/// Materializes the active tab's own planes (the `--open <saved>.ffp` path,
/// with no run involved) and feeds the canvas -- a no-op for an `Unsaved`
/// tab, which [`AppController::materialize_active_tab_planes`] already
/// refuses.
pub fn feed_canvas(
    app: &AppWindow,
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Option<CanvasRenderer>>>,
) {
    let planes = controller.lock().unwrap().materialize_active_tab_planes();
    if let Some((composite, zmap)) = planes {
        open_canvas(canvas, &composite, &zmap);
        repaint(app, controller, canvas);
    }
}

/// Installs a repeating timer, on the window's own clock ([`job::POLL`]),
/// that polls the live run's progress onto the toolbar/bottom-bar
/// properties, feeds the canvas once a run completes, opens the failure
/// toast the moment a run fails, and stops itself once the run settles --
/// what `Fuse stack`/`Retry` starts, and the only place
/// [`AppController::settle_job`] is called from.
fn start_job_timer(
    app_weak: slint::Weak<AppWindow>,
    controller: Arc<Mutex<AppController>>,
    canvas: Rc<RefCell<Option<CanvasRenderer>>>,
    job_timer: Rc<RefCell<Option<slint::Timer>>>,
    toast_timer: Rc<RefCell<Option<slint::Timer>>>,
) {
    let timer = slint::Timer::default();
    let timer_slot = job_timer.clone();
    timer.start(slint::TimerMode::Repeated, job::POLL, move || {
        let Some(app) = app_weak.upgrade() else {
            return;
        };
        let planes = controller.lock().unwrap().settle_job();
        if let Some((composite, zmap)) = planes {
            open_canvas(&canvas, &composite, &zmap);
            push_projects(&app, &controller);
            repaint(&app, &controller, &canvas);
        } else if let Some(message) = controller
            .lock()
            .unwrap()
            .job_failure_message()
            .map(str::to_string)
        {
            open_job_toast(&app, message, &toast_timer);
        }
        let view = controller.lock().unwrap().job_view();
        let live = job::is_live(&view.state);
        push_job(&app, &view);
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
    pending: Vec<PathBuf>,
}

impl DropHandler {
    /// `controller` is cloned into this handler before any window exists
    /// (`src/main.rs`): the handler needs no `AppWindow` of its own --
    /// `AppController::open_paths`'s own sink is what pushes the resulting
    /// window properties, through `slint::invoke_from_event_loop`, exactly
    /// as a picker callback's own `open_paths` call does.
    pub fn new(controller: Arc<Mutex<AppController>>) -> Self {
        DropHandler {
            controller,
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
            // The controller's own sink pushes every resulting window
            // property (`ShellEvent::ProjectsChanged`/`QuestionRaised`/
            // `EntryRefused`) through `slint::invoke_from_event_loop` --
            // this call needs no repaint of its own.
            let _ = self.controller.lock().unwrap().open_paths(&paths);
        }
        EventResult::Propagate
    }
}

/// Wires every callback in `app` to `controller`, and returns [`Installed`]
/// so `main.rs` can read the same state the window shows and drive the same
/// GPU renderer cell. `controller` is a parameter, not created here:
/// `main.rs` builds it first and clones it into the
/// winit-level [`DropHandler`], which must be installed on the
/// `BackendSelector` chain before any window exists. The sink wraps
/// `slint::invoke_from_event_loop` so a background thread's push (a
/// finished diagnostics write) and a callback's own event take the same
/// path.
pub fn install(app: &AppWindow, controller: Arc<Mutex<AppController>>) -> Installed {
    let canvas: Rc<RefCell<Option<CanvasRenderer>>> = Rc::new(RefCell::new(None));
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
    app.on_canvas_key_pressed({
        let controller = controller.clone();
        move |text, ctrl, shift, alt| {
            let mut controller = controller.lock().unwrap();
            let mods = modifiers(&controller, ctrl, shift, alt);
            controller.key(true, text.as_str(), mods);
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

    // The entry path: the picker is a one-line forward, and `open_paths`'s
    // own sink pushes every property this raises (`ShellEvent::ProjectsChanged`/
    // `QuestionRaised`/`EntryRefused`), so this callback body needs no
    // return-value handling of its own.
    app.on_open_folder_requested({
        let controller = controller.clone();
        move || {
            if let Some(path) = crate::picker::pick_entry() {
                let _ = controller.lock().unwrap().open_paths(&[path]);
            }
        }
    });
    app.on_plus_clicked({
        let controller = controller.clone();
        move || controller.lock().unwrap().select_plus()
    });
    app.on_tab_selected({
        let controller = controller.clone();
        move |i| controller.lock().unwrap().select_project(i as usize)
    });
    app.on_tab_closed({
        let controller = controller.clone();
        move |i| controller.lock().unwrap().close_project(i as usize)
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
        move || {
            let _ = controller
                .lock()
                .unwrap()
                .answer_question(OpenAs::OneProject);
        }
    });
    app.on_question_open_as_many({
        let controller = controller.clone();
        move || {
            let _ = controller
                .lock()
                .unwrap()
                .answer_question(OpenAs::NProjects);
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
        let app_weak = app.as_weak();
        move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_refusal_open(false);
            }
            if let Some(path) = crate::picker::pick_entry() {
                let _ = controller.lock().unwrap().open_paths(&[path]);
            }
        }
    });
    app.on_skipped_notice_dismissed({
        let app_weak = app.as_weak();
        move || {
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

    Installed {
        controller,
        canvas,
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
        // The three entry-path events. `EntryRefused`'s
        // message is already rendered by `AppController::refuse`, never
        // formatted here.
        ShellEvent::ProjectsChanged => push_projects(app, controller),
        ShellEvent::TabScrollChanged => push_tab_scroll(app, &controller.lock().unwrap()),
        ShellEvent::QuestionRaised(question) => push_question(app, &question),
        ShellEvent::EntryRefused(_) => app.set_refusal_open(true),
        ShellEvent::SettingsChanged => push_settings_panel(app, &controller.lock().unwrap()),
        ShellEvent::JobChanged => push_job(app, &controller.lock().unwrap().job_view()),
    }
}
