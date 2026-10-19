// SPDX-License-Identifier: GPL-3.0-or-later

//! Windowed tests: they launch the real binary, so they need a compositor
//! and a GPU adapter. Guarded on `DISPLAY`/`WAYLAND_DISPLAY`, printing an
//! explicit skip line and passing rather than reddening
//! `cargo test --workspace` on a headless gate host.

mod common;

#[cfg(feature = "mcp")]
use std::io::{Read, Write};
#[cfg(feature = "mcp")]
use std::net::{TcpListener, TcpStream};
#[cfg(feature = "mcp")]
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::has_display;

#[test]
fn comparison_window_geometry() {
    use focusforge_gui::ComparisonWindow;
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;

    i_slint_backend_testing::init_no_event_loop();
    let window = ComparisonWindow::new().unwrap();
    window.window().set_size(slint::PhysicalSize::new(820, 600));
    let rect = |id| {
        let element = ElementQuery::from_root(&window)
            .match_id(id)
            .find_first()
            .unwrap_or_else(|| panic!("missing {id}"));
        let position = element.absolute_position();
        let size = element.size();
        (position.x, position.y, size.width, size.height)
    };
    assert_eq!(rect("ComparisonWindow::bar"), (0.0, 0.0, 820.0, 28.0));
    assert_eq!(rect("ComparisonWindow::pane"), (1.0, 29.0, 818.0, 570.0));
}

#[test]
fn comparison_current_crop_matches_the_visible_draw() {
    use focusforge_gui::ComparisonWindow;
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;

    i_slint_backend_testing::init_no_event_loop();
    let window = ComparisonWindow::new().unwrap();
    window.window().set_size(slint::PhysicalSize::new(820, 600));
    window.set_crop_x(18.0);
    window.set_crop_y(31.0);
    window.set_crop_w(400.0);
    window.set_crop_h(250.0);
    let pane = ElementQuery::from_root(&window)
        .match_id("ComparisonWindow::pane")
        .find_first()
        .expect("comparison pane exists");
    let crop = ElementQuery::from_root(&window)
        .match_id("FFPane::image-crop")
        .find_first()
        .expect("comparison crop exists");
    let pane_pos = pane.absolute_position();
    let crop_pos = crop.absolute_position();
    let crop_size = crop.size();
    assert_eq!(
        (crop_pos.x, crop_pos.y),
        (pane_pos.x + 18.0, pane_pos.y + 31.0)
    );
    assert_eq!((crop_size.width, crop_size.height), (400.0, 250.0));
    assert!(
        window.get_crop_clips(),
        "the visible crop must clip retained texture coverage outside its current draw"
    );
}

#[test]
fn comparison_backdrop_sits_under_the_exact_image_outside_its_crop() {
    use focusforge_gui::ComparisonWindow;
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;

    i_slint_backend_testing::init_no_event_loop();
    let window = ComparisonWindow::new().unwrap();
    window.window().set_size(slint::PhysicalSize::new(820, 600));
    let images = |window: &ComparisonWindow| {
        ElementQuery::from_root(window)
            .match_type_name("Image")
            .find_all()
            .into_iter()
            .map(|image| {
                let position = image.absolute_position();
                let size = image.size();
                (position.x, position.y, size.width, size.height)
            })
            .collect::<Vec<_>>()
    };
    let backdrop_rect = (-40.0, 12.0, 700.0, 333.0);
    let at = |rect: (f32, f32, f32, f32)| (1.0 + rect.0, 29.0 + rect.1, rect.2, rect.3);
    assert!(
        !images(&window).contains(&at(backdrop_rect)),
        "no backdrop is drawn before one is set"
    );

    window.set_crop_x(18.0);
    window.set_crop_y(31.0);
    window.set_crop_w(400.0);
    window.set_crop_h(250.0);
    window.set_backdrop(slint::Image::from_rgba8(slint::SharedPixelBuffer::<
        slint::Rgba8Pixel,
    >::new(4, 3)));
    window.set_backdrop_x(backdrop_rect.0);
    window.set_backdrop_y(backdrop_rect.1);
    window.set_backdrop_w(backdrop_rect.2);
    window.set_backdrop_h(backdrop_rect.3);
    assert!(
        images(&window).contains(&at(backdrop_rect)),
        "the backdrop keeps its own placement, not the crop's: {:?}",
        images(&window)
    );
}

