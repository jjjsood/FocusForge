// SPDX-License-Identifier: GPL-3.0-or-later

//! The Slint side of the seam, and the only file in this crate that names
//! `winit`. Owns the `AppController`, installs its observer sink, wires
//! every `AppWindow` callback as a one-line forward, and performs each
//! `ShellEvent` against the window.

use std::sync::{Arc, Mutex};

use slint::ComponentHandle;
use slint::winit_030::WinitWindowAccessor;

use crate::controller::{AppController, DockTab, ShellEvent, ShellMode};
use crate::generated::AppWindow;

/// Wires every callback in `app` to a fresh [`AppController`], and returns
/// the controller handle so `main.rs` can read the same state the window
/// shows. The sink wraps `slint::invoke_from_event_loop` so a worker
/// thread's push and a callback's own event take the same path.
pub fn install(app: &AppWindow) -> Arc<Mutex<AppController>> {
    let controller = Arc::new(Mutex::new(AppController::new()));

    {
        let app_weak = app.as_weak();
        controller.lock().unwrap().set_sink(Box::new(move |event| {
            let app_weak = app_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = app_weak.upgrade() else {
                    return;
                };
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

    controller
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
    }
}
