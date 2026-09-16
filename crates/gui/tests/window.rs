// SPDX-License-Identifier: GPL-3.0-or-later

//! Windowed tests: they launch the real binary, so they need a compositor
//! and a GPU adapter. Guarded on `DISPLAY`/`WAYLAND_DISPLAY`, printing an
//! explicit skip line and passing rather than reddening
//! `cargo test --workspace` on a headless gate host.

use std::process::Command;

mod common;

/// `true` when this host can plausibly open a window.
fn has_display() -> bool {
    std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
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

    let bin = env!("CARGO_BIN_EXE_focusforge-gui");
    let status = Command::new(bin)
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