#[test]
fn comparison_window_forwards_focused_keys_and_focus_loss() {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use focusforge_gui::ComparisonWindow;
    use slint::ComponentHandle;
    use slint::platform::WindowEvent;

    i_slint_backend_testing::init_no_event_loop();
    let window = ComparisonWindow::new().unwrap();
    let events = Rc::new(RefCell::new(Vec::new()));
    let focus_changes = Rc::new(Cell::new(0));
    window.on_pane_key_pressed({
        let events = events.clone();
        move |text, _, _, _| events.borrow_mut().push(format!("down:{text}"))
    });
    window.on_pane_key_released({
        let events = events.clone();
        move |text, _, _, _| events.borrow_mut().push(format!("up:{text}"))
    });
    window.on_pane_focus_changed({
        let focus_changes = focus_changes.clone();
        move || focus_changes.set(focus_changes.get() + 1)
    });
    window
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: " ".into() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: " ".into() });
    window
        .window()
        .dispatch_event(WindowEvent::WindowActiveChanged(false));
    assert_eq!(events.borrow().as_slice(), ["down: ", "up: "]);
    assert!(focus_changes.get() > 0);
}

#[test]
fn comparison_hover_ends_outside_its_pane() {
    use focusforge_gui::ComparisonWindow;
    use slint::ComponentHandle;
    use slint::platform::WindowEvent;

    i_slint_backend_testing::init_no_event_loop();
    let window = ComparisonWindow::new().unwrap();
    window.window().set_size(slint::PhysicalSize::new(820, 600));
    window.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(400.0, 300.0),
    });
    assert!(window.get_pane_hovered());
    window.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(400.0, 10.0),
    });
    assert!(!window.get_pane_hovered());
    window.window().dispatch_event(WindowEvent::PointerExited);
    assert!(!window.get_pane_hovered());
}

#[test]
fn undock_without_project_is_empty() {
    use focusforge_gui::canvas::PaneSource;
    use focusforge_gui::{AppController, ComparisonWindow};
    use i_slint_backend_testing::ElementQuery;

    i_slint_backend_testing::init_no_event_loop();
    let mut controller = AppController::new();
    controller.undock_requested(PaneSource::Frame);
    assert_eq!(controller.detached_pane(), Some(PaneSource::Frame));

    let window = ComparisonWindow::new().unwrap();
    assert!(!window.get_project_loaded());
    for id in ["FFPane::pane-label", "FFPane::pane-undock"] {
        assert!(
            ElementQuery::from_root(&window)
                .match_id(id)
                .find_all()
                .is_empty(),
            "{id} appears without a project"
        );
    }
}

#[test]
fn dock_back_restores_two_panes() {
    use focusforge_gui::canvas::PaneSource;
    use focusforge_gui::{AppController, AppWindow};
    use slint::ComponentHandle;

    i_slint_backend_testing::init_no_event_loop();
    let app = AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    let mut controller = AppController::new();
    controller.undock_requested(PaneSource::Frame);
    app.set_detached_pane("frame".into());
    assert_eq!(app.get_canvas_pane_left_width(), 0.0);
    assert_eq!(app.get_canvas_pane_right_width(), 1100.0);

    controller.undocked_window_closed(PaneSource::Frame);
    app.set_detached_pane("".into());
    assert_eq!(controller.detached_pane(), None);
    assert_eq!(app.get_canvas_pane_left_width(), 542.0);
    assert_eq!(app.get_canvas_pane_right_width(), 542.0);
}

