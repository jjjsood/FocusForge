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
#[cfg(feature = "mcp")]
use std::time::{Duration, Instant};

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
