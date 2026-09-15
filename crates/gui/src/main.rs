// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-gui`'s entry point. Every flag below exists so the window
//! can be observed without a person in front of it (steps 9-13).

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use focusforge_gui::{AppWindow, ThemeSheet, bridge, screenshot};
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
/// composite/Z-Map texture off the CPU round-trip. No usable adapter ->
/// print the named `GpuError` plus the still-working CLI path and exit:
/// no CPU-tone-mapped fallback, no `Automatic` fallback.
fn select_backend() {
    let mut settings = slint::wgpu_29::WGPUSettings::default();
    settings.device_required_limits = slint::wgpu_29::wgpu::Limits::default();
    if let Err(e) = slint::BackendSelector::new()
        .require_wgpu_29(slint::wgpu_29::WGPUConfiguration::Automatic(settings))
        .select()
    {
        eprintln!("{e}");
        std::process::exit(1);
    }
    // The crate's feature set pins exactly one backend/renderer
    // combination, so "resolved" is what was compiled in.
    println!("resolved backend: winit, renderer: renderer-femtovg-wgpu (wgpu 29.0.4)");
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
                eprintln!("--size expects WxH, got {spec:?}");
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
                            eprintln!("--screenshot failed: {e}");
                            exit_code.set(1);
                        }
                    }
                    Err(e) => {
                        eprintln!("take_snapshot failed: {e}");
                        exit_code.set(1);
                    }
                }
            }

            let _ = slint::quit_event_loop();
        });
    }

    if let Err(e) = app.run() {
        eprintln!("event loop exited with an error: {e}");
        exit_code.set(1);
    }
    exit_code.get()
}

fn main() {
    let args = Args::parse();

    select_backend();

    let code = if args.theme_sheet {
        let sheet = ThemeSheet::new().expect("failed to instantiate ThemeSheet");
        run_window(sheet, &args)
    } else {
        let app = AppWindow::new().expect("failed to instantiate AppWindow");
        let _controller = bridge::install(&app);
        run_window(app, &args)
    };

    std::process::exit(code);
}
