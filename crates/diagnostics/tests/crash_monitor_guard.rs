// SPDX-License-Identifier: GPL-3.0-or-later

//! Initialising diagnostics re-spawns the running executable as its own
//! crash monitor. `focusforge-cli` and `focusforge-gui` catch that re-spawn
//! in the first statement of `main`; a libtest binary has no such `main`, so
//! one that initialises diagnostics with native-crash capture left on runs
//! its whole test list again in every re-spawned generation. This binary
//! scans the workspace's own Rust sources for that shape.

use std::path::{Path, PathBuf};

/// Where `text` builds an `Options` value, which is the argument `init`
/// takes: the first `Options {` that neither continues a longer type name
/// nor declares the struct itself.
fn options_built_at(text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(offset) = text[from..].find("Options {") {
        let at = from + offset;
        let continues_a_name = text[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_');
        let line_start = text[..at].rfind('\n').map_or(0, |n| n + 1);
        let declares_the_struct = text[line_start..at].contains("struct ");
        if !continues_a_name && !declares_the_struct {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// True when `text` initialises diagnostics without either running as a
/// monitor first or switching native-crash capture off beforehand.
fn initialises_without_a_monitor_check(text: &str) -> bool {
    let Some(at) = options_built_at(text) else {
        return false;
    };
    if !text.contains("init(") {
        return false;
    }
    let before = &text[..at];
    let switches_capture_off = before
        .lines()
        .any(|line| line.contains("FF_CRASH_MONITOR") && line.contains("\"off\""));
    !before.contains("run_monitor()") && !switches_capture_off
}

/// Every `*.rs` under `<dir>`, at any depth.
fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Every crate's hand-written Rust -- `src` and `tests` of each crate beside
/// this one -- resolved from this crate's manifest rather than from whatever
/// directory the test runner was started in.
fn workspace_sources() -> Vec<PathBuf> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("a crate directory has a parent");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(crates)
        .unwrap_or_else(|e| panic!("{} must be readable: {e}", crates.display()))
        .flatten()
    {
        for sub in ["src", "tests"] {
            rust_sources(&entry.path().join(sub), &mut out);
        }
    }
    out
}

#[test]
fn no_source_initialises_diagnostics_with_native_crash_capture_on() {
    // This file's own negative sample is text, not a call.
    let own = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(Path::new(file!()).file_name().expect("a file name"));
    let offenders: Vec<PathBuf> = workspace_sources()
        .into_iter()
        .filter(|path| *path != own)
        .filter(|path| {
            let text = std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("{} must be readable: {e}", path.display()));
            initialises_without_a_monitor_check(&text)
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "these files initialise diagnostics with native-crash capture left on: {offenders:?}. \
         A process that does so re-executes itself as its own crash monitor, and a libtest \
         binary re-executed that way runs its whole test list again, once per generation. \
         Set FF_CRASH_MONITOR to \"off\" before the call, or run_monitor() ahead of it."
    );
}

#[test]
fn an_entry_point_that_checks_for_the_monitor_first_is_accepted() {
    let text = r#"
fn main() {
    if let Some(code) = focusforge_diagnostics::run_monitor() {
        std::process::exit(code);
    }
    let _diagnostics = focusforge_diagnostics::init(&focusforge_diagnostics::Options {
        data_dir: None,
        verbosity: 0,
    });
}
"#;
    assert!(!initialises_without_a_monitor_check(text));
}

#[test]
fn a_test_that_switches_capture_off_first_is_accepted() {
    let text = r#"
#[test]
fn writes_a_log_line() {
    unsafe {
        std::env::set_var("FF_CRASH_MONITOR", "off");
    }
    let dir = tempfile::tempdir().unwrap();
    focusforge_diagnostics::init(&focusforge_diagnostics::Options {
        data_dir: Some(dir.path().to_path_buf()),
        verbosity: 0,
    });
}
"#;
    assert!(!initialises_without_a_monitor_check(text));
}

#[test]
fn a_test_that_leaves_capture_on_is_refused() {
    let text = r#"
#[test]
fn writes_a_log_line() {
    let dir = tempfile::tempdir().unwrap();
    focusforge_diagnostics::init(&focusforge_diagnostics::Options {
        data_dir: Some(dir.path().to_path_buf()),
        verbosity: 0,
    });
}
"#;
    assert!(initialises_without_a_monitor_check(text));
}

#[test]
fn a_file_that_initialises_nothing_is_accepted() {
    let text = r#"
fn tile_budget(options: &TileSchedulerOptions) -> u64 {
    options.max_bytes_in_flight
}
"#;
    assert!(!initialises_without_a_monitor_check(text));
}

/// Set on the child of `a_libtest_binary_refuses_the_monitor_argument`, so
/// that a libtest which ever did accept the argument would re-run this list
/// once rather than without end.
const RESPAWNED: &str = "FF_CRASH_MONITOR_ARGUMENT_CHILD";

/// The argument a re-spawned monitor carries.
const MONITOR_ARGUMENT: &str = "--focusforge-crash-monitor";

#[test]
fn the_monitor_spawn_carries_an_argument_no_argument_parser_accepts() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("native.rs");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} must be readable: {e}", path.display()));
    let attach = &text[text
        .find("fn attach(")
        .expect("native.rs spawns the monitor in attach")..];
    assert!(
        attach.contains(".arg("),
        "the spawned monitor must carry an argument a test binary cannot accept"
    );
    assert!(
        text.contains(&format!("\"{MONITOR_ARGUMENT}\"")),
        "the argument it carries is {MONITOR_ARGUMENT}"
    );
}

#[test]
fn a_libtest_binary_refuses_the_monitor_argument() {
    if std::env::var_os(RESPAWNED).is_some() {
        return;
    }
    let executable = std::env::current_exe().expect("this test binary has a path");
    let output = std::process::Command::new(&executable)
        .arg(MONITOR_ARGUMENT)
        .env(RESPAWNED, "1")
        .output()
        .unwrap_or_else(|e| panic!("{} must be runnable: {e}", executable.display()));

    assert!(
        !output.status.success(),
        "a test binary re-executed as a monitor must refuse the argument, got {:?}",
        output.status.code()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("running"),
        "it must refuse before running any test, got: {stdout}"
    );
}
