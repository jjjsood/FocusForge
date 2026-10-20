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

fn timing_value_of(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_str()
            .and_then(|value| value.trim_matches('"').parse().ok())
    })
}

fn timing_value(event: &Value) -> Option<u64> {
    timing_value_of(&event["args"]["value"])
}

fn timing_counter_total(events: &[Value], counter: &str) -> u64 {
    events
        .iter()
        .filter(|event| event["ph"] == "B" && event["name"] == "timing_count")
        .filter(|event| {
            event["args"]["counter"]
                .as_str()
                .is_some_and(|name| name.trim_matches('"') == counter)
        })
        .filter_map(timing_value)
        .sum()
}

fn summary_u64(summary: &Value, field: &str) -> u64 {
    summary["args"][field]
        .as_u64()
        .or_else(|| {
            summary["args"][field]
                .as_str()
                .and_then(|value| value.trim_matches('"').parse().ok())
        })
        .unwrap_or_else(|| panic!("timing summary field {field} must be a finite u64: {summary:?}"))
}

fn timing_scope_end(events: &[Value], kind: &str) -> f64 {
    let begin = events
        .iter()
        .find(|event| {
            event["ph"] == "B"
                && event["name"] == "timing"
                && event["args"]["kind"]
                    .as_str()
                    .is_some_and(|value| value.trim_matches('"') == kind)
        })
        .expect("the timing scope begins");
    let tid = begin["tid"]
        .as_u64()
        .expect("the timing scope names a thread");
    let mut stack = Vec::new();
    for event in events
        .iter()
        .filter(|event| event["tid"].as_u64() == Some(tid))
    {
        if let Some("B") = event["ph"].as_str() {
            stack.push(
                event["args"]["kind"]
                    .as_str()
                    .map(|value| value.trim_matches('"'))
                    .unwrap_or_else(|| event["name"].as_str().unwrap_or_default()),
            );
        } else if let Some("E") = event["ph"].as_str()
            && stack.pop() == Some(kind)
        {
            return event["ts"].as_f64().expect("every end carries a timestamp");
        }
    }
    panic!("the {kind} timing scope closes")
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
        "--algorithm",
        "laplacian",
        "--align-algorithm",
        "pyramidal-ecc",
        // The anchor policy's parallel solve is the one that records the
        // prepare boundary; the default chain policy solves serially.
        "--align-set",
        "reference-policy=anchor",
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

    let timing_kinds: Vec<&str> = begins(&events, "timing")
        .iter()
        .filter_map(|span| span["args"]["kind"].as_str())
        .map(|kind| kind.trim_matches('"'))
        .collect();
    for expected in [
        "align_prepare",
        "pyramid_prepare",
        "align_solve",
        "ecc_solve",
        "tiff_read",
        "warp_read",
        "fusion_tile",
        "fusion_consume",
    ] {
        assert!(
            timing_kinds.contains(&expected),
            "the trace must contain the {expected} boundary, got {timing_kinds:?}"
        );
    }
    let prepare_end = timing_scope_end(&events, "align_prepare");
    let first_solve = begins(&events, "timing")
        .into_iter()
        .filter(|span| {
            span["args"]["kind"]
                .as_str()
                .is_some_and(|kind| kind.trim_matches('"') == "align_solve")
        })
        .filter_map(|span| span["ts"].as_f64())
        .min_by(f64::total_cmp)
        .expect("an alignment solve begins");
    assert!(
        prepare_end <= first_solve,
        "alignment preparation must close before solving starts: {prepare_end} > {first_solve}"
    );

    for counter in [
        "converted_bytes",
        "warp_bytes",
        "allocation_bytes",
        "allocation_operations",
        "copy_bytes",
        "copy_operations",
    ] {
        let counts: Vec<&Value> = events
            .iter()
            .filter(|event| event["ph"] == "B" && event["name"] == "timing_count")
            .collect();
        assert!(
            counts.iter().any(|event| {
                event["args"]["counter"]
                    .as_str()
                    .is_some_and(|name| name.trim_matches('"') == counter)
                    && timing_value(event).is_some_and(|value| value > 0)
            }),
            "the trace must report a nonzero actual {counter} count, got {counts:?}"
        );
    }
    for counter in ["worker_limit", "peak_workers"] {
        assert!(
            events.iter().any(|event| {
                event["ph"] == "B"
                    && event["name"] == "timing_maximum"
                    && event["args"]["counter"]
                        .as_str()
                        .is_some_and(|name| name.trim_matches('"') == counter)
                    && timing_value(event).is_some_and(|value| value > 0)
            }),
            "the trace must report a nonzero {counter}"
        );
    }

    let summary = begins(&events, "timing_summary");
    assert_eq!(
        summary.len(),
        1,
        "one fixed accounting packet closes the trace"
    );
    let budget = summary_u64(summary[0], "admission_budget");
    assert_eq!(budget, 262_144, "the detailed admission budget is fixed");
    assert!(
        summary_u64(summary[0], "admitted_spans") <= budget,
        "the detailed span count remains bounded: {summary:?}"
    );
    assert_eq!(
        summary_u64(summary[0], "gui_admission_budget"),
        262_144,
        "the graphical front end's admission budget is fixed"
    );
    for field in ["gui_admitted_spans", "gui_exhausted_admissions"] {
        assert_eq!(
            summary_u64(summary[0], field),
            0,
            "a command-line run opens no graphical span: {field}"
        );
    }
    for field in [
        "exhausted_admissions",
        "converted_bytes",
        "warp_bytes",
        "allocation_bytes",
        "allocation_operations",
        "copy_bytes",
        "copy_operations",
        "worker_limit",
        "peak_workers",
        "metadata_reads",
        "row_updates",
        "thumbnail_bytes",
        "job_polls",
        "cache_hits",
        "cache_misses",
        "decoded_bytes",
        "sample_bytes",
        "packed_bytes",
        "upload_bytes",
        "pipeline_creations",
        "cpu_submissions",
        "buffer_creations",
        "readback_bytes",
        "prepare_equalize_ns",
    ] {
        let _ = summary_u64(summary[0], field);
    }
    for field in [
        "gpu_tile_read_ns",
        "gpu_tile_warp_ns",
        "gpu_tile_upload_ns",
        "gpu_tile_pyramid_ns",
        "gpu_tile_select_ns",
        "gpu_tile_reconstruct_ns",
        "gpu_tile_readback_ns",
        "gpu_tile_wait_ns",
        "gpu_tile_other_ns",
    ] {
        assert_eq!(
            summary_u64(summary[0], field),
            0,
            "a CPU run charges no GPU tile time to {field}"
        );
    }
    assert!(
        summary_u64(summary[0], "prepare_equalize_ns") > 0,
        "the default brightness equalization is charged to prepare_equalize_ns: {summary:?}"
    );
    for capability in [
        "gpu_execution_available",
        "presentation_available",
        "saturated",
    ] {
        assert_eq!(
            summary[0]["args"][capability]
                .as_str()
                .map(|value| value.trim_matches('"')),
            Some("false"),
            "the packet records the fixed {capability} state: {summary:?}"
        );
    }

    let varied_trace_path = dir.join("trace-varied.json");
    let varied_out = dir.join("out-varied");
    std::fs::create_dir_all(&varied_out).expect("failed to create varied output directory");
    let varied_out = varied_out.to_string_lossy().into_owned();
    let varied_args = common::push_no_gpu(vec![
        "stack",
        &fixture,
        "-o",
        &varied_out,
        "--algorithm",
        "laplacian",
        "--align-algorithm",
        "pyramidal-ecc",
        "--no-cache",
        "--quiet",
        "--tile-size",
        "512",
    ]);
    let varied = common::cli_bin("trace_chrome_varied")
        .args(&varied_args)
        .env("FOCUSFORGE_TRACE_FILE", &varied_trace_path)
        .output()
        .expect("failed to run varied focusforge-cli");
    assert!(
        varied.status.success(),
        "the varied run must succeed: {}",
        String::from_utf8_lossy(&varied.stderr)
    );
    let varied_events: Vec<Value> = serde_json::from_slice(
        &std::fs::read(&varied_trace_path).expect("the varied trace exists"),
    )
    .expect("the varied trace is one closed JSON array");
    let converted = timing_counter_total(&events, "converted_bytes");
    let varied_converted = timing_counter_total(&varied_events, "converted_bytes");
    assert!(
        converted > 0 && varied_converted > 0 && converted != varied_converted,
        "converted payload totals must follow actual tile geometry: {converted} vs {varied_converted}"
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

/// A traced `align --json` records, per frame, one iteration count and status
/// per pyramid level, and the counts add up to the frame's reported total.
#[test]
fn level_iterations() {
    const DEPTH: usize = 2;
    let dir = common::tmp_dir("trace_levels");
    let trace_path = dir.join("trace.json");
    let fixture = common::fixtures_dir().join("mantis-64");
    let fixture = fixture.to_string_lossy().into_owned();

    let args = common::push_no_gpu(vec![
        "align",
        &fixture,
        "--algorithm",
        "pyramidal-ecc",
        "--set",
        "pyramid-depth=2",
        // Frame spans wrap the level records under the anchor policy's
        // parallel solve; the default chain policy records none.
        "--set",
        "reference-policy=anchor",
        "--quiet",
        "--json",
    ]);
    let output = common::cli_bin("trace_levels")
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
    let report: Value =
        serde_json::from_slice(&output.stdout).expect("align --json prints one document");
    let events: Vec<Value> = serde_json::from_slice(
        &std::fs::read(&trace_path).unwrap_or_else(|e| panic!("no trace: {e}")),
    )
    .expect("the trace is one closed JSON array");

    // Each level record sits inside its frame's span on one thread.
    let mut current: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    let mut per_frame: std::collections::BTreeMap<u64, Vec<u64>> = Default::default();
    for event in &events {
        let tid = event["tid"].as_u64().expect("every entry names a tid");
        match (event["ph"].as_str(), event["name"].as_str()) {
            (Some("B"), Some("frame")) => {
                let index = timing_value_of(&event["args"]["index"]).expect("frame index");
                current.insert(tid, index);
            }
            (Some("E"), Some("frame")) => {
                current.remove(&tid);
            }
            (Some("B"), Some("timing_level")) => {
                let frame = current
                    .get(&tid)
                    .copied()
                    .expect("a level record sits inside a frame span");
                let iterations =
                    timing_value_of(&event["args"]["iterations"]).expect("level iterations");
                per_frame.entry(frame).or_default().push(iterations);
                let status = event["args"]["status"]
                    .as_str()
                    .map(|s| s.trim_matches('"'))
                    .expect("a level record names its status");
                assert!(
                    matches!(status, "converged" | "hit-cap" | "stalled")
                        || status.starts_with("diverged-"),
                    "frame {frame}: unknown level status {status:?}"
                );
            }
            _ => {}
        }
    }

    let alignments = report["alignments"].as_array().expect("alignments");
    assert!(
        !per_frame.is_empty(),
        "at least one frame records its levels"
    );
    for entry in alignments {
        let frame = entry["frame"].as_u64().expect("frame index");
        let reported = entry["iterations"].as_u64().expect("iterations");
        let levels = per_frame.get(&frame).map(Vec::as_slice).unwrap_or(&[]);
        assert!(
            levels.len() <= DEPTH + 1,
            "frame {frame}: at most {} level records, got {levels:?}",
            DEPTH + 1
        );
        assert_eq!(
            levels.iter().sum::<u64>(),
            reported,
            "frame {frame}: level iterations {levels:?} must sum to the reported {reported}"
        );
    }
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
