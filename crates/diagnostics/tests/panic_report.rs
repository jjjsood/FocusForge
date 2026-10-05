// SPDX-License-Identifier: GPL-3.0-or-later

//! A panic on a named pool thread leaves a crash report naming that thread.
//! `init` installs the process-wide subscriber and the panic hook exactly
//! once per process, so this file carries a single test.

use std::panic::AssertUnwindSafe;

#[test]
#[allow(unsafe_code)]
fn a_panic_on_a_named_pool_thread_is_reported_with_that_thread_name() {
    // SAFETY: this file's own doc comment guarantees `init` runs at most
    // once per process, so no other thread reads or writes the environment
    // while this runs. `current_exe()` here is this test binary itself,
    // which has no `main` checking this marker, so leaving native-crash
    // capture on would make `init` re-spawn -- and re-run -- this test.
    unsafe {
        std::env::set_var("FF_CRASH_MONITOR", "off");
    }
    let dir = tempfile::tempdir().unwrap();
    focusforge_diagnostics::init(&focusforge_diagnostics::Options {
        data_dir: Some(dir.path().to_path_buf()),
        verbosity: 0,
    });

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .thread_name(|i| format!("focusforge-tile-{i}"))
        .build()
        .unwrap();
    let caught = std::panic::catch_unwind(AssertUnwindSafe(|| {
        pool.install(|| panic!("tile worker gave up"));
    }));
    assert!(caught.is_err(), "the panic must reach the caller");

    let crashes = dir.path().join("crashes");
    let reports: Vec<std::path::PathBuf> = std::fs::read_dir(&crashes)
        .unwrap_or_else(|e| panic!("{} must exist: {e}", crashes.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(reports.len(), 1, "exactly one report, got {reports:?}");

    let name = reports[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        !name.contains(':'),
        "the file name must carry no colon: {name}"
    );
    assert!(name.ends_with(".json"), "{name}");

    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&reports[0]).unwrap())
            .expect("the report is one JSON object");
    assert!(report.get("format").is_none());
    assert_eq!(report["thread"], "focusforge-tile-0");
    assert_eq!(report["payload"], "tile worker gave up");
    assert_eq!(report["file"], "crates/diagnostics/tests/panic_report.rs");
    assert!(report["line"].as_u64().is_some());
    assert!(
        !report["backtrace"].as_str().unwrap().is_empty(),
        "the report carries a backtrace"
    );
    assert!(
        report["ring"]
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line.as_str().is_some_and(|l| l.contains("starting"))),
        "the ring dump carries the startup line: {}",
        report["ring"]
    );
}
