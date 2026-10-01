// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-gui`'s entry point. Every flag below exists so the window
//! can be observed without a person in front of it.

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use focusforge_gui::bridge::Canvas;
use focusforge_gui::{AppController, AppWindow, ThemeSheet, bridge, job, screenshot};
use slint::ComponentHandle;
use slint::wgpu_29::wgpu;

#[derive(Parser)]
#[command(name = "focusforge-gui")]
struct Args {
    /// Exit this many milliseconds after the window is shown.
    #[arg(long)]
    exit_after: Option<u64>,
    /// Write a screenshot of the window to this path, taken when
    /// `--exit-after` fires.
    #[arg(long)]
    screenshot: Option<PathBuf>,
    /// Resize the window to `WxH` shortly after it is shown.
    #[arg(long)]
    size: Option<String>,
    /// Renders the design-token specimen sheet instead of the shell.
    #[arg(long)]
    theme_sheet: bool,
    /// Opens a path the same way the picker or a drop would: a folder
    /// proposes a grouping, a `*.ffp` reopens a saved project
    /// (`AppController::open_paths`). Repeatable; each path is opened in
    /// order, one `open_paths` call per path.
    #[arg(long, action = clap::ArgAction::Append)]
    open: Vec<PathBuf>,
    /// Read and write the input map at `<DIR>/focusforge/settings.toml`'s
    /// `[input]` table instead of the platform config directory.
    #[arg(long)]
    config_dir: Option<PathBuf>,
    /// Opens the settings modal on the named section at startup:
    /// `input-map` or `diagnostics`.
    #[arg(long)]
    settings_section: Option<String>,
    /// Selects the dock's tab row at startup: `images` or `settings`.
    #[arg(long)]
    dock_tab: Option<String>,
    /// Sets the toolbar's transport to a named run state at startup, for
    /// inspecting a state no click sequence currently reaches: `idle`,
    /// `running`, `paused`, `cancelling` or `failed`.
    #[arg(long)]
    toolbar_state: Option<String>,
    /// Opens the export modal at startup with literal preview text, for
    /// inspecting a state no click sequence quickly reaches: `ready`,
    /// `writing` or `refused`.
    #[arg(long)]
    export_dialog_preview: Option<String>,
    /// Opens the `Images` panel's sort menu at startup, for inspecting it
    /// with no click sequence.
    #[arg(long)]
    images_sort_menu_open: bool,
    /// Opens the projects overlay at startup, for inspecting every state of
    /// this surface with no click sequence. With `--fuse-tabs`, it opens once
    /// the last run settles instead.
    #[arg(long)]
    projects_overlay: bool,
    /// Presses `Save project` once the last `--fuse-tabs` run settles,
    /// accepting the proposed path without a dialog -- for inspecting the
    /// saved state with no click sequence.
    #[arg(long, requires = "fuse_tabs")]
    save_active_project: bool,
    /// Asks to close the active tab once the last `--fuse-tabs` run settles,
    /// as `Ctrl+W` does -- for inspecting the close question with no key
    /// press.
    #[arg(long, requires = "fuse_tabs")]
    request_close: bool,
    /// Asks to quit the window once the last `--fuse-tabs` run settles, as
    /// the app-drawn window-close control does -- for inspecting the quit
    /// question with no click.
    #[arg(long, requires = "fuse_tabs")]
    request_quit: bool,
    /// Selects editor mode once the last `--fuse-tabs` run settles, as
    /// clicking the mode switch's `Editor` segment does -- for inspecting
    /// the editor surface with no click sequence.
    #[arg(long, requires = "fuse_tabs")]
    enter_editor: bool,
    /// Requests removing the frame at this index right after `--open`, for
    /// inspecting the frame-set warning it raises on a checkpointed project
    /// with no click sequence.
    #[arg(long)]
    frame_remove_warning: Option<usize>,
    /// Presses `Export image` on the active tab's current row once `--open`
    /// settles, confirming with the proposed destination -- a real export
    /// through `AppController`/`job::export_position`, not
    /// `--export-dialog-preview`'s literal text, for inspecting an actual
    /// refusal (or a real success) with no click sequence.
    #[arg(long)]
    export_active_row: bool,
    /// Selects these tab indices in turn and presses `Fuse stack` on each,
    /// right after `--open`, leaving the last one selected -- for
    /// inspecting a live run and the tabs queued behind it with no click
    /// sequence. Comma-separated. An index repeated waits for every run to
    /// settle before it is pressed again, so `0,0` fuses tab 0 twice.
    #[arg(long, value_delimiter = ',')]
    fuse_tabs: Vec<usize>,
}

