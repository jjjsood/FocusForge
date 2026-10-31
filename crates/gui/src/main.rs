// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-gui`'s entry point. Every flag below exists so the window
//! can be observed without a person in front of it.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use focusforge_gui::bridge::Canvas;
use focusforge_gui::{AppController, AppWindow, ThemeSheet, bridge, job, picker, screenshot};
use slint::ComponentHandle;
use slint::wgpu_29::wgpu;
#[cfg(target_os = "linux")]
use slint::winit_030::winit::platform::startup_notify::WindowAttributesExtStartupNotify;
#[cfg(target_os = "linux")]
use slint::winit_030::winit::platform::wayland::{EventLoopBuilderExtWayland, WindowExtWayland};
#[cfg(target_os = "linux")]
use slint::winit_030::winit::platform::x11::EventLoopBuilderExtX11;
#[cfg(target_os = "linux")]
use slint::winit_030::{SlintEvent, WinitWindowAccessor, winit};

#[derive(Parser)]
#[command(name = "focusforge-gui", version)]
struct Args {
    /// Exit this many milliseconds after the window is shown.
    #[arg(long)]
    exit_after: Option<u64>,
    /// Write a screenshot of the window to this path, taken when
    /// `--exit-after` fires.
    #[arg(long)]
    screenshot: Option<PathBuf>,
    /// Choose the main or detached window for `--screenshot`.
    #[arg(long, value_enum, default_value = "main", requires = "screenshot")]
    screenshot_window: ScreenshotWindow,
    /// Undock a pane once a project loads, for window inspection.
    #[arg(long, value_enum, requires = "open")]
    undock_pane: Option<UndockPane>,
    /// Dock the inspected pane this many milliseconds after undocking.
    #[arg(long, requires = "undock_pane")]
    dock_after: Option<u64>,
    /// Reopen the inspected pane this many milliseconds after docking it.
    #[arg(long, requires = "dock_after")]
    reopen_after: Option<u64>,
    /// Request main-window close this many milliseconds after undocking.
    #[arg(long, requires = "undock_pane")]
    close_main_after: Option<u64>,
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
    /// `performance`, `appearance`, `storage`, `input-map`, `diagnostics` or `about`.
    #[arg(long)]
    settings_section: Option<String>,
    /// Displays one Appearance design state at startup. This does not load,
    /// apply or persist a source-presentation preference.
    #[arg(long, value_enum)]
    appearance_preview: Option<AppearancePreview>,
    /// Displays a deterministic compact Images-panel state for native
    /// inspection without reading source files.
    #[arg(long, value_enum, conflicts_with = "open")]
    list_preview: Option<ListPreview>,
    /// Displays deterministic Image list states for native inspection without
    /// reading source files.
    #[arg(long, value_enum, conflicts_with_all = ["open", "list_preview"])]
    image_list_preview: Option<ImageListPreview>,
    /// Displays deterministic Large preview states for native inspection
    /// without reading source files.
    #[arg(long, value_enum, conflicts_with_all = ["open", "list_preview", "image_list_preview"])]
    large_preview: Option<LargePreview>,
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
    /// `ready-png`, `ready-jpeg`, `ready-dng`, `ready-menu`, `writing`,
    /// `refused` or `refused-format`.
    #[arg(long)]
    export_dialog_preview: Option<String>,
    /// Opens the `Images` panel's sort menu at startup, for inspecting it
    /// with no click sequence.
    #[arg(long)]
    images_sort_menu_open: bool,
    /// Opens the projects overlay at startup, for inspecting every state of
    /// this surface with no click sequence. With `--fuse-tabs`, it opens once
    /// the last run settles instead. With `--open`, it stays open once the
    /// load lands.
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
    /// Cancels the genuine unsaved-work quit question after the last
    /// `--fuse-tabs` run settles, for observing guarded two-window lifetime.
    #[arg(long, requires = "fuse_tabs")]
    cancel_quit_after: Option<u64>,
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
    /// sequence. Comma-separated. An index repeated waits until every earlier
    /// run has settled and any Keep/Discard question has been answered in
    /// the window, so `0,0` fuses tab 0 twice.
    #[arg(long, value_delimiter = ',')]
    fuse_tabs: Vec<usize>,
    /// Holds the Nth fuse run started in this launch (counted from 1, a whole
    /// number) in its fuse stage for `--slow-run-seconds`. Other runs keep
    /// their speed.
    #[arg(long, allow_hyphen_values = true)]
    slow_run: Option<String>,
    /// Minimum fuse running time, in whole seconds, of the run `--slow-run`
    /// selects. Defaults to 150.
    #[arg(long, allow_hyphen_values = true)]
    slow_run_seconds: Option<String>,
    /// Holds every folder or project load for this many whole seconds before
    /// it reads anything.
    #[arg(long, allow_hyphen_values = true)]
    slow_open: Option<String>,
    /// Writes every project save and image export into this existing folder
    /// under its proposed file name, with no save dialog.
    #[arg(long, value_name = "DIR", allow_hyphen_values = true)]
    save_folder: Option<String>,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ScreenshotWindow {
    Main,
    Comparison,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum AppearancePreview {
    Default,
    Focus,
    Open,
    Disabled,
    SaveError,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ListPreview {
    Complete,
    Dragging,
    Empty,
    Editor,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ImageListPreview {
    Complete,
    States,
    Tooltip,
    Dragging,
    Empty,
    Editor,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum LargePreview {
    Complete,
    States,
    Tooltip,
    Dragging,
    Empty,
    Editor,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum UndockPane {
    Frame,
    Composite,
}

const TOOLBAR_STATES: [&str; 5] = ["idle", "running", "paused", "cancelling", "failed"];

/// Sets the export modal's own state directly, with no controller or
/// worker behind it, for inspecting the writing/refused states no click
/// sequence quickly reaches.
fn apply_export_dialog_preview(app: &AppWindow, spec: &str) {
    use focusforge_gui::job::ExportFormat;
    // `ready`, `ready-png`, `ready-jpeg`, `ready-dng`, `ready-menu` (TIFF
    // compression list open), `writing`, `refused` or `refused-format` (a
    // format cause, no `Locate frames…`).
    let (stage, variant) = spec.split_once('-').unwrap_or((spec, ""));
    let choice = focusforge_gui::export_settings::ExportChoice {
        format: match variant {
            "png" => ExportFormat::Png,
            "jpeg" => ExportFormat::Jpeg,
            "dng" => ExportFormat::Dng,
            _ => ExportFormat::Tiff,
        },
        ..Default::default()
    };
    let open = (variant == "menu").then_some("tiff-compression");
    focusforge_gui::bridge::push_export_view(app, &choice.view(open));
    app.set_export_open(true);
    app.set_export_context("mantis-512.ffp · ORB · 2026-09-28 14:32".into());
    app.set_export_what("Exporting the saved checkpoint you selected.".into());
    app.set_export_stage(stage.into());
    app.set_export_writing_file_name(if stage == "writing" {
        "mantis-512-checkpoint-3.tif".into()
    } else {
        slint::SharedString::default()
    });
    app.set_export_can_locate(!(stage == "refused" && variant == "format"));
    let lines: Vec<slint::SharedString> = if stage == "refused" && variant == "format" {
        vec![
            "JPEG can't be bigger than 65,535 pixels on a side. This image is 70,000 × 40,000."
                .into(),
        ]
    } else if stage == "refused" {
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
    cancel_quit_after: Option<u64>,
}

/// Presses `Fuse stack` on each of `indices` in turn, once every `--open`
/// load has settled (`open_timer` is the load timer starting them): an
/// earlier press for a tab not yet open lands on the tab before it and is
/// ignored. An index already pressed waits until no run is pending and no
/// tab holds a Keep/Discard question; a press on a parked tab would
/// discard its result.
/// Once every press is made and every run settled, `save` presses `Save
/// project` on the proposed path, `overlay` opens the projects overlay, and
/// `enter_editor` selects editor mode.
fn start_fuse_tabs_timer(
    app_weak: slint::Weak<AppWindow>,
    controller: Arc<Mutex<AppController>>,
    slot: &Rc<RefCell<Option<slint::Timer>>>,
    indices: Vec<usize>,
    follow_up: FuseTabsFollowUp,
    open_timer: &Rc<RefCell<Option<slint::Timer>>>,
) {
    let FuseTabsFollowUp {
        save,
        close,
        quit,
        overlay,
        enter_editor,
        cancel_quit_after,
    } = follow_up;
    let timer_slot = slot.clone();
    let open_timer = open_timer.clone();
    let mut pending: std::collections::VecDeque<usize> = indices.into();
    let mut pressed: Vec<usize> = Vec::new();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, job::POLL, move || {
        let Some(app) = app_weak.upgrade() else {
            return;
        };
        if controller.lock().unwrap().load_in_flight()
            || open_timer
                .borrow()
                .as_ref()
                .is_some_and(slint::Timer::running)
        {
            return;
        }
        while let Some(&index) = pending.front() {
            if pressed.contains(&index) && {
                let controller = controller.lock().unwrap();
                controller.work_pending() || controller.parked_run_waiting()
            } {
                return;
            }
            pending.pop_front();
            pressed.push(index);
            // The window's own callbacks, so a press here arms the poll
            // timer exactly as a click on the button does.
            app.invoke_tab_selected(index as i32);
            app.invoke_fuse_requested();
        }
        if (save || close || quit || overlay || enter_editor || cancel_quit_after.is_some()) && {
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
                .save_active_project(|proposed| Some(save_destination(proposed)));
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
        if let Some(ms) = cancel_quit_after {
            controller.lock().unwrap().window_close();
            let app_weak = app.as_weak();
            slint::Timer::single_shot(Duration::from_millis(ms), move || {
                if let Some(app) = app_weak.upgrade() {
                    app.invoke_close_guard_cancelled();
                }
            });
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
            controller.export_confirmed(|proposed| Some(save_destination(proposed)));
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

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(result) => return result,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// Builds the one device used by Slint and every imported canvas texture.
fn manual_wgpu_configuration(
    #[cfg(target_os = "linux")] display_backend: LinuxDisplayBackend,
) -> Result<slint::wgpu_29::WGPUConfiguration, String> {
    let flags = wgpu::InstanceFlags::from_build_config().with_env();
    #[cfg(target_os = "linux")]
    let flags = adjust_manual_wgpu_flags(
        flags,
        display_backend,
        std::env::var_os("WGPU_VALIDATION").is_some(),
    );
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::from_env().unwrap_or_default(),
        flags,
        backend_options: wgpu::BackendOptions::from_env_or_default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        display: None,
    });
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        .map_err(|error| format!("no compatible wgpu adapter for the selected display: {error}"))?;
    let limits = focusforge_gpu::limits::device_limits(&adapter.limits());
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("focusforge-gui-device"),
        required_features: focusforge_gpu::limits::REQUIRED_FEATURES,
        required_limits: limits,
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::default(),
    }))
    .map_err(|error| format!("could not create FocusForge's shared wgpu device: {error}"))?;

    // Slint drops its Manual queue from thread-local backend state at process
    // shutdown, after wgpu's queue lock trace has already been destroyed.
    let _queue_until_process_exit = Box::leak(Box::new(queue.clone()));

    Ok(slint::wgpu_29::WGPUConfiguration::Manual {
        instance,
        adapter,
        device,
        queue,
    })
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinuxDisplayBackend {
    X11,
    Wayland,
}

#[cfg(target_os = "linux")]
impl LinuxDisplayBackend {
    fn from_environment() -> Result<Self, ExitCode> {
        Self::from_availability(
            std::env::var_os("DISPLAY").is_some(),
            std::env::var_os("WAYLAND_DISPLAY").is_some(),
        )
        .ok_or_else(|| {
            tracing::error!("no Linux display is available: set DISPLAY or WAYLAND_DISPLAY");
            ExitCode::from(1)
        })
    }

    fn from_availability(display: bool, wayland: bool) -> Option<Self> {
        match (display, wayland) {
            (true, _) => Some(Self::X11),
            (false, true) => Some(Self::Wayland),
            (false, false) => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::X11 => "winit-x11",
            Self::Wayland => "winit-wayland",
        }
    }
}

/// The launcher's startup tokens, `DESKTOP_STARTUP_ID` then
/// `XDG_ACTIVATION_TOKEN`; nothing off Linux.
#[cfg(target_os = "linux")]
type LaunchTokens = (Option<String>, Option<String>);
#[cfg(not(target_os = "linux"))]
struct LaunchTokens;

/// Reads the launcher's startup tokens and removes them from the process
/// environment, so no thread or child process started later inherits them.
fn take_launch_tokens() -> LaunchTokens {
    #[cfg(target_os = "linux")]
    {
        let tokens = (
            std::env::var("DESKTOP_STARTUP_ID").ok(),
            std::env::var("XDG_ACTIVATION_TOKEN").ok(),
        );
        winit::platform::startup_notify::reset_activation_token_env();
        tokens
    }
    #[cfg(not(target_os = "linux"))]
    LaunchTokens
}

/// The token that ends the launcher's startup notification: the variable of
/// the session `backend` opens (X11 reads `DESKTOP_STARTUP_ID`, Wayland
/// `XDG_ACTIVATION_TOKEN`), the other one when that is unset or empty.
#[cfg(target_os = "linux")]
fn startup_token(
    backend: LinuxDisplayBackend,
    x11: Option<String>,
    wayland: Option<String>,
) -> Option<String> {
    let (first, second) = match backend {
        LinuxDisplayBackend::X11 => (x11, wayland),
        LinuxDisplayBackend::Wayland => (wayland, x11),
    };
    [first, second]
        .into_iter()
        .flatten()
        .find(|token| !token.is_empty())
}

#[cfg(target_os = "linux")]
fn adjust_manual_wgpu_flags(
    mut flags: wgpu::InstanceFlags,
    display_backend: LinuxDisplayBackend,
    validation_explicit: bool,
) -> wgpu::InstanceFlags {
    if display_backend == LinuxDisplayBackend::Wayland && !validation_explicit {
        // WGPU validation registers an EGL callback that can outlive tracing's
        // thread-local state during native Wayland debug teardown.
        flags.remove(wgpu::InstanceFlags::VALIDATION);
    }
    flags
}

#[cfg(target_os = "linux")]
fn linux_event_loop_builder(backend: LinuxDisplayBackend) -> slint::winit_030::EventLoopBuilder {
    let mut builder = winit::event_loop::EventLoop::<SlintEvent>::with_user_event();
    match backend {
        LinuxDisplayBackend::X11 => {
            builder.with_x11();
        }
        LinuxDisplayBackend::Wayland => {
            builder.with_wayland();
        }
    }
    builder
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
/// Returns whether the session is native Wayland, where dropping files onto
/// the window does not work.
fn select_backend(
    controller: Arc<Mutex<AppController>>,
    canvas: Rc<RefCell<Canvas>>,
    app_handle: Rc<RefCell<Option<slint::Weak<AppWindow>>>>,
    toast_timer: Rc<RefCell<Option<slint::Timer>>>,
    launch_tokens: LaunchTokens,
) -> Result<bool, ExitCode> {
    #[cfg(target_os = "linux")]
    let display_backend = LinuxDisplayBackend::from_environment()?;
    let config = match manual_wgpu_configuration(
        #[cfg(target_os = "linux")]
        display_backend,
    ) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!("{error}");
            return Err(ExitCode::from(1));
        }
    };
    #[cfg(not(target_os = "linux"))]
    let LaunchTokens = launch_tokens;
    // The drop route is installed on this *existing* `BackendSelector`
    // chain, before any window exists, which is why `controller` and
    // `canvas` are both built before `select_backend` runs rather than
    // inside `bridge::install`.
    let selector = slint::BackendSelector::new().backend_name("winit".into());
    #[cfg(target_os = "linux")]
    let selector =
        selector.with_winit_event_loop_builder(linux_event_loop_builder(display_backend));
    // The hook runs for every window; only the first takes the token.
    #[cfg(target_os = "linux")]
    let selector = match startup_token(display_backend, launch_tokens.0, launch_tokens.1) {
        Some(token) => {
            let token = std::cell::Cell::new(Some(winit::window::ActivationToken::from_raw(token)));
            selector.with_winit_window_attributes_hook(move |attributes| match token.take() {
                Some(token) => attributes.with_activation_token(token),
                None => attributes,
            })
        }
        None => selector,
    };
    let selector = selector
        .require_wgpu_29(config)
        .with_winit_custom_application_handler(bridge::DropHandler::new(
            controller,
            canvas,
            app_handle,
            toast_timer,
        ));
    if let Err(e) = selector.select() {
        #[cfg(target_os = "linux")]
        tracing::error!(
            "cannot open the {} display (check DISPLAY and WAYLAND_DISPLAY): {e}",
            display_backend.label()
        );
        #[cfg(not(target_os = "linux"))]
        tracing::error!("{e}");
        return Err(ExitCode::from(1));
    }
    #[cfg(target_os = "linux")]
    println!(
        "selected backend: {}, renderer: renderer-femtovg-wgpu (wgpu 29.0.4, Manual)",
        display_backend.label()
    );
    #[cfg(not(target_os = "linux"))]
    println!("selected backend: winit, renderer: renderer-femtovg-wgpu (wgpu 29.0.4, Manual)");
    #[cfg(target_os = "linux")]
    return Ok(display_backend == LinuxDisplayBackend::Wayland);
    #[cfg(not(target_os = "linux"))]
    Ok(false)
}

#[cfg(target_os = "linux")]
fn log_native_display<T: ComponentHandle + 'static>(app: &T) {
    let app = app.as_weak();
    slint::Timer::single_shot(Duration::from_millis(150), move || {
        let resolved = app
            .upgrade()
            .and_then(|app| {
                app.window()
                    .with_winit_window(|native| native.xdg_toplevel().is_some())
            })
            .map(|wayland| {
                if wayland {
                    "winit-wayland"
                } else {
                    "winit-x11"
                }
            })
            .unwrap_or("unavailable");
        println!("resolved native display: {resolved}");
    });
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
        let last_scale = std::cell::Cell::new(app.window().scale_factor());
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
                        controller
                            .lock()
                            .unwrap()
                            .attach_blender(focusforge_gpu::blend::Blender::new(next, queue));
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
                    let controller = controller.clone();
                    let app_handle = app_handle.clone();
                    let lifecycle = lifecycle.clone();
                    slint::Timer::single_shot(Duration::ZERO, move || {
                        if !teardown_is_current(&lifecycle, teardown) {
                            return;
                        }
                        if let Some(app) = app_handle.and_then(|app| app.upgrade()) {
                            app.set_frame_source(slint::Image::default());
                            app.set_composite_source(slint::Image::default());
                        }
                        if let Some(canvas) = canvas {
                            canvas.borrow_mut().detach_device();
                        }
                        if let Some(controller) = controller {
                            controller.lock().unwrap().detach_blender();
                        }
                        *device.borrow_mut() = None;
                    });
                }
                // Slint reports no scale-factor change on its own, and one
                // without a resize leaves the pane geometry stale.
                slint::RenderingState::BeforeRendering => {
                    if let Some(app) = app_handle.as_ref().and_then(|app| app.upgrade())
                        && last_scale.replace(app.window().scale_factor())
                            != app.window().scale_factor()
                    {
                        slint::Timer::single_shot(Duration::ZERO, move || {
                            app.invoke_canvas_resized();
                        });
                    }
                }
                slint::RenderingState::AfterRendering => bridge::record_frame_rendered(),
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
        let screenshot_window = args.screenshot_window;
        let exit_code = exit_code.clone();
        let closing = closing.clone();
        slint::Timer::single_shot(Duration::from_millis(ms), move || {
            if let Some(app) = app_weak.upgrade()
                && let Some(path) = &screenshot_path
            {
                if focusforge_gpu::errors::device_lost() {
                    tracing::error!("--screenshot skipped: the graphics device is already lost");
                } else {
                    let snapshot = match screenshot_window {
                        ScreenshotWindow::Main => app.window().take_snapshot().map(Some),
                        ScreenshotWindow::Comparison => bridge::comparison_snapshot(),
                    };
                    match snapshot {
                        Ok(None) => {
                            tracing::error!(
                                "--screenshot-window comparison requires a detached pane"
                            );
                            exit_code.set(1);
                        }
                        Ok(Some(buffer)) => {
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

    #[cfg(target_os = "linux")]
    log_native_display(&app);

    let run_result = app.run();
    bridge::shutdown_windows();
    // Quitting never interrupts a write in progress.
    if let Some(controller) = &controller {
        let mut controller = controller.lock().unwrap();
        controller.finish_saves();
        controller.finish_compactions();
        controller.detach_blender();
    }
    if let Some(app) = app_handle.and_then(|app| app.upgrade()) {
        app.set_frame_source(slint::Image::default());
        app.set_composite_source(slint::Image::default());
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
    controller.install_input_table(loaded.table);

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

    if let Some(section) = &args.settings_section {
        controller.open_settings();
        app.set_settings_open(true);
        let parsed = focusforge_gui::SettingsSection::from_slug(section)
            .expect("`--settings-section` was validated against the section list in `run`");
        controller.select_settings_section(parsed);
        app.set_settings_section(parsed.slug().into());
    }
}

fn apply_appearance_preview(app: &AppWindow, preview: Option<AppearancePreview>) {
    let (presentation, menu_open, menu_focused, menu_disabled, save_error) = match preview {
        Some(AppearancePreview::Focus) => ("list", false, true, false, ""),
        Some(AppearancePreview::Open) => ("image-list", true, false, false, ""),
        Some(AppearancePreview::Disabled) => ("image-list", false, false, true, ""),
        Some(AppearancePreview::SaveError) => (
            "image-list",
            false,
            false,
            false,
            "Could not save settings. Previous saved view kept.",
        ),
        Some(AppearancePreview::Default) | None => ("image-list", false, false, false, ""),
    };
    app.set_appearance_presentation(presentation.into());
    app.set_appearance_menu_open(menu_open);
    app.set_appearance_menu_focused(menu_focused);
    app.set_appearance_menu_disabled(menu_disabled);
    app.set_appearance_save_error(save_error.into());
}

fn preview_row(chip: [u8; 3], name: &str, time: &str) -> focusforge_gui::ImageRowView {
    focusforge_gui::ImageRowView {
        chip: slint::Color::from_rgb_u8(chip[0], chip[1], chip[2]),
        name: name.into(),
        time_text: time.into(),
        capture_stamp: "".into(),
        excluded: false,
        unresolved: false,
        selected: false,
        hovered: false,
        focused: false,
        job_disabled: false,
        preview: preview_image(),
        preview_state: "ready".into(),
    }
}

fn preview_image() -> slint::Image {
    let mut pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(2, 2);
    pixels.make_mut_bytes().copy_from_slice(&[
        220, 201, 55, 255, 112, 132, 48, 255, 160, 222, 58, 255, 180, 171, 53, 255,
    ]);
    slint::Image::from_rgba8(pixels)
}

fn image_list_row(chip: [u8; 3], name: &str, time: &str) -> focusforge_gui::ImageRowView {
    let mut row = preview_row(chip, name, time);
    row.capture_stamp = time.into();
    row
}

/// Installs native-inspection data for the compact source-list surface.
/// The normal source model continues to own all live rows and interactions.
fn apply_list_preview(app: &AppWindow, preview: Option<ListPreview>) {
    let Some(preview) = preview else {
        return;
    };

    let (mode, header, rows, dragging, target, offset) = match preview {
        ListPreview::Complete | ListPreview::Dragging => (
            "stacking",
            "6 images",
            vec![
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: true,
                    hovered: true,
                    focused: false,
                    job_disabled: false,
                    ..preview_row([220, 201, 55], "IMG_9819.tif", "16:42:09")
                },
                focusforge_gui::ImageRowView {
                    excluded: true,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    ..preview_row([180, 171, 53], "IMG_9820.tif", "")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: true,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    ..preview_row([112, 132, 48], "IMG_9823.tif", "")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: true,
                    job_disabled: true,
                    ..preview_row([160, 222, 58], "IMG_9824.tif", "16:42:09")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    ..preview_row([220, 201, 55], "IMG_9819-long-filename.tif", "16:42:09")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    ..preview_row([220, 201, 55], "IMG_9825.tif", "16:44:11")
                },
            ],
            if matches!(preview, ListPreview::Dragging) {
                4
            } else {
                -1
            },
            if matches!(preview, ListPreview::Dragging) {
                3
            } else {
                -1
            },
            if matches!(preview, ListPreview::Dragging) {
                -26.0
            } else {
                0.0
            },
        ),
        ListPreview::Empty => ("stacking", "0 images", Vec::new(), -1, -1, 0.0),
        ListPreview::Editor => (
            "editor",
            "Source legend",
            vec![
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: true,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    ..preview_row([220, 201, 55], "IMG_9819.tif", "10")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    ..preview_row([180, 171, 53], "IMG_9820.tif", "11")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    ..preview_row([160, 222, 58], "IMG_9821.tif", "12")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    ..preview_row([112, 132, 48], "IMG_9822.tif", "13")
                },
            ],
            -1,
            -1,
            0.0,
        ),
    };

    app.set_list_preview_active(true);
    app.set_list_preview_mode(mode.into());
    app.set_list_preview_presentation("list".into());
    app.set_list_preview_header_text(header.into());
    app.set_list_preview_rows(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    app.set_list_preview_drag_offset(offset);
    app.set_list_preview_dragging_row(dragging);
    app.set_list_preview_drag_target_row(target);
}

/// Installs bounded source-view examples for the Image list inspection path.
/// It shares the static dock seam with List while leaving controller data intact.
fn apply_image_list_preview(app: &AppWindow, preview: Option<ImageListPreview>) {
    let Some(preview) = preview else {
        return;
    };

    let (header, rows, dragging, target, offset, editor) = match preview {
        ImageListPreview::Complete => (
            "4 images",
            vec![
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: true,
                    hovered: true,
                    focused: false,
                    job_disabled: false,
                    preview_state: "ready".into(),
                    ..image_list_row([220, 201, 55], "IMG_9819.tif", "11.10.26\n16:42")
                },
                focusforge_gui::ImageRowView {
                    excluded: true,
                    unresolved: false,
                    selected: false,
                    hovered: true,
                    focused: false,
                    job_disabled: false,
                    preview_state: "ready".into(),
                    ..image_list_row([180, 171, 53], "IMG_9820.tif", "")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: true,
                    job_disabled: true,
                    preview_state: "ready".into(),
                    ..image_list_row([112, 132, 48], "IMG_9823.tif", "11.10.26\n16:43")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    preview_state: "ready".into(),
                    ..image_list_row([160, 222, 58], "IMG_9824.tif", "11.10.26\n16:44")
                },
            ],
            -1,
            -1,
            0.0,
            false,
        ),
        ImageListPreview::States => (
            "3 images",
            vec![
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: true,
                    job_disabled: false,
                    preview_state: "loading".into(),
                    ..image_list_row([220, 201, 55], "IMG_9821.tif", "")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: true,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    preview_state: "missing".into(),
                    ..image_list_row([180, 171, 53], "IMG_9822.tif", "")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: true,
                    hovered: true,
                    focused: true,
                    job_disabled: false,
                    preview_state: "error".into(),
                    ..image_list_row([160, 222, 58], "IMG_9824.tif", "11.10.26\n16:44")
                },
            ],
            -1,
            -1,
            0.0,
            false,
        ),
        ImageListPreview::Tooltip => (
            "1 image",
            vec![focusforge_gui::ImageRowView {
                excluded: false,
                unresolved: false,
                selected: false,
                hovered: false,
                focused: false,
                job_disabled: false,
                preview_state: "ready".into(),
                ..image_list_row(
                    [220, 201, 55],
                    "IMG_9819_composite-exposure-calibrated-master-source-frame.tif",
                    "11.10.26\n16:42",
                )
            }],
            -1,
            -1,
            0.0,
            false,
        ),
        ImageListPreview::Dragging => (
            "3 images",
            vec![
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    preview_state: "ready".into(),
                    ..image_list_row([220, 201, 55], "IMG_9819.tif", "11.10.26\n16:42")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    preview_state: "ready".into(),
                    ..image_list_row([180, 171, 53], "IMG_9820.tif", "11.10.26\n16:42")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    preview_state: "ready".into(),
                    ..image_list_row([160, 222, 58], "IMG_9821.tif", "11.10.26\n16:42")
                },
            ],
            1,
            0,
            -120.0,
            false,
        ),
        ImageListPreview::Empty => ("0 images", Vec::new(), -1, -1, 0.0, false),
        ImageListPreview::Editor => (
            "Source legend",
            vec![
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: true,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    preview_state: "ready".into(),
                    ..image_list_row([220, 201, 55], "IMG_9819.tif", "10")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    preview_state: "ready".into(),
                    ..image_list_row([180, 171, 53], "IMG_9820.tif", "11")
                },
            ],
            -1,
            -1,
            0.0,
            true,
        ),
    };

    app.set_list_preview_active(true);
    app.set_list_preview_mode(if editor { "editor" } else { "stacking" }.into());
    app.set_list_preview_presentation("image-list".into());
    app.set_list_preview_header_text(header.into());
    app.set_list_preview_rows(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    app.set_list_preview_dragging_row(dragging);
    app.set_list_preview_drag_target_row(target);
    app.set_list_preview_drag_offset(offset);
}

/// Installs bounded source-view examples for the Large preview inspection
/// path. It shares Image list's preview-only seam and leaves controller data
/// intact.
fn apply_large_preview(app: &AppWindow, preview: Option<LargePreview>) {
    let Some(preview) = preview else {
        return;
    };

    let (header, rows, dragging, target, offset, editor) = match preview {
        LargePreview::Complete => (
            "3 images",
            vec![
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: true,
                    hovered: true,
                    focused: false,
                    job_disabled: false,
                    preview_state: "ready".into(),
                    ..image_list_row([220, 201, 55], "IMG_9819.tif", "16:42:09")
                },
                focusforge_gui::ImageRowView {
                    excluded: true,
                    unresolved: false,
                    selected: false,
                    hovered: true,
                    focused: false,
                    job_disabled: false,
                    preview_state: "ready".into(),
                    ..image_list_row([180, 171, 53], "IMG_9820.tif", "")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: true,
                    job_disabled: true,
                    preview_state: "ready".into(),
                    ..image_list_row([112, 132, 48], "IMG_9823.tif", "16:43:11")
                },
            ],
            -1,
            -1,
            0.0,
            false,
        ),
        LargePreview::States => (
            "3 images",
            vec![
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: true,
                    job_disabled: false,
                    preview_state: "loading".into(),
                    ..image_list_row([220, 201, 55], "IMG_9821.tif", "")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: true,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    preview_state: "missing".into(),
                    ..image_list_row([180, 171, 53], "IMG_9822.tif", "Unavailable")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: true,
                    hovered: true,
                    focused: true,
                    job_disabled: false,
                    preview_state: "error".into(),
                    ..image_list_row([160, 222, 58], "IMG_9824.tif", "16:44:19")
                },
            ],
            -1,
            -1,
            0.0,
            false,
        ),
        LargePreview::Tooltip => (
            "1 image",
            vec![focusforge_gui::ImageRowView {
                excluded: false,
                unresolved: false,
                selected: false,
                hovered: false,
                focused: false,
                job_disabled: false,
                preview_state: "ready".into(),
                ..image_list_row(
                    [220, 201, 55],
                    "IMG_9819_composite-exposure-calibrated-master-source-frame.tif",
                    "16:42:09",
                )
            }],
            -1,
            -1,
            0.0,
            false,
        ),
        LargePreview::Dragging => (
            "3 images",
            vec![
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    preview_state: "ready".into(),
                    ..image_list_row([220, 201, 55], "IMG_9819.tif", "16:42:09")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    preview_state: "ready".into(),
                    ..image_list_row([180, 171, 53], "IMG_9820.tif", "16:42:37")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    preview_state: "ready".into(),
                    ..image_list_row([160, 222, 58], "IMG_9821.tif", "16:43:02")
                },
            ],
            1,
            0,
            -200.0,
            false,
        ),
        LargePreview::Empty => ("0 images", Vec::new(), -1, -1, 0.0, false),
        LargePreview::Editor => (
            "Source legend",
            vec![
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: true,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    preview_state: "ready".into(),
                    ..image_list_row([220, 201, 55], "IMG_9819.tif", "10")
                },
                focusforge_gui::ImageRowView {
                    excluded: false,
                    unresolved: false,
                    selected: false,
                    hovered: false,
                    focused: false,
                    job_disabled: false,
                    preview_state: "ready".into(),
                    ..image_list_row([180, 171, 53], "IMG_9820.tif", "11")
                },
            ],
            -1,
            -1,
            0.0,
            true,
        ),
    };

    app.set_list_preview_active(true);
    app.set_list_preview_mode(if editor { "editor" } else { "stacking" }.into());
    app.set_list_preview_presentation("large".into());
    app.set_list_preview_header_text(header.into());
    app.set_list_preview_rows(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    app.set_list_preview_dragging_row(dragging);
    app.set_list_preview_drag_target_row(target);
    app.set_list_preview_drag_offset(offset);
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

/// Loads the `Recent` list, the export choice and the performance settings
/// from the same config directory `install_input_table` resolves and installs
/// them on `controller`. A directory that fails to resolve leaves the list
/// empty and the performance settings at their defaults (GPU processing on)
/// for the session, never a reason the window fails to open.
fn install_recent(
    controller: &std::sync::Arc<std::sync::Mutex<focusforge_gui::AppController>>,
    app: &AppWindow,
    args: &Args,
) {
    let Some(config_dir) = resolve_config_dir(args) else {
        controller.lock().unwrap().install_performance(
            focusforge_gui::performance_settings::PerformanceSettings::default(),
        );
        return;
    };
    let entries = focusforge_gui::recent::load(&config_dir);
    let choice = focusforge_gui::export_settings::load(&config_dir);
    let performance = focusforge_gui::performance_settings::load(&config_dir);
    let appearance = focusforge_gui::appearance_settings::load(&config_dir);
    let mut controller = controller.lock().unwrap();
    controller.install_export_choice(choice);
    controller.install_performance(performance);
    controller.install_appearance(appearance);
    app.set_appearance_presentation(controller.source_presentation().slug().into());
    controller.install_recent(entries, config_dir);
}

/// Parses a whole number of at least 1; `None` for anything else, including
/// a negative or non-numeric value.
fn parse_positive(spec: &str) -> Option<u64> {
    spec.parse::<u64>().ok().filter(|&n| n > 0)
}

/// Reads one slow-down option: logs the expected form and returns `None`
/// for a value that does not parse, leaving that option off.
fn slow_down_value(name: &str, spec: Option<&String>) -> Option<u64> {
    let spec = spec?;
    let parsed = parse_positive(spec);
    if parsed.is_none() {
        tracing::warn!("{name} expects a whole number of at least 1, got {spec:?}");
    }
    parsed
}

/// Installs `--slow-run`, `--slow-run-seconds` and `--slow-open` on the
/// controller.
fn install_slow_down(
    controller: &std::sync::Arc<std::sync::Mutex<focusforge_gui::AppController>>,
    args: &Args,
) {
    const DEFAULT_SLOW_RUN_SECONDS: u64 = 150;
    let run = slow_down_value("--slow-run", args.slow_run.as_ref());
    let seconds = slow_down_value("--slow-run-seconds", args.slow_run_seconds.as_ref());
    let hold = slow_down_value("--slow-open", args.slow_open.as_ref());
    let mut controller = controller.lock().unwrap();
    if let Some(run) = run {
        let floor = Duration::from_secs(seconds.unwrap_or(DEFAULT_SLOW_RUN_SECONDS));
        controller.set_slow_run(run as usize, floor);
    }
    if let Some(hold) = hold {
        controller.set_open_hold(Duration::from_secs(hold));
    }
}

/// `proposed`, or its file name inside the save folder when one is set.
fn save_destination(proposed: &Path) -> PathBuf {
    picker::in_save_folder(proposed).unwrap_or_else(|| proposed.to_path_buf())
}

/// Hands `--save-folder` to the pickers. A folder that is missing, not a
/// folder or not writable is logged and leaves the native save dialogs in
/// place.
fn install_save_folder(args: &Args) {
    let Some(spec) = &args.save_folder else {
        return;
    };
    let checked = std::fs::canonicalize(spec).and_then(|dir| {
        if !dir.is_dir() {
            return Err(std::io::Error::other("not a folder"));
        }
        let probe = dir.join(".focusforge-write-probe");
        std::fs::File::create_new(&probe)?;
        std::fs::remove_file(&probe)?;
        Ok(dir)
    });
    match checked {
        Ok(dir) => picker::set_save_folder(dir),
        Err(cause) => tracing::warn!(
            "--save-folder expects an existing folder you can write to, got {spec:?}: {cause}"
        ),
    }
}

/// A windowed Windows process starts without a console, so clap's help,
/// version and error text would go nowhere. Attaching the starting terminal's
/// console first lets it print there. Without a parent console (Explorer, the
/// Start menu) the call fails and nothing opens; handles the parent already
/// redirected are left alone.
#[cfg(windows)]
#[allow(unsafe_code)]
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // SAFETY: takes no pointers; the failure case is the one described above.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

#[cfg(not(windows))]
fn attach_parent_console() {}

fn main() -> ExitCode {
    // Before any thread or child process exists, so none inherits the tokens.
    let launch_tokens = take_launch_tokens();
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

    let args = Args::try_parse().unwrap_or_else(|error| {
        attach_parent_console();
        error.exit()
    });
    let result = run(&args, launch_tokens);
    focusforge_gui::bridge::finish_input_trace();
    result
}

/// Everything the arguments ask for, from the first refusal through the
/// event loop. Every ending is a returned code rather than a process exit,
/// which runs no destructor: the diagnostics value `main` holds has to drop
/// for what it buffers to reach disk. Exit codes: 0 for a window closed
/// normally, 1 for a failure while opening one, 2 for a refused argument, 3
/// for a session ended because the graphics device was lost.
fn run(args: &Args, launch_tokens: LaunchTokens) -> ExitCode {
    if let Some(section) = &args.settings_section
        && focusforge_gui::SettingsSection::from_slug(section).is_none()
    {
        let accepted = focusforge_gui::SettingsSection::ALL
            .map(|s| format!("{:?}", s.slug()))
            .join(" or ");
        tracing::error!("--settings-section accepts {accepted}, got {section:?}");
        return ExitCode::from(2);
    }
    if args.appearance_preview.is_some()
        && args.settings_section.as_deref()
            != Some(focusforge_gui::SettingsSection::Appearance.slug())
    {
        tracing::error!("--appearance-preview requires --settings-section appearance");
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
    let controller_state = AppController::new();
    let controller = Arc::new(Mutex::new(controller_state));
    let canvas: Rc<RefCell<Canvas>> = Rc::new(RefCell::new(Canvas::default()));
    let app_handle: Rc<RefCell<Option<slint::Weak<AppWindow>>>> = Rc::new(RefCell::new(None));
    // Shared with `bridge::install`'s own failure toast, so a blocked drop
    // and a blocked add never close each other's auto-dismiss timer early.
    let toast_timer: Rc<RefCell<Option<slint::Timer>>> = Rc::new(RefCell::new(None));
    let drop_unavailable = match select_backend(
        controller.clone(),
        canvas.clone(),
        app_handle.clone(),
        toast_timer.clone(),
        launch_tokens,
    ) {
        Ok(unavailable) => unavailable,
        Err(code) => return code,
    };

    let code = if args.theme_sheet {
        let sheet = ThemeSheet::new().expect("failed to instantiate ThemeSheet");
        run_window(sheet, args, None, None, None)
    } else {
        let app = AppWindow::new().expect("failed to instantiate AppWindow");
        *app_handle.borrow_mut() = Some(app.as_weak());
        app.set_drop_unavailable(drop_unavailable);
        let focusforge_gui::bridge::Installed {
            controller,
            job_timer,
            toast_timer,
            preview_timer,
        } = bridge::install(&app, controller, canvas.clone(), toast_timer);
        let _ = &job_timer;
        let _ = &toast_timer;
        let _ = &preview_timer;

        install_input_table(&controller, &app, args);
        apply_appearance_preview(&app, args.appearance_preview);
        install_diagnostics(&controller, &app);
        bridge::install_crash_notice(&controller, &app, &toast_timer);
        install_recent(&controller, &app, args);
        install_slow_down(&controller, args);
        install_save_folder(args);
        bridge::push_performance_view(&app, &controller);
        bridge::push_storage_view(&app, &controller);
        bridge::push_about_view(&app);

        if let Some(state) = &args.toolbar_state {
            let mut controller = controller.lock().unwrap();
            controller.set_toolbar_preview(state.as_str());
            bridge::push_job(&app, &controller.job_view());
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

        let undock_timer = Rc::new(slint::Timer::default());
        if let Some(pane) = args.undock_pane {
            let app_weak = app.as_weak();
            let timer = undock_timer.clone();
            let dock_after = args.dock_after;
            let reopen_after = args.reopen_after;
            let close_main_after = args.close_main_after;
            undock_timer.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(50),
                move || {
                    let Some(app) = app_weak.upgrade() else {
                        return;
                    };
                    if app.get_project_loaded() {
                        timer.stop();
                        match pane {
                            UndockPane::Frame => app.invoke_frame_undock_requested(),
                            UndockPane::Composite => app.invoke_composite_undock_requested(),
                        }
                        if let Some(ms) = dock_after {
                            let app_weak = app.as_weak();
                            slint::Timer::single_shot(Duration::from_millis(ms), move || {
                                bridge::dock_comparison_for_inspection();
                                if let Some(reopen_ms) = reopen_after {
                                    let app_weak = app_weak.clone();
                                    slint::Timer::single_shot(
                                        Duration::from_millis(reopen_ms),
                                        move || {
                                            if let Some(app) = app_weak.upgrade() {
                                                match pane {
                                                    UndockPane::Frame => {
                                                        app.invoke_frame_undock_requested()
                                                    }
                                                    UndockPane::Composite => {
                                                        app.invoke_composite_undock_requested()
                                                    }
                                                }
                                            }
                                        },
                                    );
                                }
                            });
                        }
                        if let Some(ms) = close_main_after {
                            let app_weak = app.as_weak();
                            slint::Timer::single_shot(Duration::from_millis(ms), move || {
                                if let Some(app) = app_weak.upgrade() {
                                    app.invoke_window_close_requested();
                                }
                            });
                        }
                    }
                },
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

        apply_list_preview(&app, args.list_preview);
        apply_image_list_preview(&app, args.image_list_preview);
        apply_large_preview(&app, args.large_preview);

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
                    cancel_quit_after: args.cancel_quit_after,
                },
                &open_wait_timer,
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
    fn version_flag_prints_the_binary_name_and_workspace_version() {
        let error = Args::try_parse_from(["focusforge-gui", "--version"])
            .err()
            .expect("--version is a recognised flag that stops parsing");

        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
        let text = error.render().to_string();
        assert!(text.starts_with("focusforge-gui "), "{text}");
        assert!(text.contains(env!("CARGO_PKG_VERSION")), "{text}");
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

    /// With two `--open` folders, the second tab does not exist until the
    /// load timer starts its load. A `--fuse-tabs` press for it must wait for
    /// that tab instead of landing on the first one, which ignores it.
    #[test]
    fn fuse_tabs_press_for_a_later_open_waits_for_that_tab() {
        i_slint_backend_testing::init_no_event_loop();
        let app = AppWindow::new().unwrap();
        app.window().set_size(slint::PhysicalSize::new(1440, 900));
        let dir = tempfile::tempdir().unwrap();
        let folder = |name: &str| {
            let root = dir.path().join(name);
            std::fs::create_dir(&root).unwrap();
            for index in 0..3 {
                let frame = format!("{index:02}.tif");
                std::fs::copy(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("../../fixtures/mantis-64/frames")
                        .join(&frame),
                    root.join(frame),
                )
                .unwrap();
            }
            root
        };
        let (first, second) = (folder("first"), folder("second"));
        let mut state = AppController::new();
        state.set_scratch_root_for_test(dir.path().join("scratch"));
        let controller = Arc::new(Mutex::new(state));
        let canvas: Rc<RefCell<Canvas>> = Rc::new(RefCell::new(Canvas::default()));
        let _installed = bridge::install(
            &app,
            controller.clone(),
            canvas.clone(),
            Rc::new(RefCell::new(None)),
        );

        // As `main` starts them: the first load settled, the rest left to
        // the load timer, and the press timer created after it.
        let load = controller
            .lock()
            .unwrap()
            .open_paths(std::slice::from_ref(&first))
            .expect("the first load starts");
        load.join().unwrap();
        controller.lock().unwrap().settle_open().unwrap();
        let open_slot: Rc<RefCell<Option<slint::Timer>>> = Rc::new(RefCell::new(None));
        bridge::start_load_timer(
            app.as_weak(),
            controller.clone(),
            canvas,
            &open_slot,
            vec![second],
        );
        let press_slot: Rc<RefCell<Option<slint::Timer>>> = Rc::new(RefCell::new(None));
        start_fuse_tabs_timer(
            app.as_weak(),
            controller.clone(),
            &press_slot,
            vec![0, 1],
            FuseTabsFollowUp {
                save: false,
                close: false,
                quit: false,
                overlay: false,
                enter_editor: false,
                cancel_quit_after: None,
            },
            &open_slot,
        );

        let running = |slot: &Rc<RefCell<Option<slint::Timer>>>| {
            slot.borrow().as_ref().is_some_and(slint::Timer::running)
        };
        let started = std::time::Instant::now();
        loop {
            i_slint_backend_testing::mock_elapsed_time(job::POLL);
            // The load worker's completion event, which this backend has no
            // event loop to deliver.
            if controller.lock().unwrap().settle_open().is_some() {
                bridge::push_projects(&app, &controller);
            }
            let idle = {
                let controller = controller.lock().unwrap();
                !controller.load_in_flight() && !controller.work_pending()
            };
            if idle && !running(&open_slot) && !running(&press_slot) {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(120),
                "runs never settled"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        let mut controller = controller.lock().unwrap();
        assert_eq!(controller.project_tabs().len(), 2);
        for tab in 0..2 {
            controller.select_project(tab);
            assert!(
                controller.pending_divergence().is_some(),
                "tab {tab} fused and parked at its own question"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_display_selection_prefers_x11_when_available() {
        assert_eq!(LinuxDisplayBackend::from_availability(false, false), None);
        assert_eq!(
            LinuxDisplayBackend::from_availability(true, false),
            Some(LinuxDisplayBackend::X11)
        );
        assert_eq!(
            LinuxDisplayBackend::from_availability(false, true),
            Some(LinuxDisplayBackend::Wayland)
        );
        assert_eq!(
            LinuxDisplayBackend::from_availability(true, true),
            Some(LinuxDisplayBackend::X11)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_wayland_keeps_an_explicit_validation_choice() {
        let flags = wgpu::InstanceFlags::VALIDATION;
        assert!(
            !adjust_manual_wgpu_flags(flags, LinuxDisplayBackend::Wayland, false)
                .contains(wgpu::InstanceFlags::VALIDATION)
        );
        assert!(
            adjust_manual_wgpu_flags(flags, LinuxDisplayBackend::Wayland, true)
                .contains(wgpu::InstanceFlags::VALIDATION)
        );
        assert!(
            adjust_manual_wgpu_flags(flags, LinuxDisplayBackend::X11, false)
                .contains(wgpu::InstanceFlags::VALIDATION)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_token_comes_from_the_session_variable() {
        let value = |v: &str| (!v.is_empty()).then(|| v.to_string());
        let cases = [
            // (backend, DESKTOP_STARTUP_ID, XDG_ACTIVATION_TOKEN, token)
            (LinuxDisplayBackend::X11, "x", "w", Some("x")),
            (LinuxDisplayBackend::Wayland, "x", "w", Some("w")),
            (LinuxDisplayBackend::X11, "x", "", Some("x")),
            (LinuxDisplayBackend::Wayland, "", "w", Some("w")),
            (LinuxDisplayBackend::X11, "", "w", Some("w")),
            (LinuxDisplayBackend::Wayland, "x", "", Some("x")),
            (LinuxDisplayBackend::X11, "", "", None),
            (LinuxDisplayBackend::Wayland, "", "", None),
        ];
        for (backend, x11, wayland, expected) in cases {
            assert_eq!(
                startup_token(backend, value(x11), value(wayland)),
                expected.map(str::to_string),
                "{backend:?}, x11 {x11:?}, wayland {wayland:?}"
            );
        }
        // An empty variable reads as an unset one.
        assert_eq!(
            startup_token(
                LinuxDisplayBackend::X11,
                Some(String::new()),
                Some(String::new())
            ),
            None
        );
    }
}
