// SPDX-License-Identifier: GPL-3.0-or-later

//! `init` installs the process-wide subscriber exactly once per process, so
//! this file carries a single test exercising both directions the file sink
//! must accept: a `tracing` event and a `log`-crate record.

use std::io::Read;

use serde_json::Value;

fn read_lines(path: &std::path::Path) -> Vec<Value> {
    let mut contents = String::new();
    std::fs::File::open(path)
        .unwrap()
        .read_to_string(&mut contents)
        .unwrap();
    contents
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON: {l}: {e}")))
        .collect()
}

#[test]
#[allow(unsafe_code)]
fn tracing_events_and_log_records_both_reach_the_file_sink() {
    // SAFETY: this file's own doc comment guarantees `init` runs at most
    // once per process, so no other thread reads or writes the environment
    // while this runs. `current_exe()` here is this test binary itself,
    // which has no `main` checking this marker, so leaving native-crash
    // capture on would make `init` re-spawn -- and re-run -- this test.
    unsafe {
        std::env::set_var("FF_CRASH_MONITOR", "off");
    }
    let dir = tempfile::tempdir().unwrap();
    let diagnostics = focusforge_diagnostics::init(&focusforge_diagnostics::Options {
        data_dir: Some(dir.path().to_path_buf()),
        verbosity: 0,
    });
    let log_path = diagnostics.log_path().expect("file sink must have opened");

    tracing_log::log::warn!(target: "some_third_party_crate", "a log-crate warning");
    tracing::warn!(target: "focusforge_core", "a tracing warning");

    let lines = read_lines(log_path);
    assert!(
        lines.len() >= 2,
        "expected at least the startup line plus both emitted records, got {lines:?}"
    );

    for line in &lines {
        let obj = line.as_object().expect("each line must be a JSON object");
        for key in ["timestamp", "level", "target", "message"] {
            assert!(obj.contains_key(key), "line missing {key}: {line}");
        }
    }

    let log_record = lines
        .iter()
        .find(|l| l["target"] == "some_third_party_crate")
        .expect("the log-crate record must reach the file with its own target");
    assert_eq!(log_record["level"], "WARN");
    assert_eq!(log_record["message"], "a log-crate warning");

    let tracing_record = lines
        .iter()
        .find(|l| l["target"] == "focusforge_core" && l["message"] == "a tracing warning")
        .expect("the tracing event must reach the file");
    assert_eq!(tracing_record["level"], "WARN");
}