#[test]
fn comparison_capture_requires_detached_pane() {
    if !has_display() {
        println!("SKIP: window tests need DISPLAY or WAYLAND_DISPLAY");
        return;
    }
    let output = common::gui_bin("comparison_capture_requires_detached_pane")
        .args(["--exit-after", "500", "--screenshot-window", "comparison"])
        .arg("--screenshot")
        .arg(common::workspace_root().join(".scratch/gui-screenshots/no-detached-pane.png"))
        .output()
        .expect("launch focusforge-gui");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--screenshot-window comparison requires a detached pane")
    );
}

#[test]
fn refused_slow_down_values_are_logged_and_the_window_opens() {
    if !has_display() {
        println!("SKIP: window tests need DISPLAY or WAYLAND_DISPLAY");
        return;
    }
    let cases: [(&[&str], &str); 5] = [
        (&["--slow-run", "abc"], "--slow-run expects"),
        (&["--slow-run", "-1"], "--slow-run expects"),
        (&["--slow-run", "0"], "--slow-run expects"),
        (&["--slow-run-seconds", "x"], "--slow-run-seconds expects"),
        (&["--slow-open", "x"], "--slow-open expects"),
    ];
    for (i, (args, expected)) in cases.into_iter().enumerate() {
        let output = common::gui_bin(&format!("refused_slow_down_{i}"))
            .args(args)
            .args(["--exit-after", "500"])
            .output()
            .expect("launch focusforge-gui");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?} must still open the window: {stderr}"
        );
        assert!(stderr.contains(expected), "{args:?}: stderr was {stderr}");
    }
}

#[test]
fn refused_save_folder_is_logged_and_the_window_opens() {
    if !has_display() {
        println!("SKIP: window tests need DISPLAY or WAYLAND_DISPLAY");
        return;
    }
    let dir = common::tree("refused_save_folder");
    let file = dir.join("not-a-folder");
    std::fs::write(&file, b"x").expect("write the regular file");
    let mut refused = vec![dir.join("missing"), file];
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let locked = dir.join("locked");
        std::fs::create_dir(&locked).expect("create the folder");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555))
            .expect("make the folder read-only");
        // A superuser writes anyway, so the folder would not be refused.
        if std::fs::File::create(locked.join("probe")).is_err() {
            refused.push(locked);
        }
    }
    for (i, path) in refused.iter().enumerate() {
        let output = common::gui_bin(&format!("refused_save_folder_{i}"))
            .arg("--save-folder")
            .arg(path)
            .args(["--exit-after", "500"])
            .output()
            .expect("launch focusforge-gui");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{path:?} must still open the window: {stderr}"
        );
        assert!(
            stderr.contains("--save-folder expects"),
            "{path:?}: {stderr}"
        );
        assert!(
            stderr.contains(&path.display().to_string()),
            "{path:?}: {stderr}"
        );
    }
}

