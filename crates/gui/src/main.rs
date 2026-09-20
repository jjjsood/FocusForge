// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-gui`'s entry point. Every flag below exists so the window
//! can be observed without a person in front of it (steps 9-13).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use focusforge_gui::{AppController, AppWindow, ThemeSheet, bridge, screenshot};
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
    /// Requests removing the frame at this index right after `--open`, for
    /// inspecting the frame-set warning it raises on a checkpointed project
    /// with no click sequence.
    #[arg(long)]
    frame_remove_warning: Option<usize>,
}

const TOOLBAR_STATES: [&str; 5] = ["idle", "running", "paused", "cancelling", "failed"];

/// Sets the toolbar's and the bottom bar's transport preview together, from
/// literal values with no controller or job behind them -- for inspecting a
/// state no click sequence currently reaches.
fn apply_toolbar_state_preview(app: &AppWindow, state: &str) {
    app.set_toolbar_run_state(state.into());
    let (stage_word, step_text, fraction, tone, eta) = match state {
        "running" => ("Fusing", "step 2 of 3", 0.62, "running", "ETA 1:12"),
        "paused" => ("Paused", "step 2 of 3", 0.62, "held", "ETA —"),
        "cancelling" => ("Cancelling", "step 1 of 3", 0.0, "held", "ETA —"),
        "failed" => ("Failed", "step 1 of 3", 0.28, "failed", "ETA —"),
        _ => ("", "", 0.0, "held", ""),
    };
    app.set_stage_word(stage_word.into());
    app.set_step_text(step_text.into());
    app.set_bar_fraction(fraction);
    app.set_bar_tone(tone.into());
    app.set_eta_text(eta.into());
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
fn select_backend(controller: Arc<Mutex<AppController>>) -> Result<(), ExitCode> {
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
    // chain, before any window exists, which is why `controller` is built
    // before `select_backend` runs rather than inside `bridge::install`.
    if let Err(e) = slint::BackendSelector::new()
        .require_wgpu_29(config)
        .with_winit_custom_application_handler(bridge::DropHandler::new(controller))
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
/// The size timer fires at 50 ms, not step 13's literal 0 ms: measured on
/// this Wayland session, a 0 ms delay still raced `show()`'s own initial
/// layout commit and lost -- the written PNG's width silently reverted to
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

    if let Some(ms) = args.exit_after {
        let app_weak = app.as_weak();
        let screenshot_path = args.screenshot.clone();
        let exit_code = exit_code.clone();
        slint::Timer::single_shot(Duration::from_millis(ms), move || {
            if let Some(app) = app_weak.upgrade()
                && let Some(path) = &screenshot_path
            {
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

            let _ = slint::quit_event_loop();
        });
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
/// defaults on first run, and installs it on `controller` and `app`.
/// A refused or unwritable config file is never a reason the window
/// fails to open.
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

    if args.settings_section.is_some() {
        controller.open_settings();
        app.set_settings_open(true);
    }
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
/// normally, 1 for a failure while opening one, 2 for a refused argument.
fn run(args: &Args) -> ExitCode {
    if let Some(section) = &args.settings_section
        && section != "input-map"
    {
        tracing::error!("--settings-section accepts only \"input-map\", got {section:?}");
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

    // Built before `select_backend`: the drop route
    // (`bridge::DropHandler`) installs on the `BackendSelector` chain
    // before any window exists.
    let controller = Arc::new(Mutex::new(AppController::new()));
    if let Err(code) = select_backend(controller.clone()) {
        return code;
    }

    let code = if args.theme_sheet {
        let sheet = ThemeSheet::new().expect("failed to instantiate ThemeSheet");
        run_window(sheet, args)
    } else {
        let app = AppWindow::new().expect("failed to instantiate AppWindow");
        let focusforge_gui::bridge::Installed {
            controller,
            canvas,
            job_timer,
            toast_timer,
        } = bridge::install(&app, controller);
        let _ = &job_timer;
        let _ = &toast_timer;

        install_input_table(&controller, &app, args);

        if let Some(state) = &args.toolbar_state {
            apply_toolbar_state_preview(&app, state.as_str());
        }

        for path in &args.open {
            // The same `open_paths` the picker and the drop route call: a
            // folder proposes a grouping, a `*.ffp` reopens a saved project.
            // A reopened project feeds the canvas straight from its own
            // sealed planes; an unsaved group waits for `Fuse stack`.
            let outcome = controller
                .lock()
                .unwrap()
                .open_paths(std::slice::from_ref(path));
            if let focusforge_gui::OpenOutcome::Refused(err) = outcome {
                tracing::error!("--open failed: {err}");
                return ExitCode::from(1);
            }
            bridge::feed_canvas(&app, &controller, &canvas);
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

        if let Some(index) = args.frame_remove_warning {
            controller.lock().unwrap().remove_frame(index);
        }

        run_window(app, args)
    };

    ExitCode::from(code as u8)
}
