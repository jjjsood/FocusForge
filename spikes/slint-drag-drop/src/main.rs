// SPDX-License-Identifier: GPL-3.0-or-later
//! Spike: observe whether files and folders dropped on a Slint window arrive as usable
//! filesystem paths, and through which route — Slint's public `DropArea` API (in-process only
//! on this pin) or winit's `WindowEvent::{DroppedFile,HoveredFile,HoveredFileCancelled}` reached
//! via a `CustomApplicationHandler`. Throwaway spike — nothing here is meant to be
//! reused.

use std::path::PathBuf;
use std::time::Duration;

use slint::winit_030::{CustomApplicationHandler, EventResult, WinitWindowAccessor, winit};
use winit::event_loop::ActiveEventLoop;
use winit::platform::wayland::WindowExtWayland;
use winit::window::WindowId;

slint::slint! {
    export component AppWindow inherits Window {
        preferred-width: 520px;
        preferred-height: 240px;
        title: "slint-drag-drop-spike";
        callback public-api-drop();
        DropArea {
            can-drop(e) => { DragAction.copy }
            dropped(e) => { root.public-api-drop(); DragAction.copy }
        }
        Text { text: "drop files here"; }
    }
}

struct Config {
    exit_after_ms: Option<u64>,
}

fn parse_args() -> Config {
    let mut exit_after_ms = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--exit-after" => {
                let v = args.next().expect("--exit-after needs a value");
                exit_after_ms = Some(v.parse().expect("--exit-after needs an integer"));
            }
            other => panic!("unknown argument {other}"),
        }
    }

    Config { exit_after_ms }
}

/// Winit-level drop observer, installed via `BackendSelector::with_winit_custom_application_handler`
/// — the only route to winit events on this pin, since `slint::winit_030` does not re-export
/// `Backend` / `BackendBuilder`. Both methods return `EventResult::Propagate` so Slint still sees
/// everything.
#[derive(Default)]
struct DropSpy {
    pending: Vec<PathBuf>,
    gesture: u32,
}

impl CustomApplicationHandler for DropSpy {
    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _winit_window: Option<&winit::window::Window>,
        _slint_window: Option<&slint::Window>,
        event: &winit::event::WindowEvent,
    ) -> EventResult {
        match event {
            winit::event::WindowEvent::DroppedFile(path) => {
                let kind = if path.is_dir() {
                    "dir"
                } else if path.exists() {
                    "file"
                } else {
                    "missing"
                };
                println!("winit dropped-file: {} kind={kind}", path.display());
                self.pending.push(path.clone());
            }
            winit::event::WindowEvent::HoveredFile(path) => {
                println!("winit hovered-file: {}", path.display());
            }
            winit::event::WindowEvent::HoveredFileCancelled => {
                println!("winit hovered-file-cancelled");
            }
            _ => {}
        }
        EventResult::Propagate
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) -> EventResult {
        if !self.pending.is_empty() {
            self.gesture += 1;
            let dirs = self.pending.iter().filter(|p| p.is_dir()).count();
            let files = self.pending.len() - dirs;
            println!(
                "drop-gesture {}: count={} dirs={} files={} source=raw-winit-event",
                self.gesture,
                self.pending.len(),
                dirs,
                files
            );
            self.pending.clear();
        }
        EventResult::Propagate
    }
}

fn main() {
    let config = parse_args();

    slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_custom_application_handler(DropSpy::default())
        .select()
        .expect("backend selection failed");

    let app = AppWindow::new().expect("failed to instantiate AppWindow");

    app.on_public_api_drop(|| {
        // Expected never to print for a file-manager drop: Slint's DropArea is in-process only
        // on this pin.
        println!("source=slint-DropArea: a drop reached the public API");
    });

    let app_weak = app.as_weak();
    slint::Timer::single_shot(Duration::from_millis(200), move || {
        let Some(app) = app_weak.upgrade() else {
            return;
        };
        // A winit window only exists once the event loop is active, which is why this is a timer
        // and not a line printed before `run()`.
        let is_wayland = app
            .window()
            .with_winit_window(|w| w.xdg_toplevel().is_some())
            .unwrap_or(false);
        let display_server = if is_wayland { "wayland" } else { "x11" };
        let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
        println!(
            "backend=winit renderer=software display-server={display_server} session={session}"
        );
    });

    if let Some(ms) = config.exit_after_ms {
        slint::Timer::single_shot(Duration::from_millis(ms), || {
            slint::quit_event_loop().expect("quit_event_loop failed");
        });
    }

    app.run().expect("event loop failed");
}