#[test]
fn both_sides_open_a_real_comparison_window() {
    if !has_display() {
        println!("SKIP: comparison windows need DISPLAY or WAYLAND_DISPLAY");
        return;
    }
    let dir = common::tree("both_comparison_sides");
    for side in ["frame", "composite"] {
        let capture = dir.join(format!("{side}.png"));
        let output = common::gui_bin(&format!("comparison_{side}"))
            .arg("--open")
            .arg(common::fixture_frame("mantis-64", 0))
            .args(["--undock-pane", side, "--exit-after", "8000"])
            .args(["--screenshot-window", "comparison", "--screenshot"])
            .arg(&capture)
            .output()
            .expect("launch comparison window");
        assert!(
            output.status.success(),
            "{side} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let bytes = std::fs::read(&capture).expect("detached window capture");
        let info = png::Decoder::new(std::io::Cursor::new(bytes))
            .read_info()
            .expect("valid detached PNG");
        assert!(info.info().width >= 820 && info.info().height >= 600);
    }
}

#[test]
fn comparison_reopens_after_dock_and_main_close_exits() {
    if !has_display() {
        println!("SKIP: comparison windows need DISPLAY or WAYLAND_DISPLAY");
        return;
    }
    let capture = common::tree("comparison_reopen").join("reopened.png");
    let output = common::gui_bin("comparison_reopen")
        .arg("--open")
        .arg(common::fixture_frame("mantis-64", 0))
        .args([
            "--undock-pane",
            "frame",
            "--dock-after",
            "500",
            "--reopen-after",
            "500",
            "--exit-after",
            "8000",
            "--screenshot-window",
            "comparison",
            "--screenshot",
        ])
        .arg(&capture)
        .output()
        .expect("launch reopened comparison");
    assert!(
        output.status.success(),
        "reopen failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(capture.exists(), "reopened comparison was not captured");

    let watchdog_capture = common::tree("comparison_main_close").join("watchdog.png");
    let output = common::gui_bin("comparison_main_close")
        .arg("--open")
        .arg(common::fixture_frame("mantis-64", 0))
        .args([
            "--undock-pane",
            "frame",
            "--close-main-after",
            "500",
            "--exit-after",
            "15000",
            "--screenshot",
        ])
        .arg(&watchdog_capture)
        .output()
        .expect("launch comparison and close main");
    assert!(
        output.status.success(),
        "main close failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !watchdog_capture.exists(),
        "main close did not exit before watchdog"
    );
}

#[test]
fn cancelling_a_guarded_main_quit_keeps_both_real_windows_rendering() {
    if !has_display() {
        println!("SKIP: comparison windows need DISPLAY or WAYLAND_DISPLAY");
        return;
    }
    let capture = common::tree("comparison_cancelled_main_quit").join("comparison.png");
    let started = Instant::now();
    let output = common::gui_bin("comparison_cancelled_main_quit")
        .arg("--open")
        .arg(common::fixture_frame("mantis-64", 0))
        .args([
            "--fuse-tabs",
            "0",
            "--undock-pane",
            "frame",
            "--cancel-quit-after",
            "500",
            "--exit-after",
            "8000",
            "--screenshot-window",
            "comparison",
            "--screenshot",
        ])
        .arg(&capture)
        .output()
        .expect("launch comparison and cancel guarded main quit");
    assert!(
        output.status.success(),
        "cancelled main quit failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        started.elapsed() >= Duration::from_secs(7),
        "the main event loop exited before its scheduled inspection end"
    );
    let bytes = std::fs::read(&capture).expect("comparison survives the cancelled quit");
    png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .expect("comparison stays renderable after a cancelled main quit");
}

#[test]
fn comparison_chrome_intents_do_not_change_main_chrome() {
    use focusforge_gui::{AppController, ShellEvent, WindowChromeIntent, WindowTarget};
    use std::sync::{Arc, Mutex};

    let mut controller = AppController::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    controller.set_sink(Arc::new(move |event| {
        sink_events.lock().unwrap().push(event)
    }));

    controller.window_chrome(WindowTarget::Comparison, WindowChromeIntent::Minimize);
    controller.window_chrome(WindowTarget::Comparison, WindowChromeIntent::ToggleMaximize);
    controller.window_chrome(WindowTarget::Comparison, WindowChromeIntent::ToggleMaximize);

    assert!(
        !controller.maximized(),
        "comparison chrome cannot alter main chrome"
    );
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [
            ShellEvent::WindowChrome {
                target: WindowTarget::Comparison,
                intent: WindowChromeIntent::Minimize,
            },
            ShellEvent::WindowChrome {
                target: WindowTarget::Comparison,
                intent: WindowChromeIntent::ToggleMaximize,
            },
            ShellEvent::WindowChrome {
                target: WindowTarget::Comparison,
                intent: WindowChromeIntent::ToggleMaximize,
            },
        ]
    );
}

#[cfg(feature = "gpu-tests")]
#[test]
fn comparison_device_loss_exits_cleanly() {
    if !has_display() {
        println!("SKIP: comparison windows need DISPLAY or WAYLAND_DISPLAY");
        return;
    }
    let output = common::gui_bin("comparison_device_loss")
        .arg("--open")
        .arg(common::fixture_frame("mantis-64", 0))
        .args(["--undock-pane", "frame", "--exit-after", "5000"])
        .env("FF_FORCE_DEVICE_LOST_MS", "1500")
        .output()
        .expect("launch comparison and destroy device");
    assert_eq!(output.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("graphics device is gone"), "{stderr}");
    assert!(!stderr.contains("panicked at"), "{stderr}");
}

#[cfg(target_os = "linux")]
#[test]
fn missing_linux_display_fails_clearly() {
    let output = common::gui_bin("missing_linux_display")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .expect("launch focusforge-gui");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("no Linux display is available: set DISPLAY or WAYLAND_DISPLAY")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn unusable_x11_display_fails_clearly() {
    let output = common::gui_bin("unusable_x11_display")
        .env("DISPLAY", ":99999")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .expect("launch focusforge-gui");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains("display") || stderr.contains("x11"),
        "no display error: {stderr}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn unusable_wayland_display_fails_clearly() {
    let output = common::gui_bin("unusable_wayland_display")
        .env_remove("DISPLAY")
        .env("WAYLAND_DISPLAY", "wayland-missing")
        .output()
        .expect("launch focusforge-gui");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains("display") || stderr.contains("wayland"),
        "no Wayland display error: {stderr}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn wayland_only_launch_reports_wayland() {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        println!("SKIP: Wayland display unavailable");
        return;
    }
    let output = common::gui_bin("wayland_only_backend")
        .env_remove("DISPLAY")
        .args(["--exit-after", "1500"])
        .output()
        .expect("launch focusforge-gui on Wayland");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("selected backend: winit-wayland"),
        "{stdout}"
    );
    assert!(
        stdout.contains("resolved native display: winit-wayland"),
        "{stdout}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn x11_only_launch_reports_x11() {
    if std::env::var_os("DISPLAY").is_none() {
        println!("SKIP: X11 display unavailable");
        return;
    }
    let output = common::gui_bin("x11_only_backend")
        .env_remove("WAYLAND_DISPLAY")
        .args(["--exit-after", "1500"])
        .output()
        .expect("launch focusforge-gui on X11");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("selected backend: winit-x11"), "{stdout}");
    assert!(
        stdout.contains("resolved native display: winit-x11"),
        "{stdout}"
    );
}

