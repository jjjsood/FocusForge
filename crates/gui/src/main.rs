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
    /// Resize the window to `WxH`, applied from a one-shot timer after
    /// `show()` -- slint-ui/slint#13245. A 0 ms delay still races the
    /// initial `show()` layout commit that reasserts `preferred-width` on
    /// this host (measured: width silently reverted to 1440, height applied
    /// correctly); 50 ms clears that race.
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
    /// Overrides where the input map is read from/written to --
    /// `<DIR>/focusforge/input.toml`, mirroring what
    /// `input_config::default_config_dir()` itself resolves to. Exists so a
    /// test run never writes into the tester's own
    /// `~/.config/focusforge/`.
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
    /// Opens the `Images` panel's sort menu at startup, for inspecting it
    /// with no click sequence.
    #[arg(long)]
    images_sort_menu_open: bool,
    /// Opens the projects overlay at startup, for inspecting every state of
    /// this surface with no click sequence.
    #[arg(long)]
    projects_overlay: bool,
    /// Requests removing the frame at this index right after `--open`, for
    /// inspecting the frame-set warning it raises on a checkpointed project
    /// with no click sequence.
    #[arg(long)]
    frame_remove_warning: Option<usize>,
    /// Selects these tab indices in turn and presses `Fuse stack` on each,
    /// right after `--open`, leaving the last one selected -- for
    /// inspecting a live run and the tabs queued behind it with no click
    /// sequence. Comma-separated.
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