const TOOLBAR_STATES: [&str; 5] = ["idle", "running", "paused", "cancelling", "failed"];

/// Sets the toolbar's and the bottom bar's transport preview together, from
/// literal values with no controller or job behind them -- for inspecting a
/// state no click sequence currently reaches.
fn apply_toolbar_state_preview(app: &AppWindow, state: &str) {
    app.set_toolbar_run_state(state.into());
    let (stage_word, step_text, fraction, tone, eta) = match state {
        "running" => ("Aligning", "42 of 61", 0.62, "running", "ETA 1:12"),
        "paused" => ("Paused", "", 0.62, "held", "ETA —"),
        "cancelling" => ("Cancelling", "", 0.0, "held", "ETA —"),
        "failed" => ("Failed", "", 0.28, "failed", "ETA —"),
        _ => ("", "", 0.0, "held", ""),
    };
    app.set_stage_word(stage_word.into());
    app.set_step_text(step_text.into());
    app.set_bar_fraction(fraction);
    app.set_bar_tone(tone.into());
    app.set_eta_text(eta.into());
}

/// Sets the export modal's own state directly, with no controller or
/// worker behind it -- `apply_toolbar_state_preview`'s twin, for inspecting
/// the writing/refused states no click sequence quickly reaches.
fn apply_export_dialog_preview(app: &AppWindow, stage: &str) {
    app.set_export_open(true);
    app.set_export_context("mantis-512.ffp · ORB · 2026-09-28 14:32".into());
    app.set_export_what("Exporting the saved checkpoint you selected.".into());
    app.set_export_stage(stage.into());
    app.set_export_writing_file_name(if stage == "writing" {
        "mantis-512-checkpoint-3.tif".into()
    } else {
        slint::SharedString::default()
    });
    let lines: Vec<slint::SharedString> = if stage == "refused" {
        vec![
            "mantis_014.tif — missing".into(),
            "mantis_037.tif — changed since this checkpoint was fused".into(),
        ]
    } else {
        Vec::new()
    };
    app.set_export_refusal_lines(slint::ModelRc::new(slint::VecModel::from(lines)));
}

/// The post-fuse actions `start_fuse_tabs_timer` may run once every pressed
/// tab settles, bundled to keep the function's argument count down.
struct FuseTabsFollowUp {
    save: bool,
    close: bool,
    quit: bool,
    overlay: bool,
    enter_editor: bool,
}