#[test]
fn screenshot_is_not_uniform() {
    if !has_display() {
        println!("SKIP: window tests need DISPLAY or WAYLAND_DISPLAY");
        return;
    }

    let out_path = common::workspace_root()
        .join(".scratch")
        .join("gui-screenshots")
        .join("screenshot_is_not_uniform.png");
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }

    let status = common::gui_bin("window")
        .arg("--exit-after")
        .arg("3000")
        .arg("--screenshot")
        .arg(&out_path)
        .status()
        .expect("failed to launch focusforge-gui");
    assert!(status.success(), "focusforge-gui exited with {status:?}");

    let bytes = std::fs::read(&out_path).unwrap();
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().unwrap();
    let info = reader.info();
    // Physical pixels, not logical: the host's own DPI/scale factor (unset by
    // this test, so whatever winit reports for the display it runs on)
    // multiplies the 1440x900 base the window opens at. Assert the invariant
    // that survives across hosts -- both dimensions scaled by the same
    // factor, and that factor plausible -- rather than a fixed pixel count.
    let scale_x = f64::from(info.width) / 1440.0;
    let scale_y = f64::from(info.height) / 900.0;
    assert!(
        (scale_x - scale_y).abs() < 0.01,
        "screenshot scale factor differs between width and height: {scale_x} vs {scale_y} ({}x{})",
        info.width,
        info.height
    );
    assert!(
        (0.5..=4.0).contains(&scale_x),
        "implausible screenshot scale factor {scale_x} ({}x{})",
        info.width,
        info.height
    );

    let mut buf = vec![0u8; reader.output_buffer_size().expect("PNG has no frames")];
    reader.next_frame(&mut buf).unwrap();

    let mut colours = std::collections::HashSet::new();
    for px in buf.chunks(4) {
        colours.insert((px[0], px[1], px[2]));
        if colours.len() > 1 {
            break;
        }
    }
    assert!(colours.len() > 1, "screenshot is a single uniform colour");
}

