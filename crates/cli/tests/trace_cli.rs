// SPDX-License-Identifier: GPL-3.0-or-later

//! The Chrome-trace output of a `trace` build: what `FOCUSFORGE_TRACE_FILE`
//! writes for a real `stack` run, and what an output path that cannot be
//! opened does instead.

#![cfg(feature = "trace")]

mod common;

use serde_json::Value;

/// The `B` entries of the spans named `name`, in the order they were
/// recorded.
fn begins<'a>(events: &'a [Value], name: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["ph"] == "B" && e["name"] == name)
        .collect()
}

/// Every thread id whose `thread_name` metadata entry starts with `prefix`.
fn thread_ids_named(events: &[Value], prefix: &str) -> Vec<u64> {
    events
        .iter()
        .filter(|e| e["ph"] == "M" && e["name"] == "thread_name")
        .filter(|e| {
            e["args"]["name"]
                .as_str()
                .is_some_and(|n| n.starts_with(prefix))
        })
        .filter_map(|e| e["tid"].as_u64())
        .collect()
}

#[test]
fn chrome_trace_nesting() {
    let dir = common::tmp_dir("trace_chrome");
    let trace_path = dir.join("trace.json");
    let fixture = common::fixtures_dir().join("mantis-512");
    let fixture = fixture.to_string_lossy().into_owned();
    let out = dir.join("out");
    std::fs::create_dir_all(&out).expect("failed to create the output directory");
    let out = out.to_string_lossy().into_owned();

    let args = common::push_no_gpu(vec![
        "stack",
        &fixture,
        "-o",
        &out,
        "--algo",
        "laplacian",
        "--align-algo",
        "pyramidal-ecc",
        "--lax",
        "--no-cache",
        "--quiet",
        // Not the default tile, which covers this fixture whole: the
        // smallest the pyramidal aligner's margin admits, four tiles
        // across the pool.
        "--tile-size",
        "448",
    ]);
    let output = common::cli_bin("trace_chrome")
        .args(&args)
        .env("FOCUSFORGE_TRACE_FILE", &trace_path)
        .output()
        .expect("failed to run focusforge-cli");
    assert!(
        output.status.success(),
        "the run must succeed, got {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let body = std::fs::read_to_string(&trace_path)
        .unwrap_or_else(|e| panic!("no trace at {}: {e}", trace_path.display()));
    let events: Vec<Value> =
        serde_json::from_str(&body).expect("the trace is one closed JSON array");

    let stack = begins(&events, "stack");
    assert_eq!(stack.len(), 1, "exactly one stack span, got {stack:?}");
    let stack_tid = stack[0]["tid"].as_u64().expect("every entry names a tid");
    let stack_begin = stack[0]["ts"].as_f64().expect("every entry carries a ts");
    let stack_end = events
        .iter()
        .filter(|e| e["ph"] == "E" && e["name"] == "stack" && e["tid"].as_u64() == Some(stack_tid))
        .filter_map(|e| e["ts"].as_f64())
        .next_back()
        .expect("the stack span closes");

    let stages = begins(&events, "stage");
    for stage in &stages {
        let ts = stage["ts"].as_f64().expect("every entry carries a ts");
        assert!(
            (stack_begin..=stack_end).contains(&ts),
            "a stage span falls inside the stack span, got {ts} outside {stack_begin}..{stack_end}"
        );
    }
    // Field values reach `args` as they are formatted, quotes and all.
    let named: Vec<&str> = stages
        .iter()
        .filter(|s| s["tid"].as_u64() == Some(stack_tid))
        .filter_map(|s| s["args"]["stage"].as_str())
        .map(|s| s.trim_matches('"'))
        .collect();
    let expected = ["setup", "align", "fuse"];
    let mut wanted = expected.iter();
    let mut next = wanted.next();
    for stage in &named {
        if next == Some(stage) {
            next = wanted.next();
        }
    }
    assert!(
        next.is_none(),
        "the stage fields must cover {expected:?} in order, got {named:?}"
    );

    let pool = thread_ids_named(&events, "focusforge-tile-");
    let tiles = begins(&events, "tile");
    let on_pool = tiles
        .iter()
        .filter(|t| t["tid"].as_u64().is_some_and(|tid| pool.contains(&tid)))
        .count();
    assert!(
        on_pool >= 4,
        "at least four tile spans on a named pool thread, got {on_pool} of {} across {pool:?}",
        tiles.len()
    );
}

#[test]
fn unwritable_trace_path() {
    let dir = common::tmp_dir("trace_unwritable");
    let trace_path = dir.join("missing").join("trace.json");

    let output = common::cli_bin("trace_unwritable")
        .args(["algorithms"])
        .env("FOCUSFORGE_TRACE_FILE", &trace_path)
        .output()
        .expect("failed to run focusforge-cli");

    assert!(
        output.status.success(),
        "a refused trace path leaves the run's own exit code alone, got {:?}",
        output.status.code()
    );
    assert!(!trace_path.exists(), "no trace file is left behind");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let warnings: Vec<&str> = stderr
        .lines()
        .filter(|l| l.contains("WARN") && l.contains(&*trace_path.to_string_lossy()))
        .collect();
    assert_eq!(
        warnings.len(),
        1,
        "exactly one warning naming the path, got: {stderr}"
    );
}