/// Presses `Fuse stack` on each of `indices` in turn, the moment no
/// background load (`--open`) is still in flight. A press fired while a
/// load is still running targets a tab that does not exist yet and is
/// silently dropped, since `--open`'s directory walk and thumbnail decode
/// run off the event-loop thread and settle only once the window's own poll
/// picks up their result. An index already pressed waits until no run is
/// pending, since a press on a tab already running or queued is ignored.
/// Once every press is made and every run settled, `save` presses `Save
/// project` on the proposed path, `overlay` opens the projects overlay, and
/// `enter_editor` selects editor mode.
fn start_fuse_tabs_timer(
    app_weak: slint::Weak<AppWindow>,
    controller: Arc<Mutex<AppController>>,
    slot: &Rc<RefCell<Option<slint::Timer>>>,
    indices: Vec<usize>,
    follow_up: FuseTabsFollowUp,
) {
    let FuseTabsFollowUp {
        save,
        close,
        quit,
        overlay,
        enter_editor,
    } = follow_up;
    let timer_slot = slot.clone();
    let mut pending: std::collections::VecDeque<usize> = indices.into();
    let mut pressed: Vec<usize> = Vec::new();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, job::POLL, move || {
        let Some(app) = app_weak.upgrade() else {
            return;
        };
        if controller.lock().unwrap().load_in_flight() {
            return;
        }
        while let Some(&index) = pending.front() {
            if pressed.contains(&index) && controller.lock().unwrap().work_pending() {
                return;
            }
            pending.pop_front();
            pressed.push(index);
            // The window's own callbacks, so a press here arms the poll
            // timer exactly as a click on the button does.
            app.invoke_tab_selected(index as i32);
            app.invoke_fuse_requested();
        }
        if (save || close || quit || overlay || enter_editor) && {
            let controller = controller.lock().unwrap();
            controller.work_pending() || controller.pending_divergence().is_some()
        } {
            return;
        }
        if enter_editor {
            app.invoke_mode_selected("editor".into());
        }
        if save {
            controller
                .lock()
                .unwrap()
                .save_active_project(|proposed| Some(proposed.to_path_buf()));
        }
        if close {
            let mut controller = controller.lock().unwrap();
            if let Some(i) = controller.active_project() {
                controller.request_close(i);
            }
        }
        if quit {
            controller.lock().unwrap().window_close();
        }
        if overlay && !controller.lock().unwrap().projects_overlay_open() {
            controller.lock().unwrap().toggle_projects_overlay();
        }
        if let Some(t) = timer_slot.borrow().as_ref() {
            t.stop();
        }
    });
    *slot.borrow_mut() = Some(timer);
}

/// Presses `Export image` and confirms with the proposed destination once
/// the `--open`ed project settles, then polls the real worker
/// (`AppController::settle_export`) to completion and repaints --
/// `--export-active-row`'s own timer, `start_fuse_tabs_timer`'s pattern for
/// one press instead of several.
fn start_export_active_row_timer(
    app_weak: slint::Weak<AppWindow>,
    controller: Arc<Mutex<AppController>>,
    slot: &Rc<RefCell<Option<slint::Timer>>>,
) {
    let timer_slot = slot.clone();
    let mut pressed = false;
    let mut started = false;
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, job::POLL, move || {
        let Some(app) = app_weak.upgrade() else {
            return;
        };
        if !pressed {
            if controller.lock().unwrap().load_in_flight() {
                return;
            }
            let mut controller = controller.lock().unwrap();
            controller.checkpoint_export_requested();
            controller.export_confirmed(|proposed| Some(proposed.to_path_buf()));
            started = controller.take_export_started();
            pressed = true;
            bridge::push_export_dialog(&app, &controller);
            if !started && let Some(t) = timer_slot.borrow().as_ref() {
                t.stop();
            }
            return;
        }
        if started {
            let done = controller.lock().unwrap().settle_export();
            bridge::push_export_dialog(&app, &controller.lock().unwrap());
            if !done {
                return;
            }
        }
        if let Some(t) = timer_slot.borrow().as_ref() {
            t.stop();
        }
    });
    *slot.borrow_mut() = Some(timer);
}