/// Closing the window while the background manifest scan is still in
/// flight must exit cleanly: a ~20 MB seeded data directory keeps the scan
/// running well past the 100 ms `--exit-after` deadline (a synchronous scan
/// over the same shape of data measures around 0.7 s of wall time).
#[test]
fn window_closed_during_scan() {
    if !has_display() {
        println!("SKIP: window tests need DISPLAY or WAYLAND_DISPLAY");
        return;
    }

    let mut cmd = common::gui_bin("window_closed_during_scan");
    let ff_data_dir = cmd
        .get_envs()
        .find(|(k, _)| *k == "FOCUSFORGE_DATA_DIR")
        .and_then(|(_, v)| v)
        .map(std::path::PathBuf::from)
        .expect("gui_bin always sets FOCUSFORGE_DATA_DIR");
    common::seed_large_diagnostics_dir(&ff_data_dir, 20 * 1024 * 1024);
    let crashes = ff_data_dir.join("focusforge").join("crashes");
    let before: std::collections::BTreeSet<_> = std::fs::read_dir(&crashes)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();

    let output = cmd
        .args(["--exit-after", "100"])
        .output()
        .expect("failed to launch focusforge-gui");
    assert!(
        output.status.success(),
        "focusforge-gui exited with {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let after: std::collections::BTreeSet<_> = std::fs::read_dir(&crashes)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        before, after,
        "closing mid-scan must leave no new crash report"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked at"),
        "closing mid-scan must print no panic text, got: {stderr}"
    );
}

#[cfg(feature = "mcp")]
fn unused_local_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test port");
    listener.local_addr().expect("read test port").port()
}

#[cfg(feature = "mcp")]
fn read_http_response(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set MCP read timeout");
    let mut response = Vec::new();
    loop {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).expect("read MCP response");
        assert_ne!(read, 0, "MCP server closed before its complete response");
        response.extend_from_slice(&chunk[..read]);

        let Some(header_end) = response.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&response[..header_end]).expect("MCP headers are UTF-8");
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .expect("MCP response has Content-Length")
            .parse::<usize>()
            .expect("MCP Content-Length is a number");
        if response.len() >= header_end + 4 + content_length {
            return String::from_utf8(response).expect("MCP response is UTF-8");
        }
    }
}

#[cfg(feature = "mcp")]
fn initialize_mcp(port: u16) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"focusforge-test","version":"1"}}}"#;
            write!(
                stream,
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("send MCP initialize");
            return read_http_response(&mut stream);
        }
        assert!(
            Instant::now() < deadline,
            "MCP server never listened on {port}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[cfg(feature = "mcp")]
fn mcp_feature_exposes_the_running_window() {
    if !has_display() {
        println!("SKIP: window tests need DISPLAY or WAYLAND_DISPLAY");
        return;
    }

    let port = unused_local_port();
    let mut child = common::gui_bin("mcp")
        .env("SLINT_MCP_PORT", port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch focusforge-gui with MCP");
    let response = initialize_mcp(port);
    let _ = child.kill();
    let _ = child.wait();

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "initialize failed: {response}"
    );
    assert!(
        response.contains("list_windows"),
        "tools absent: {response}"
    );
}
