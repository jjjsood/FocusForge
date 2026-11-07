// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-level coverage of `check-update` against a one-connection fake
//! release server on 127.0.0.1, reached through `FOCUSFORGE_RELEASE_URL`.

mod common;

use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::process::Output;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use focusforge_core::update::parse_version;

const PATH: &str = "/repos/o/r/releases/latest";
const RUNNING: &str = env!("CARGO_PKG_VERSION");

fn release_json(tag: &str, prerelease: bool) -> String {
    format!(
        r#"{{"tag_name": "{tag}", "html_url": "https://example.test/releases/{tag}", "prerelease": {prerelease}, "draft": false}}"#
    )
}

fn http_response(status_line: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Serves `answer` to one connection; the handle yields the request text and
/// the non-blocking listener.
fn serve(answer: String) -> (String, JoinHandle<(String, TcpListener)>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test port");
    let url = format!(
        "http://127.0.0.1:{}{PATH}",
        listener.local_addr().expect("read test port").port()
    );
    listener
        .set_nonblocking(true)
        .expect("switch the listener to non-blocking");
    let handle = thread::spawn(move || {
        // A run that never connects fails the test instead of hanging it.
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(err) if err.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(err) => panic!("no request reached the fake server: {err}"),
            }
        };
        stream
            .set_nonblocking(false)
            .expect("switch the connection to blocking");
        let mut request = Vec::new();
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let mut chunk = [0u8; 1024];
            let read = stream.read(&mut chunk).expect("read request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        // The client may close early; the write result does not matter.
        drop(stream.write_all(answer.as_bytes()));
        (String::from_utf8_lossy(&request).into_owned(), listener)
    });
    (url, handle)
}

fn run(test: &str, url: &str, json: bool) -> Output {
    let mut cmd = common::cli_bin(test);
    cmd.arg("check-update").env("FOCUSFORGE_RELEASE_URL", url);
    if json {
        cmd.arg("--json");
    }
    cmd.output().expect("spawn focusforge-cli")
}

fn json_of(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).expect("stdout is one JSON document")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn newer_release_prints_version_and_link_and_exits_3() {
    let running = parse_version(RUNNING).expect("running version parses");
    for tag in [
        format!("v{}.0.0", running.major + 1),
        format!("{}.{}.0", running.major, running.minor + 1),
    ] {
        let version = tag.trim_start_matches('v');
        let link = format!("https://example.test/releases/{tag}");
        for json in [false, true] {
            let (url, server) = serve(http_response("200 OK", &release_json(&tag, false)));
            let output = run("check_update_newer", &url, json);
            server.join().expect("server thread");
            assert_eq!(output.status.code(), Some(3), "{tag} json={json}");
            if json {
                assert_eq!(
                    json_of(&output),
                    serde_json::json!({
                        "outcome": "newer",
                        "running": RUNNING,
                        "version": version,
                        "url": link,
                    })
                );
            } else {
                assert_eq!(
                    stdout(&output).trim_end(),
                    format!("FocusForge {version} is available (running {RUNNING}): {link}")
                );
            }
        }
    }
}

#[test]
fn up_to_date_for_equal_or_older_release_exits_0() {
    let mut tags = vec!["v0.0.1".to_string()];
    if !RUNNING.contains('-') {
        tags.push(RUNNING.to_string());
    }
    for tag in tags {
        for json in [false, true] {
            let (url, server) = serve(http_response("200 OK", &release_json(&tag, false)));
            let output = run("check_update_up_to_date", &url, json);
            server.join().expect("server thread");
            assert_eq!(output.status.code(), Some(0), "{tag} json={json}");
            if json {
                assert_eq!(
                    json_of(&output),
                    serde_json::json!({"outcome": "up-to-date", "running": RUNNING})
                );
            } else {
                assert_eq!(
                    stdout(&output).trim_end(),
                    format!("FocusForge {RUNNING} is up to date")
                );
            }
            assert!(!stdout(&output).contains("is available"));
        }
    }
}

#[test]
fn failure_classes_exit_1_with_their_reason() {
    let closed = {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test port");
        format!(
            "http://127.0.0.1:{}{PATH}",
            listener.local_addr().expect("read test port").port()
        )
    };
    // Canned answer (None: the closed port) and the lowercase reason fragments.
    let cases = [
        (None, vec!["could not reach the release server", "refused"]),
        (
            Some(http_response("404 Not Found", "{}")),
            vec!["unexpected http status 404"],
        ),
        (
            Some(http_response("200 OK", "<html>not json</html>")),
            vec!["the release answer could not be read"],
        ),
        (
            Some(http_response("200 OK", &release_json("v99.0.0", true))),
            vec!["no final release is published"],
        ),
    ];
    for (answer, fragments) in cases {
        for json in [false, true] {
            let (url, server) = match &answer {
                Some(answer) => {
                    let (url, handle) = serve(answer.clone());
                    (url, Some(handle))
                }
                None => (closed.clone(), None),
            };
            let output = run("check_update_failure", &url, json);
            if let Some(server) = server {
                server.join().expect("server thread");
            }
            assert_eq!(output.status.code(), Some(1), "{fragments:?} json={json}");
            let err = stderr(&output).to_lowercase();
            assert!(err.contains("error: update check failed: "), "{err}");
            let reason = if json {
                let doc = json_of(&output);
                assert_eq!(doc["outcome"], "failed");
                assert_eq!(doc["running"], RUNNING);
                doc["reason"]
                    .as_str()
                    .expect("reason is a string")
                    .to_lowercase()
            } else {
                assert!(stdout(&output).is_empty(), "{}", stdout(&output));
                err.clone()
            };
            for fragment in &fragments {
                assert!(err.contains(fragment), "{fragment} missing from {err}");
                assert!(
                    reason.contains(fragment),
                    "{fragment} missing from {reason}"
                );
            }
        }
    }
}

#[test]
fn one_request_per_run_to_the_configured_path() {
    let (url, server) = serve(http_response("200 OK", &release_json("v0.0.1", false)));
    let output = run("check_update_one_request", &url, false);
    let (request, listener) = server.join().expect("server thread");
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        request.lines().next(),
        Some(format!("GET {PATH} HTTP/1.1").as_str())
    );
    assert_eq!(
        listener.accept().expect_err("no second connection").kind(),
        ErrorKind::WouldBlock
    );
}