/// Parses `"WxH"`; `None` on anything else, including a missing or
/// non-numeric dimension.
fn parse_size(spec: &str) -> Option<(u32, u32)> {
    let (w, h) = spec.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// Configures Slint's surface-compatible wgpu device with the compute limits
/// the canvas needs.
fn wgpu_settings() -> slint::wgpu_29::WGPUSettings {
    let mut settings = slint::wgpu_29::WGPUSettings::default();
    settings.device_required_features = focusforge_gpu::limits::REQUIRED_FEATURES;
    settings.device_required_limits = focusforge_gpu::limits::required_limits();
    settings
}

fn begin_lifecycle(generation: &std::cell::Cell<u64>) -> u64 {
    let next = generation.get().wrapping_add(1);
    generation.set(next);
    next
}

fn teardown_is_current(generation: &std::cell::Cell<u64>, teardown: u64) -> bool {
    generation.get() == teardown
}

/// Selects Slint's pinned wgpu 29 backend before any window is created.
fn select_backend(
    controller: Arc<Mutex<AppController>>,
    canvas: Rc<RefCell<Canvas>>,
    app_handle: Rc<RefCell<Option<slint::Weak<AppWindow>>>>,
    toast_timer: Rc<RefCell<Option<slint::Timer>>>,
) -> Result<(), ExitCode> {
    let config = slint::wgpu_29::WGPUConfiguration::Automatic(wgpu_settings());
    // The drop route is installed on this *existing* `BackendSelector`
    // chain, before any window exists, which is why `controller` and
    // `canvas` are both built before `select_backend` runs rather than
    // inside `bridge::install`.
    if let Err(e) = slint::BackendSelector::new()
        .require_wgpu_29(config)
        .with_winit_custom_application_handler(bridge::DropHandler::new(
            controller,
            canvas,
            app_handle,
            toast_timer,
        ))
        .select()
    {
        tracing::error!("{e}");
        return Err(ExitCode::from(1));
    }
    // The crate's feature set pins exactly one backend/renderer
    // combination, so "resolved" is what was compiled in.
    println!("resolved backend: winit, renderer: renderer-femtovg-wgpu (wgpu 29.0.4)");
    Ok(())
}

/// Installs the exit-after and resize timers, then runs the event loop.
/// Returns the process exit code.
///
/// The size timer fires at 50 ms, not a literal 0 ms: measured on this
/// Wayland session, a 0 ms delay still raced `show()`'s own initial layout
/// commit and lost -- the written PNG's width silently reverted to
/// `preferred-width` while height applied correctly, the same asymmetry
/// slint-ui/slint#13245 describes. 50 ms clears the race; the exit-after
/// timer's own screenshot is what proves it.
fn run_window<T: ComponentHandle + 'static>(
    app: T,
    args: &Args,
    controller: Option<Arc<Mutex<AppController>>>,
    canvas: Option<Rc<RefCell<Canvas>>>,
    app_handle: Option<slint::Weak<AppWindow>>,
) -> i32 {
    let exit_code = std::rc::Rc::new(std::cell::Cell::new(0i32));
    let device: Rc<RefCell<Option<wgpu::Device>>> = Rc::new(RefCell::new(None));
    let closing = Rc::new(std::cell::Cell::new(false));
    let forced_loss = Rc::new(std::cell::Cell::new(false));
    let loss_reported = Rc::new(std::cell::Cell::new(false));
    let lifecycle = Rc::new(std::cell::Cell::new(0));

    {
        let device = device.clone();
        let closing = closing.clone();
        let lifecycle = lifecycle.clone();
        let controller = controller.clone();
        let canvas = canvas.clone();
        let app_handle = app_handle.clone();
        if let Err(e) = app
            .window()
            .set_rendering_notifier(move |state, api| match state {
                slint::RenderingState::RenderingSetup => {
                    begin_lifecycle(&lifecycle);
                    let slint::GraphicsAPI::WGPU29 {
                        device: next,
                        queue,
                        ..
                    } = api
                    else {
                        return;
                    };
                    focusforge_gpu::errors::install_handlers(next);
                    *device.borrow_mut() = Some(next.clone());
                    if let (Some(controller), Some(canvas), Some(app)) = (
                        controller.as_ref(),
                        canvas.as_ref(),
                        app_handle.as_ref().and_then(|app| app.upgrade()),
                    ) {
                        canvas
                            .borrow_mut()
                            .attach_device(next.clone(), queue.clone());
                        let controller = controller.clone();
                        let canvas = canvas.clone();
                        slint::Timer::single_shot(Duration::ZERO, move || {
                            bridge::repaint(&app, &controller, &canvas);
                        });
                    }
                }
                slint::RenderingState::RenderingTeardown => {
                    if closing.get() {
                        return;
                    }
                    let teardown = lifecycle.get();
                    let device = device.clone();
                    let canvas = canvas.clone();
                    let app_handle = app_handle.clone();
                    let lifecycle = lifecycle.clone();
                    slint::Timer::single_shot(Duration::ZERO, move || {
                        if !teardown_is_current(&lifecycle, teardown) {
                            return;
                        }
                        if let Some(app) = app_handle.and_then(|app| app.upgrade()) {
                            app.set_frame_source(slint::Image::default());
                            app.set_composite_source(slint::Image::default());
                            app.set_zmap_source(slint::Image::default());
                        }
                        if let Some(canvas) = canvas {
                            canvas.borrow_mut().detach_device();
                        }
                        *device.borrow_mut() = None;
                    });
                }
                _ => {}
            })
        {
            tracing::error!("could not register graphics notifier: {e}");
            return 1;
        }
    }

    if let Some(spec) = args.size.clone() {
        let app_weak = app.as_weak();
        slint::Timer::single_shot(Duration::from_millis(50), move || {
            let Some(app) = app_weak.upgrade() else {
                return;
            };
            if let Some((w, h)) = parse_size(&spec) {
                app.window().set_size(slint::PhysicalSize::new(w, h));
            } else {
                tracing::warn!("--size expects WxH, got {spec:?}");
            }
        });
    }

    #[cfg(feature = "gpu-tests")]
    if let Some(ms) = std::env::var("FF_FORCE_DEVICE_LOST_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        let device = device.clone();
        let forced_loss = forced_loss.clone();
        slint::Timer::single_shot(Duration::from_millis(ms), move || {
            if let Some(device) = device.borrow().as_ref() {
                forced_loss.set(true);
                device.destroy();
                let _ = device.poll(wgpu::PollType::Poll);
            }
        });
    }

    if let Some(ms) = args.exit_after {
        let app_weak = app.as_weak();
        let screenshot_path = args.screenshot.clone();
        let exit_code = exit_code.clone();
        let closing = closing.clone();
        slint::Timer::single_shot(Duration::from_millis(ms), move || {
            if let Some(app) = app_weak.upgrade()
                && let Some(path) = &screenshot_path
            {
                if focusforge_gpu::errors::device_lost() {
                    tracing::error!("--screenshot skipped: the graphics device is already lost");
                } else {
                    match app.window().take_snapshot() {
                        Ok(buffer) => {
                            if let Err(e) = screenshot::write_png(&buffer, path) {
                                tracing::error!("--screenshot failed: {e}");
                                exit_code.set(1);
                            }
                        }
                        Err(e) => {
                            tracing::error!("take_snapshot failed: {e}");
                            exit_code.set(1);
                        }
                    }
                }
            }

            closing.set(true);
            let _ = slint::quit_event_loop();
        });
    }

    // Polls for a lost device every 250 ms, off the render path entirely --
    // the flag other code reads is only ever this fresh. Bound to a local so
    // dropping it at function exit stops it. Latched: the loss flag is never
    // cleared, so a second tick before the loop unwinds must not repeat it.
    let loss_timer = slint::Timer::default();
    {
        let exit_code = exit_code.clone();
        let device = device.clone();
        let closing = closing.clone();
        let loss_reported = loss_reported.clone();
        let latched = std::cell::Cell::new(false);
        loss_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(250),
            move || {
                if latched.get() {
                    return;
                }
                if let Some(device) = device.borrow().as_ref().cloned() {
                    let _ = device.poll(wgpu::PollType::Poll);
                }
                if focusforge_gpu::errors::device_lost() {
                    latched.set(true);
                    loss_reported.set(true);
                    tracing::error!("ending the session: the graphics device is gone");
                    exit_code.set(3);
                    closing.set(true);
                    let _ = slint::quit_event_loop();
                }
            },
        );
    }

    let run_result = app.run();
    if let Some(app) = app_handle.and_then(|app| app.upgrade()) {
        app.set_frame_source(slint::Image::default());
        app.set_composite_source(slint::Image::default());
        app.set_zmap_source(slint::Image::default());
    }
    if let Some(canvas) = canvas {
        canvas.borrow_mut().detach_device();
    }
    if let Err(e) = run_result {
        tracing::error!("event loop exited with an error: {e}");
        if let Some(device) = device.borrow().as_ref().cloned() {
            let _ = device.poll(wgpu::PollType::Poll);
        }
        if focusforge_gpu::errors::device_lost() || forced_loss.get() {
            if !loss_reported.replace(true) {
                tracing::error!("ending the session: the graphics device is gone");
            }
            exit_code.set(3);
        } else if exit_code.get() == 0 {
            exit_code.set(1);
        }
    }
    *device.borrow_mut() = None;
    exit_code.get()
}