/// Presses `Fuse stack` on each of `indices` in turn, the moment no
/// background load (`--open`) is still in flight. A press fired while a
/// load is still running targets a tab that does not exist yet and is
/// silently dropped, since `--open`'s directory walk and thumbnail decode
/// run off the event-loop thread and settle only once the window's own poll
/// picks up their result.
fn start_fuse_tabs_timer(
    app_weak: slint::Weak<AppWindow>,
    controller: Arc<Mutex<AppController>>,
    slot: &Rc<RefCell<Option<slint::Timer>>>,
    indices: Vec<usize>,
) {
    let timer_slot = slot.clone();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, job::POLL, move || {
        let Some(app) = app_weak.upgrade() else {
            return;
        };
        if controller.lock().unwrap().load_in_flight() {
            return;
        }
        for index in &indices {
            // The window's own callbacks, so a press here arms the poll
            // timer exactly as a click on the button does.
            app.invoke_tab_selected(*index as i32);
            app.invoke_fuse_requested();
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

/// Selects the pinned wgpu 29 backend before any window is created, on the
/// `Manual` device-ownership path: `focusforge_gpu::device::shared_device()`
/// is created first, then handed to Slint by value, which is what keeps the
/// composite/Z-Map texture off the CPU round-trip. There is no
/// CPU-tone-mapped and no `Automatic` fallback, so no usable adapter is the
/// run's end: the named `GpuError` and the still-working command-line path
/// are printed and `Err` carries the code the process ends with.
fn select_backend(
    controller: Arc<Mutex<AppController>>,
    canvas: Rc<RefCell<Canvas>>,
    app_handle: Rc<RefCell<Option<slint::Weak<AppWindow>>>>,
) -> Result<(), ExitCode> {
    let shared = match focusforge_gpu::device::shared_device() {
        Ok(shared) => shared,
        Err(err) => {
            tracing::error!("{err}\nfocusforge-cli still works without a GPU adapter");
            return Err(ExitCode::from(1));
        }
    };

    let config = slint::wgpu_29::WGPUConfiguration::Manual {
        instance: shared.instance().clone(),
        adapter: shared.adapter().clone(),
        device: shared.device().clone(),
        queue: shared.queue().clone(),
    };
    // The drop route is installed on this *existing* `BackendSelector`
    // chain, before any window exists, which is why `controller` and
    // `canvas` are both built before `select_backend` runs rather than
    // inside `bridge::install`.
    if let Err(e) = slint::BackendSelector::new()
        .require_wgpu_29(config)
        .with_winit_custom_application_handler(bridge::DropHandler::new(
            controller, canvas, app_handle,
        ))
        .select()
    {
        tracing::error!("{e}");
        return Err(ExitCode::from(1));
    }
    // The crate's feature set pins exactly one backend/renderer
    // combination, so "resolved" is what was compiled in.
    println!("resolved backend: winit, renderer: renderer-femtovg-wgpu (wgpu 29.0.4)");
    println!("{}", focusforge_gpu::device::adapter_report(shared.info()));
    Ok(())
}

/// Installs the exit-after and resize timers, then runs the event loop.
/// Deliberately no `set_rendering_notifier` (slint-ui/slint#12030). Returns
/// the process exit code.
///
/// The size timer fires at 50 ms, not a literal 0 ms: measured on this
/// Wayland session, a 0 ms delay still raced `show()`'s own initial layout
/// commit and lost -- the written PNG's width silently reverted to
/// `preferred-width` while height applied correctly, the same asymmetry
/// slint-ui/slint#13245 describes. 50 ms clears the race; the exit-after
/// timer's own screenshot is what proves it.
fn run_window<T: ComponentHandle + 'static>(app: T, args: &Args) -> i32 {
    let exit_code = std::rc::Rc::new(std::cell::Cell::new(0i32));

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
        slint::Timer::single_shot(Duration::from_millis(ms), || {
            if let Ok(shared) = focusforge_gpu::device::shared_device() {
                shared.force_loss();
            }
        });
    }

    if let Some(ms) = args.exit_after {
        let app_weak = app.as_weak();
        let screenshot_path = args.screenshot.clone();
        let exit_code = exit_code.clone();
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
        let latched = std::cell::Cell::new(false);
        loss_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(250),
            move || {
                if latched.get() {
                    return;
                }
                let Ok(shared) = focusforge_gpu::device::shared_device() else {
                    return;
                };
                if shared.lost() {
                    latched.set(true);
                    tracing::error!("ending the session: the graphics device is gone");
                    exit_code.set(3);
                    let _ = slint::quit_event_loop();
                }
            },
        );
    }

    if let Err(e) = app.run() {
        tracing::error!("event loop exited with an error: {e}");
        exit_code.set(1);
    }
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

    run(&Args::parse())
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
    let controller = Arc::new(Mutex::new(AppController::new()));
    let canvas: Rc<RefCell<Canvas>> = Rc::new(RefCell::new(Canvas::default()));
    let app_handle: Rc<RefCell<Option<slint::Weak<AppWindow>>>> = Rc::new(RefCell::new(None));
    if let Err(code) = select_backend(controller.clone(), canvas.clone(), app_handle.clone()) {
        return code;
    }

    let code = if args.theme_sheet {
        let sheet = ThemeSheet::new().expect("failed to instantiate ThemeSheet");
        run_window(sheet, args)
    } else {
        let app = AppWindow::new().expect("failed to instantiate AppWindow");
        *app_handle.borrow_mut() = Some(app.as_weak());
        let focusforge_gui::bridge::Installed {
            controller,
            job_timer,
            toast_timer,
        } = bridge::install(&app, controller, canvas.clone());
        let _ = &job_timer;
        let _ = &toast_timer;

        install_input_table(&controller, &app, args);
        install_diagnostics(&controller, &app);
        bridge::install_crash_notice(&controller, &app);
        install_recent(&controller, args);

        if let Some(state) = &args.toolbar_state {
            apply_toolbar_state_preview(&app, state.as_str());
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

        if args.projects_overlay {
            controller.lock().unwrap().toggle_projects_overlay();
        }

        if let Some(index) = args.frame_remove_warning {
            controller.lock().unwrap().remove_frame(index);
        }

        let fuse_tabs_timer: Rc<RefCell<Option<slint::Timer>>> = Rc::new(RefCell::new(None));
        if !args.fuse_tabs.is_empty() {
            start_fuse_tabs_timer(
                app.as_weak(),
                controller.clone(),
                &fuse_tabs_timer,
                args.fuse_tabs.clone(),
            );
        }

        run_window(app, args)
    };

    ExitCode::from(code as u8)
}
