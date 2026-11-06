// SPDX-License-Identifier: GPL-3.0-or-later

//! The HTTPS release source against a fake server on 127.0.0.1.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use focusforge_core::update::{CheckFailure, HttpReleaseSource, ReleaseSource, UpdateCheck, check};

const PATH: &str = "/repos/o/r/releases/latest";
const MAX_BODY: usize = 1 << 20;

const RELEASE: &str = r#"{"tag_name":"v9.9.9","html_url":"https://github.com/o/r/releases/tag/v9.9.9","prerelease":false,"draft":false}"#;

/// What the fake server does with the one connection it accepts.
enum Behaviour {
    /// Write these bytes, then close.
    Answer(Vec<u8>),
    /// Read the request and keep the connection open without answering.
    Silent,
}

/// Starts a one-shot server; the handle yields the request text it received.
fn serve(behaviour: Behaviour) -> (HttpReleaseSource, JoinHandle<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test port");
    let port = listener.local_addr().expect("read test port").port();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept the one connection");
        let mut request = Vec::new();
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let mut chunk = [0u8; 1024];
            let read = stream.read(&mut chunk).expect("read request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        match behaviour {
            // A client that stops reading early makes the write fail; that is the point.
            Behaviour::Answer(bytes) => drop(stream.write_all(&bytes)),
            Behaviour::Silent => thread::sleep(Duration::from_secs(8)),
        }
        String::from_utf8_lossy(&request).into_owned()
    });
    let url = format!("http://127.0.0.1:{port}{PATH}");
    (HttpReleaseSource::with_url(url), handle)
}

fn response(status_line: &str, extra_headers: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status_line}\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn ok(body: &[u8]) -> Behaviour {
    Behaviour::Answer(response("200 OK", "", body))
}

fn chunked(body: &[u8]) -> Behaviour {
    let mut out =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    for part in body.chunks(7) {
        out.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
        out.extend_from_slice(part);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    Behaviour::Answer(out)
}

#[test]
fn http_release_source_returns_body() {
    let (source, server) = serve(ok(RELEASE.as_bytes()));
    let body = source.fetch().expect("a 200 answer is returned");
    assert_eq!(body, RELEASE);
    server.join().unwrap();

    let (source, server) = serve(ok(RELEASE.as_bytes()));
    match check(&source, "0.1.0", None) {
        UpdateCheck::Newer { version, .. } => assert_eq!(version.to_string(), "9.9.9"),
        other => panic!("expected Newer, got {other:?}"),
    }
    server.join().unwrap();

    let text = "Größe ✓ 日本語 — {\"k\":\"é\"}";
    let (source, server) = serve(chunked(text.as_bytes()));
    assert_eq!(source.fetch().expect("chunked answer"), text);
    server.join().unwrap();
}

#[test]
fn http_release_source_sends_no_identifier() {
    let (source, server) = serve(ok(RELEASE.as_bytes()));
    source.fetch().expect("answer");
    let request = server.join().unwrap();

    let mut lines = request.split("\r\n");
    assert_eq!(
        lines.next(),
        Some("GET /repos/o/r/releases/latest HTTP/1.1")
    );
    let headers: Vec<(String, String)> = lines
        .take_while(|l| !l.is_empty())
        .map(|l| {
            let (name, value) = l.split_once(':').expect("header line has a colon");
            (name.to_ascii_lowercase(), value.trim().to_owned())
        })
        .collect();
    for (name, _) in &headers {
        assert!(
            ["host", "user-agent", "accept", "connection"].contains(&name.as_str()),
            "unexpected request header {name}"
        );
    }
    let get = |wanted: &str| {
        headers
            .iter()
            .find(|(n, _)| n == wanted)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(
        get("user-agent"),
        Some(concat!("FocusForge/", env!("CARGO_PKG_VERSION")))
    );
    assert_eq!(get("accept"), Some("application/vnd.github+json"));
}

#[test]
fn http_release_source_times_out() {
    let (source, _server) = serve(Behaviour::Silent);
    let started = Instant::now();
    let result = source.fetch();
    let elapsed = started.elapsed();
    let error = result.expect_err("a silent server must give Err");
    assert!(error.contains("timeout: global"), "timeout error: {error}");
    assert!(
        elapsed >= Duration::from_secs(5) && elapsed < Duration::from_secs(7),
        "timeout took {elapsed:?}"
    );
}

#[test]
fn http_release_source_refused_is_error() {
    let port = TcpListener::bind(("127.0.0.1", 0))
        .expect("bind test port")
        .local_addr()
        .expect("read test port")
        .port();
    let source = HttpReleaseSource::with_url(format!("http://127.0.0.1:{port}{PATH}"));
    let started = Instant::now();
    let result = source.fetch();
    let error = result.expect_err("a refused port must give Err");
    assert!(
        error.to_lowercase().contains("refused"),
        "refused error: {error}"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn http_release_source_non_200_is_error() {
    // The 301 points back at the fake server: a followed redirect would end in a
    // connection error instead of the status text.
    let location = format!("Location: {PATH}\r\n");
    for (status_line, headers, status) in [
        ("301 Moved Permanently", location.as_str(), 301),
        ("204 No Content", "", 204),
        ("403 Forbidden", "", 403),
        ("404 Not Found", "", 404),
        ("429 Too Many Requests", "", 429),
        ("500 Internal Server Error", "", 500),
    ] {
        let (source, server) = serve(Behaviour::Answer(response(
            status_line,
            headers,
            RELEASE.as_bytes(),
        )));
        assert_eq!(
            source.fetch(),
            Err(format!("unexpected HTTP status {status}")),
            "status {status}"
        );
        server.join().unwrap();
    }
}

#[test]
fn http_release_source_bad_body() {
    let exact = "a".repeat(MAX_BODY);
    let (source, server) = serve(ok(exact.as_bytes()));
    assert_eq!(source.fetch().expect("exactly the cap").len(), MAX_BODY);
    server.join().unwrap();

    let over = "a".repeat(MAX_BODY + 1);
    let (source, server) = serve(ok(over.as_bytes()));
    let error = source
        .fetch()
        .expect_err("one byte over the cap must be Err");
    assert!(
        error.contains("larger than request limit"),
        "over-cap error: {error}"
    );
    server.join().unwrap();

    let (source, server) = serve(ok(&[0xff, 0xfe, 0x00, b'{']));
    let error = source.fetch().expect_err("non-UTF-8 must be Err");
    assert!(error.contains("valid UTF-8"), "non-UTF-8 error: {error}");
    server.join().unwrap();

    let (source, server) = serve(ok(b"<html><body>captive portal</body></html>"));
    assert!(matches!(
        check(&source, "0.1.0", None),
        UpdateCheck::Failed(CheckFailure::Unreadable(_))
    ));
    server.join().unwrap();

    let (source, server) = serve(ok(over.as_bytes()));
    assert!(matches!(
        check(&source, "0.1.0", None),
        UpdateCheck::Failed(CheckFailure::Source(_))
    ));
    server.join().unwrap();
}