/// `<DIR>/focusforge` when `--config-dir` was given (mirroring what
/// `directories::ProjectDirs` itself would resolve for the application),
/// else `input_config::default_config_dir()`.
fn resolve_config_dir(args: &Args) -> Option<PathBuf> {
    args.config_dir
        .clone()
        .map(|dir| dir.join("focusforge"))
        .or_else(focusforge_gui::input_config::default_config_dir)
}

/// Loads the input table, prints every refusal once on stderr, writes the
/// defaults on first run, and installs it on `controller` and `app`. A
/// refused or unwritable config file is never a reason the window fails to
/// open.
fn install_input_table(
    controller: &std::sync::Arc<std::sync::Mutex<focusforge_gui::AppController>>,
    app: &AppWindow,
    args: &Args,
) {
    use focusforge_gui::input_config;

    let Some(config_dir) = resolve_config_dir(args) else {
        tracing::warn!(
            "could not resolve a config directory -- starting on the input map defaults"
        );
        return;
    };

    let mut loaded = input_config::load(&config_dir);
    for problem in &loaded.problems {
        tracing::warn!("{problem}");
    }
    if matches!(loaded.provenance, input_config::Provenance::Defaults)
        && loaded.problems.is_empty()
        && let Err(e) = input_config::write_defaults(&config_dir)
    {
        tracing::warn!("{e}");
        loaded.problems.push(e);
    }

    let mut controller = controller.lock().unwrap();
    controller.install_input_table(loaded.table, loaded.provenance);

    let rows: Vec<focusforge_gui::InputRow> = controller
        .input_rows()
        .into_iter()
        .map(|r| focusforge_gui::InputRow {
            input: r.input.into(),
            stacking: r.stacking.into(),
            editor: r.editor.into(),
        })
        .collect();
    app.set_input_rows(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    app.set_input_provenance(controller.input_provenance_line().into());

    if let Some(section) = &args.settings_section {
        controller.open_settings();
        app.set_settings_open(true);
        let parsed = if section == "diagnostics" {
            focusforge_gui::SettingsSection::Diagnostics
        } else {
            focusforge_gui::SettingsSection::InputMap
        };
        controller.select_settings_section(parsed);
        app.set_settings_section(section.clone().into());
    }
}

/// Sets the diagnostics directories, starts a scan for them on a background
/// thread, and pushes the panel's first view -- which carries no rows yet,
/// since nothing on this path reads either directory itself.
fn install_diagnostics(
    controller: &std::sync::Arc<std::sync::Mutex<focusforge_gui::AppController>>,
    app: &AppWindow,
) {
    controller.lock().unwrap().set_diagnostics_dirs(
        focusforge_diagnostics::data_dir_from_env(),
        focusforge_diagnostics::config_dir_from_env(),
    );
    let _ = controller.lock().unwrap().start_manifest_scan();
    focusforge_gui::bridge::push_diagnostics_view(app, controller);
}

/// Loads the `Recent` list from the same config directory `install_input_table`
/// resolves and installs it on `controller`. A directory that fails to
/// resolve leaves the list empty for the session, never a reason the window
/// fails to open.
fn install_recent(
    controller: &std::sync::Arc<std::sync::Mutex<focusforge_gui::AppController>>,
    args: &Args,
) {
    let Some(config_dir) = resolve_config_dir(args) else {
        return;
    };
    let entries = focusforge_gui::recent::load(&config_dir);
    controller
        .lock()
        .unwrap()
        .install_recent(entries, config_dir);
}

fn main() -> ExitCode {
    // Ahead of every parser: a re-spawned monitor process carries no
    // arguments of its own and never opens a window.
    if let Some(code) = focusforge_diagnostics::run_monitor() {
        return ExitCode::from(code as u8);
    }
    // Held across `run`, so what it buffers is flushed as it drops here.
    let _diagnostics = focusforge_diagnostics::init(&focusforge_diagnostics::Options {
        data_dir: focusforge_diagnostics::data_dir_from_env(),
        verbosity: 0,
    });
    // The earliest point a forced crash is observable: `init` is what
    // installs the hook that reports it.
    focusforge_diagnostics::force_crash("startup");

    let result = run(&Args::parse());
    focusforge_gui::bridge::finish_input_trace();
    result
}

/// Everything the arguments ask for, from the first refusal through the
/// event loop. Every ending is a returned code rather than a process exit,
/// which runs no destructor: the diagnostics value `main` holds has to drop
/// for what it buffers to reach disk. Exit codes: 0 for a window closed
/// normally, 1 for a failure while opening one, 2 for a refused argument, 3
/// for a session ended because the graphics device was lost.
fn run(args: &Args) -> ExitCode {
    if let Some(section) = &args.settings_section
        && section != "input-map"
        && section != "diagnostics"
    {
        tracing::error!(
            "--settings-section accepts \"input-map\" or \"diagnostics\", got {section:?}"
        );
        return ExitCode::from(2);
    }
    if let Some(tab) = &args.dock_tab
        && tab != "images"
        && tab != "settings"
    {
        tracing::error!("--dock-tab accepts only \"images\" or \"settings\", got {tab:?}");
        return ExitCode::from(2);
    }
    if let Some(state) = &args.toolbar_state
        && !TOOLBAR_STATES.contains(&state.as_str())
    {
        tracing::error!("--toolbar-state accepts one of {TOOLBAR_STATES:?}, got {state:?}");
        return ExitCode::from(2);
    }

    // Built before `select_backend`: the drop route (`bridge::DropHandler`)
    // installs on the `BackendSelector` chain before any window exists, and
    // needs the same renderer cell every other open path feeds.
    let mut controller_state = AppController::new();
    controller_state.set_gpu_fusion(true);
    let controller = Arc::new(Mutex::new(controller_state));
    let canvas: Rc<RefCell<Canvas>> = Rc::new(RefCell::new(Canvas::default()));
    let app_handle: Rc<RefCell<Option<slint::Weak<AppWindow>>>> = Rc::new(RefCell::new(None));
    // Shared with `bridge::install`'s own failure toast, so a blocked drop
    // and a blocked add never close each other's auto-dismiss timer early.
    let toast_timer: Rc<RefCell<Option<slint::Timer>>> = Rc::new(RefCell::new(None));
    if let Err(code) = select_backend(
        controller.clone(),
        canvas.clone(),
        app_handle.clone(),
        toast_timer.clone(),
    ) {
        return code;
    }

    let code = if args.theme_sheet {
        let sheet = ThemeSheet::new().expect("failed to instantiate ThemeSheet");
        run_window(sheet, args, None, None, None)
    } else {
        let app = AppWindow::new().expect("failed to instantiate AppWindow");
        *app_handle.borrow_mut() = Some(app.as_weak());
        let focusforge_gui::bridge::Installed {
            controller,
            job_timer,
            toast_timer,
        } = bridge::install(&app, controller, canvas.clone(), toast_timer);
        let _ = &job_timer;
        let _ = &toast_timer;

        install_input_table(&controller, &app, args);
        install_diagnostics(&controller, &app);
        bridge::install_crash_notice(&controller, &app);
        install_recent(&controller, args);

        if let Some(state) = &args.toolbar_state {
            apply_toolbar_state_preview(&app, state.as_str());
        }
        if let Some(stage) = &args.export_dialog_preview {
            apply_export_dialog_preview(&app, stage.as_str());
        }

        // Each load runs on its own worker thread; a refusal reaches the
        // window's own refusal dialog. Only the first `--open` path starts
        // here -- the load timer starts each of the rest in turn once the
        // one before it settles, and repaints once the last one has.
        let open_wait_timer: Rc<RefCell<Option<slint::Timer>>> = Rc::new(RefCell::new(None));
        if let Some((first, rest)) = args.open.split_first() {
            controller
                .lock()
                .unwrap()
                .open_paths(std::slice::from_ref(first));
            bridge::start_load_timer(
                app.as_weak(),
                controller.clone(),
                canvas.clone(),
                &open_wait_timer,
                rest.to_vec(),
            );
        }

        if let Some(tab) = &args.dock_tab {
            let tab = if tab == "settings" {
                focusforge_gui::DockTab::Settings
            } else {
                focusforge_gui::DockTab::Images
            };
            controller.lock().unwrap().select_dock_tab(tab);
        }

        if args.images_sort_menu_open {
            controller.lock().unwrap().toggle_images_sort_menu();
        }

        if args.projects_overlay && args.fuse_tabs.is_empty() {
            controller.lock().unwrap().toggle_projects_overlay();
        }

        if let Some(index) = args.frame_remove_warning {
            controller.lock().unwrap().remove_frame(index);
        }

        let export_active_row_timer: Rc<RefCell<Option<slint::Timer>>> =
            Rc::new(RefCell::new(None));
        if args.export_active_row && !args.open.is_empty() {
            start_export_active_row_timer(
                app.as_weak(),
                controller.clone(),
                &export_active_row_timer,
            );
        }

        let fuse_tabs_timer: Rc<RefCell<Option<slint::Timer>>> = Rc::new(RefCell::new(None));
        if !args.fuse_tabs.is_empty() {
            start_fuse_tabs_timer(
                app.as_weak(),
                controller.clone(),
                &fuse_tabs_timer,
                args.fuse_tabs.clone(),
                FuseTabsFollowUp {
                    save: args.save_active_project,
                    close: args.request_close,
                    quit: args.request_quit,
                    overlay: args.projects_overlay,
                    enter_editor: args.enter_editor,
                },
            );
        }

        run_window(
            app,
            args,
            Some(controller),
            Some(canvas),
            app_handle.borrow().clone(),
        )
    };

    ExitCode::from(code as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wgpu_settings_change_only_the_required_device_values() {
        let defaults = slint::wgpu_29::WGPUSettings::default();
        let settings = wgpu_settings();

        assert_eq!(
            settings.device_required_features,
            focusforge_gpu::limits::REQUIRED_FEATURES
        );
        assert_eq!(
            settings.device_required_limits,
            focusforge_gpu::limits::required_limits()
        );
        assert_eq!(settings.backends, defaults.backends);
        assert_eq!(
            format!("{:?}", settings.backend_options),
            format!("{:?}", defaults.backend_options)
        );
        assert_eq!(settings.instance_flags, defaults.instance_flags);
        assert_eq!(
            format!("{:?}", settings.instance_memory_budget_thresholds),
            format!("{:?}", defaults.instance_memory_budget_thresholds)
        );
        assert_eq!(settings.power_preference, defaults.power_preference);
        assert_eq!(settings.device_label, defaults.device_label);
        assert_eq!(
            format!("{:?}", settings.device_experimental_features),
            format!("{:?}", defaults.device_experimental_features)
        );
        assert_eq!(
            format!("{:?}", settings.device_memory_hints),
            format!("{:?}", defaults.device_memory_hints)
        );
    }

    #[test]
    fn newer_setup_keeps_a_queued_older_teardown_stale() {
        let generation = std::cell::Cell::new(0);
        let old_setup = begin_lifecycle(&generation);
        let new_setup = begin_lifecycle(&generation);

        assert_ne!(old_setup, new_setup);
        assert!(!teardown_is_current(&generation, old_setup));
        assert!(teardown_is_current(&generation, new_setup));
    }
}
