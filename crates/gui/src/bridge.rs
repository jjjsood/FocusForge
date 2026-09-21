// SPDX-License-Identifier: GPL-3.0-or-later

//! The Slint side of the seam, and the only file in this crate that names
//! `winit`. Owns the `AppController`, installs its observer sink, wires
//! every `AppWindow` callback as a one-line forward, and performs each
//! `ShellEvent` against the window.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::ComponentHandle;
use slint::winit_030::WinitWindowAccessor;

use crate::controller::{AppController, DockTab, SettingsSection, ShellEvent, ShellMode};
use crate::diagnostics::DiagnosticsStatus;
use crate::generated::{AppWindow, FFManifestRow};
use crate::gpu_canvas::CanvasRenderer;
use crate::input_map::{Button, Modifiers, PointerKind};
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
}

/// Reads `controller.pane_draw()`, asks `canvas` for the two re-rendered
/// images, and sets `composite-source`/`zmap-source`, the five placement
/// properties and `zoom-readout`. The one place a gesture becomes a
/// repaint -- every gesture callback below ends in a call to this. A no-op
/// while `canvas` is `None` (no project open yet).
pub fn repaint(
    app: &AppWindow,
    controller: &Arc<Mutex<AppController>>,
    canvas: &Rc<RefCell<Option<CanvasRenderer>>>,
) {
    let (draw, readout) = {
        let controller = controller.lock().unwrap();
        (controller.pane_draw(), controller.zoom_readout())
    };
    app.set_zoom_readout(readout.into());
    // `pane_draw()` is `Some` only once a project's image and pane geometry
    // are both known -- exactly "is there anything to show chrome for":
    // without this flag the pane labels/undock controls and the zoom
    // cluster render as if live with nothing loaded.
    app.set_project_loaded(draw.is_some());

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

/// Wires every callback in `app` to a fresh [`AppController`], and returns
/// [`Installed`] so `main.rs` can read the same state the window shows and
/// drive the same GPU renderer cell. The sink wraps
/// `slint::invoke_from_event_loop` so a background thread's push (a
/// finished diagnostics write) and a callback's own event take the same
/// path.
pub fn install(app: &AppWindow) -> Installed {
    let controller = Arc::new(Mutex::new(AppController::new()));
    let canvas: Rc<RefCell<Option<CanvasRenderer>>> = Rc::new(RefCell::new(None));

    {
        let app_weak = app.as_weak();
        let sink_controller = controller.clone();
        controller.lock().unwrap().set_sink(Box::new(move |event| {
            let app_weak = app_weak.clone();
            let controller = sink_controller.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
                // A write's outcome updates the controller's own state
                // first -- the thread that produced it holds no lock on
                // `self` -- then rebuilds the view the same way every other
                // diagnostics mutation does.
                if let ShellEvent::DiagnosticsWriteFinished(status) = event {
                    controller.lock().unwrap().finish_bundle_write(status);
                    push_diagnostics_view(&app, &controller);
                    return;
                }
                // A scan's outcome updates the controller's stored manifest
                // first, the same way a write's outcome does above, then
                // rebuilds the view -- the one place `diagnostics_view`
                // stops reporting `loading`.
                if let ShellEvent::DiagnosticsScanFinished { generation, scan } = event {
                    controller
                        .lock()
                        .unwrap()
                        .finish_manifest_scan(generation, scan);
                    push_diagnostics_view(&app, &controller);
                    return;
                }
                perform(&app, event);
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

    Installed { controller, canvas }
}

/// Performs one [`ShellEvent`] against the window. The only function in the
/// crate that names `winit`.
fn perform(app: &AppWindow, event: ShellEvent) {
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
        // Intercepted before `perform` is ever called -- the sink installed
        // in `install` handles this variant itself, since it needs the
        // controller lock this function does not carry.
        ShellEvent::DiagnosticsWriteFinished(_) => {}
        // Intercepted the same way, for the same reason.
        ShellEvent::DiagnosticsScanFinished { .. } => {}
    }
}
