// SPDX-License-Identifier: GPL-3.0-or-later

//! `AppController` is plain Rust: no Slint backend is initialised anywhere
//! in this file, and none is needed -- the callback-forwarding rule is what
//! these tests pin.

mod common;

use std::fs;
use std::sync::{Arc, Mutex};

use focusforge_gui::{AppController, DockTab, ShellEvent, ShellMode};

/// True when `body` (already trimmed) is exactly one statement of the form
/// `root.<ident>(...)`, with an optional trailing `accept`/`reject` token,
/// since `scroll-event` and `key-pressed`/`key-released` must return an
/// `EventResult` and their handler bodies therefore end in a bare `accept`
/// or `reject` after the forwarding call. Nothing else widens: a handler
/// body with two statements before that trailing token must still fail.
fn is_single_root_call(body: &str) -> bool {
    let body = body.trim();
    let body = body
        .strip_suffix("accept")
        .or_else(|| body.strip_suffix("reject"))
        .map(str::trim_end)
        .unwrap_or(body);
    body.starts_with("root.") && body.ends_with(");") && body.matches(';').count() == 1
}

#[test]
fn is_single_root_call_allows_one_trailing_accept_or_reject() {
    assert!(is_single_root_call("root.canvas_scrolled(a, b); accept"));
    assert!(is_single_root_call("root.canvas_key_pressed(a); reject"));
    assert!(is_single_root_call("root.foo();"));
    assert!(
        !is_single_root_call("root.foo(); root.bar(); accept"),
        "two statements before the trailing token must still fail"
    );
    assert!(
        !is_single_root_call("let x = 1; root.foo(); accept"),
        "a non-forward statement before the trailing token must still fail"
    );
}

/// Every `... => { ... }` handler body in `contents`, as
/// `(1-indexed line, trimmed body)`. Handles nested braces so a body that
/// itself contains a struct literal or a block does not truncate early.
fn handler_bodies(contents: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let bytes = contents.as_bytes();
    let mut search_from = 0;
    while let Some(rel) = contents[search_from..].find("=> {") {
        let arrow_pos = search_from + rel;
        let body_start = arrow_pos + "=> {".len();
        let line = contents[..arrow_pos].matches('\n').count() + 1;

        let mut depth = 1i32;
        let mut i = body_start;
        while i < bytes.len() && depth > 0 {
            match bytes[i] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        let body_end = i - 1;
        out.push((line, contents[body_start..body_end].trim().to_string()));
        search_from = i;
    }
    out
}

#[test]
fn callbacks_are_one_line_forwards() {
    let files = common::find_slint_files(&common::ui_dir());
    assert!(
        !files.is_empty(),
        "no .slint files found under {}",
        common::ui_dir().display()
    );

    let mut violations = Vec::new();
    for path in files {
        let contents = fs::read_to_string(&path).unwrap();
        for (line, body) in handler_bodies(&contents) {
            if !is_single_root_call(&body) {
                violations.push(format!("{}:{line}: {body:?}", path.display()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "handler body is not a single `root.<callback>(...)` forward:\n{}",
        violations.join("\n")
    );
}

#[test]
fn controller_api_is_slint_free() {
    let path = common::src_dir().join("controller.rs");
    let contents = fs::read_to_string(&path).unwrap();
    // Code only -- doc comments are free to *talk about* the rule (as this
    // file's own header does) without tripping the scan that enforces it.
    let code = common::strip_line_comments(&contents);

    assert!(
        !code.contains("use slint"),
        "{}: imports slint",
        path.display()
    );

    // Whole-identifier match: `DockTab::Images` must not trip the scan for
    // `Image` (`slint::Image`).
    let tokens: Vec<&str> = code
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .collect();
    for needle in ["Shared", "Model", "Image", "Brush", "Color", "Weak"] {
        assert!(
            !tokens.contains(&needle),
            "{}: names Slint type {needle:?}",
            path.display()
        );
    }
}

#[test]
fn editor_mode_is_unreachable_in_this_phase() {
    let mut controller = AppController::new();
    assert!(
        !controller.editor_enabled(),
        "editor_enabled() must be false"
    );

    let events: Arc<Mutex<Vec<ShellEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    controller.set_sink(Box::new(move |event| {
        sink_events.lock().unwrap().push(event)
    }));

    assert!(
        !controller.set_mode(ShellMode::Editor),
        "set_mode(Editor) must return false"
    );
    assert!(
        events.lock().unwrap().is_empty(),
        "set_mode(Editor) must emit no ModeChanged"
    );
    assert_eq!(
        controller.mode(),
        ShellMode::Stacking,
        "mode must not have changed"
    );
}

#[test]
fn controller_events_reach_an_installed_sink() {
    let mut controller = AppController::new();
    let events: Arc<Mutex<Vec<ShellEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    controller.set_sink(Box::new(move |event| {
        sink_events.lock().unwrap().push(event)
    }));

    controller.tab_strip_pressed();
    controller.window_minimize();
    controller.window_maximize_toggle();
    controller.select_dock_tab(DockTab::Settings);
    controller.checkpoint_export_requested();
    controller.window_close();

    let got = events.lock().unwrap().clone();
    assert_eq!(
        got,
        vec![
            ShellEvent::WindowDragRequested,
            ShellEvent::WindowMinimizeRequested,
            ShellEvent::WindowMaximizeToggled(true),
            ShellEvent::DockTabSelected(DockTab::Settings),
            ShellEvent::CheckpointExportRequested,
            ShellEvent::WindowCloseRequested,
        ],
        "events must reach the sink in the order they were raised"
    );
}

#[test]
fn maximize_toggle_flips_controller_state_and_carries_it_on_the_event() {
    let mut controller = AppController::new();
    let events: Arc<Mutex<Vec<ShellEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    controller.set_sink(Box::new(move |event| {
        sink_events.lock().unwrap().push(event)
    }));

    assert!(!controller.maximized(), "must start not maximized");

    controller.window_maximize_toggle();
    assert!(controller.maximized(), "first toggle must maximize");
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [ShellEvent::WindowMaximizeToggled(true)]
    );

    controller.window_maximize_toggle();
    assert!(!controller.maximized(), "second toggle must restore");
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [
            ShellEvent::WindowMaximizeToggled(true),
            ShellEvent::WindowMaximizeToggled(false),
        ]
    );
}

#[test]
fn controller_state_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AppController>();
}

// --- Diagnostics: manifest, write, destination, marking -------------------

use focusforge_gui::diagnostics::{DiagnosticsStatus, size_text};
use std::path::Path;

/// A data directory: a live log and a rotated log numbered above it, and a
/// crash directory holding one minidump with no sibling report.
fn seed_diagnostics_dir(root: &Path) -> std::path::PathBuf {
    let data = root.join("focusforge");
    fs::create_dir_all(data.join("logs")).unwrap();
    fs::create_dir_all(data.join("crashes")).unwrap();
    fs::write(
        data.join("logs/focusforge.jsonl"),
        "{\"message\":\"live\"}\n",
    )
    .unwrap();
    fs::write(
        data.join("logs/focusforge.2.jsonl"),
        "{\"message\":\"rotated\"}\n",
    )
    .unwrap();
    fs::write(
        data.join("crashes/20260918T010203Z-focusforge-gui-1.dmp"),
        b"MDMP",
    )
    .unwrap();
    data
}

/// A data directory holding one panic report (no dump) and one native
/// crash (metadata plus dump), for the marking test.
fn seed_two_crashes(root: &Path) -> std::path::PathBuf {
    let data = root.join("focusforge");
    fs::create_dir_all(data.join("crashes")).unwrap();
    fs::write(
        data.join("crashes/20260918T010203Z-focusforge-gui-panic.json"),
        "{\"format\":1,\"payload\":\"boom\"}",
    )
    .unwrap();
    fs::write(
        data.join("crashes/20260918T010204Z-focusforge-gui-native.json"),
        "{\"format\":1,\"minidump\":\"20260918T010204Z-focusforge-gui-native.dmp\"}",
    )
    .unwrap();
    fs::write(
        data.join("crashes/20260918T010204Z-focusforge-gui-native.dmp"),
        b"MDMP",
    )
    .unwrap();
    data
}

/// Runs `controller.start_bundle_write()`, joins it, applies the one
/// `DiagnosticsWriteFinished` event it raises through `sink`, and returns
/// the resulting status -- the same choreography `bridge.rs`'s sink
/// installer performs, minus the `slint::invoke_from_event_loop` hop that
/// needs a running window.
fn run_write(
    controller: &mut AppController,
    events: &Arc<Mutex<Vec<ShellEvent>>>,
) -> DiagnosticsStatus {
    let before = events.lock().unwrap().len();
    controller.start_bundle_write().join().unwrap();
    let got = events.lock().unwrap()[before..].to_vec();
    let [ShellEvent::DiagnosticsWriteFinished(status)] = got.as_slice() else {
        panic!("expected exactly one DiagnosticsWriteFinished event, got {got:?}");
    };
    controller.finish_bundle_write(*status);
    *status
}

fn install_recording_sink(controller: &mut AppController) -> Arc<Mutex<Vec<ShellEvent>>> {
    let events: Arc<Mutex<Vec<ShellEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    controller.set_sink(Box::new(move |event| {
        sink_events.lock().unwrap().push(event)
    }));
    events
}

/// Runs `controller.start_manifest_scan()`, joins it, and applies every
/// `DiagnosticsScanFinished` event the sink recorded since the call started
/// -- there may be more than one when a scan started elsewhere (e.g. by
/// `set_include_minidumps`) is still in flight, but `finish_manifest_scan`'s
/// own generation guard means only the one this call started ever lands in
/// the controller's stored state. Asserts that one was applied.
fn run_scan(controller: &mut AppController, events: &Arc<Mutex<Vec<ShellEvent>>>) {
    let before = events.lock().unwrap().len();
    controller.start_manifest_scan().join().unwrap();
    let got = events.lock().unwrap()[before..].to_vec();
    assert!(
        got.iter()
            .any(|e| matches!(e, ShellEvent::DiagnosticsScanFinished { .. })),
        "expected at least one DiagnosticsScanFinished event, got {got:?}"
    );
    for event in got {
        if let ShellEvent::DiagnosticsScanFinished { generation, scan } = event {
            controller.finish_manifest_scan(generation, scan);
        }
    }
    assert!(
        !controller.diagnostics_view().loading,
        "the scan this call started must match the controller's current generation"
    );
}

#[test]
fn diagnostics_bundle_manifest() {
    let seeded_root = tempfile::tempdir().unwrap();
    let seeded = seed_diagnostics_dir(seeded_root.path());
    let empty_root = tempfile::tempdir().unwrap();
    fs::create_dir_all(empty_root.path().join("focusforge")).unwrap();
    let empty = empty_root.path().join("focusforge");

    for (data_dir, expect_note) in [(&seeded, false), (&empty, true)] {
        for include_minidumps in [false, true] {
            let mut controller = AppController::new();
            let events = install_recording_sink(&mut controller);
            controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
            // Joined so its own scan report has already landed before
            // `run_scan` takes its "since here" snapshot below -- otherwise
            // a straggling report can land in an unrelated later window.
            controller
                .set_include_minidumps(include_minidumps)
                .join()
                .unwrap();
            run_scan(&mut controller, &events);
            let view = controller.diagnostics_view();

            let reference = focusforge_diagnostics::bundle::manifest(
                &focusforge_diagnostics::bundle::Options {
                    data_dir: Some(data_dir.clone()),
                    config_dir: None,
                    include_minidumps,
                },
            );
            assert_eq!(
                view.entries
                    .iter()
                    .map(|e| e.name.clone())
                    .collect::<Vec<_>>(),
                reference
                    .entries
                    .iter()
                    .map(|e| e.name.clone())
                    .collect::<Vec<_>>(),
                "row order/names must equal bundle::manifest's, include_minidumps={include_minidumps}"
            );
            for (row, entry) in view.entries.iter().zip(reference.entries.iter()) {
                assert_eq!(
                    row.not_redacted, !entry.redacted,
                    "{}: not-redacted mark must match the manifest entry",
                    row.name
                );
            }
            assert_eq!(
                view.note.is_some(),
                expect_note,
                "the note row must appear only for the empty data directory"
            );
        }
    }
}

#[test]
fn diagnostics_bundle_manifest_value_column_units() {
    assert_eq!(size_text(0), "0 KB");
    assert_eq!(size_text(1024), "1 KB");
    assert_eq!(size_text(1024 * 1024 - 1), "1024 KB");
    assert_eq!(size_text(1024 * 1024), "1.0 MB");
    assert_eq!(size_text(1024 * 1024 + 1024 * 512), "1.5 MB");
}

#[test]
fn diagnostics_bundle_write() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_diagnostics_dir(root.path());

    for include_minidumps in [false, true] {
        let mut controller = AppController::new();
        let events = install_recording_sink(&mut controller);
        controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
        // Joined for the same reason as `diagnostics_bundle_manifest`: its
        // own scan must land before `run_scan`'s window, or `run_write`'s
        // later one can catch the straggler instead.
        controller
            .set_include_minidumps(include_minidumps)
            .join()
            .unwrap();
        run_scan(&mut controller, &events);
        let view = controller.diagnostics_view();
        let out = root.path().join(format!("bundle-{include_minidumps}.zip"));
        controller.set_destination(out.to_str().unwrap());

        let status = run_write(&mut controller, &events);
        assert_eq!(status, DiagnosticsStatus::Written);
        assert_eq!(controller.diagnostics_status(), DiagnosticsStatus::Written);

        let file = fs::File::open(&out).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        let mut shown: Vec<String> = view.entries.iter().map(|e| e.name.clone()).collect();
        shown.sort();
        assert_eq!(names, shown, "archive entries must equal the shown rows");
        assert_eq!(
            names.iter().any(|n| n.ends_with(".dmp")),
            include_minidumps,
            "a minidump is in the archive iff the choice was made"
        );
    }
}

#[test]
fn diagnostics_bundle_write_repeats_without_an_extra_click() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_diagnostics_dir(root.path());

    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    // A destination outside `default_destination()`'s own
    // `<data_dir>/diagnostics/` tree, so the fresh path finish derives next
    // can never collide with it regardless of the clock's granularity.
    let first_destination = root.path().join("manual-first.zip");
    controller.set_destination(first_destination.to_str().unwrap());

    let status = run_write(&mut controller, &events);
    assert_eq!(status, DiagnosticsStatus::Written);
    run_scan(&mut controller, &events);
    let view = controller.diagnostics_view();
    assert_eq!(
        view.primary_label, "Create bundle",
        "the button must read \"Create bundle\" again, not \"Done\""
    );
    assert!(
        !view.busy,
        "the panel must not require an extra click to re-enable"
    );

    let second_destination = view.destination.clone();
    assert_ne!(
        Path::new(&second_destination),
        first_destination,
        "a finished write must re-derive a fresh destination"
    );
    assert!(
        !Path::new(&second_destination).exists(),
        "the fresh destination must not already exist"
    );

    // The same primary action, clicked again with no destination edit,
    // must write a second, different archive rather than closing anything.
    let status = run_write(&mut controller, &events);
    assert_eq!(status, DiagnosticsStatus::Written);
    assert!(
        first_destination.exists() && Path::new(&second_destination).exists(),
        "both archives must exist: {} and {second_destination}",
        first_destination.display()
    );
}

#[test]
fn diagnostics_bundle_destination() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_diagnostics_dir(root.path());

    // A destination that already exists reports the existing-file state
    // and is written only once the overwrite is taken.
    let existing = root.path().join("existing.zip");
    fs::write(&existing, b"placeholder").unwrap();
    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.set_destination(existing.to_str().unwrap());
    assert_eq!(
        controller.diagnostics_status(),
        DiagnosticsStatus::ExistingFile,
        "a destination that already exists must report ExistingFile before any write"
    );
    let status = run_write(&mut controller, &events);
    assert_eq!(status, DiagnosticsStatus::Written);
    let contents = fs::read(&existing).unwrap();
    assert_ne!(
        contents, b"placeholder",
        "the overwrite must have actually replaced the file"
    );

    // A destination under a path component that is a regular file reports
    // the failure state and leaves neither the archive nor a `.partial`.
    let blocked_parent = root.path().join("blocked");
    fs::write(&blocked_parent, b"not a directory").unwrap();
    let unwritable = blocked_parent.join("bundle.zip");
    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.set_destination(unwritable.to_str().unwrap());
    let status = run_write(&mut controller, &events);
    assert_eq!(status, DiagnosticsStatus::Failed);
    assert!(!unwritable.exists(), "no archive must be left behind");
    assert!(
        !unwritable.with_file_name("bundle.zip.partial").exists(),
        "no .partial must be left behind"
    );
}

#[test]
fn diagnostics_bundle_marks_reported() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_two_crashes(root.path());
    let unrelated_root = tempfile::tempdir().unwrap();
    let unrelated_data_dir = seed_two_crashes(unrelated_root.path());

    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.set_destination(root.path().join("bundle.zip").to_str().unwrap());
    let status = run_write(&mut controller, &events);
    assert_eq!(status, DiagnosticsStatus::Written);

    let crashes = data_dir.join("crashes");
    assert!(
        crashes
            .join("20260918T010203Z-focusforge-gui-panic.reported")
            .exists(),
        "the packaged panic report must be marked reported"
    );
    assert!(
        crashes
            .join("20260918T010204Z-focusforge-gui-native.reported")
            .exists(),
        "the packaged native crash must be marked reported"
    );
    let unrelated_crashes = unrelated_data_dir.join("crashes");
    assert!(
        !unrelated_crashes
            .join("20260918T010203Z-focusforge-gui-panic.reported")
            .exists(),
        "an unrelated data directory's crash must not be marked"
    );

    // With the crash directory made unwritable, the write still reports
    // success -- marking is best-effort.
    let root2 = tempfile::tempdir().unwrap();
    let data_dir2 = seed_two_crashes(root2.path());
    let crashes2 = data_dir2.join("crashes");
    let mut perms = fs::metadata(&crashes2).unwrap().permissions();
    let writable = perms.clone();
    perms.set_readonly(true);
    fs::set_permissions(&crashes2, perms).unwrap();

    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(data_dir2.clone()), None);
    controller.set_destination(root2.path().join("bundle.zip").to_str().unwrap());
    let status = run_write(&mut controller, &events);

    fs::set_permissions(&crashes2, writable).unwrap();
    assert_eq!(
        status,
        DiagnosticsStatus::Written,
        "an unwritable crash directory must not turn the write into a failure"
    );
}

// --- Diagnostics: the background scan --------------------------------

/// Names of `bundle::manifest`'s entries for `data_dir`, sorted the way the
/// manifest itself already sorts the generated four and the collected rest.
fn reference_entry_names(data_dir: &Path, include_minidumps: bool) -> Vec<String> {
    focusforge_diagnostics::bundle::manifest(&focusforge_diagnostics::bundle::Options {
        data_dir: Some(data_dir.to_path_buf()),
        config_dir: None,
        include_minidumps,
    })
    .entries
    .iter()
    .map(|e| e.name.clone())
    .collect()
}

#[test]
fn diagnostics_scan_off_the_main_thread() {
    let seeded_root = tempfile::tempdir().unwrap();
    let seeded = common::seed_large_diagnostics_dir(seeded_root.path(), 20 * 1024 * 1024);
    let empty_root = tempfile::tempdir().unwrap();
    fs::create_dir_all(empty_root.path().join("focusforge")).unwrap();
    let empty = empty_root.path().join("focusforge");

    for data_dir in [&seeded, &empty] {
        let mut controller = AppController::new();
        let start = std::time::Instant::now();
        controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
        let view = controller.diagnostics_view();
        let elapsed = start.elapsed();
        assert!(
            view.entries.is_empty(),
            "the first view must carry no rows before any scan has completed, got {:?}",
            view.entries
        );
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "set_diagnostics_dirs plus diagnostics_view took {elapsed:?} over {}",
            data_dir.display()
        );
    }

    let reference = reference_entry_names(&seeded, false);
    assert!(
        !reference.is_empty(),
        "bundle::manifest itself must yield entries for the seeded directory"
    );
}

#[test]
fn diagnostics_scan_reports_through_the_sink() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_diagnostics_dir(root.path());

    for include_minidumps in [false, true] {
        let mut controller = AppController::new();
        controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
        // Sets the choice and lets its own scan finish unobserved, so the
        // sink installed below records only the scan this test starts.
        controller
            .set_include_minidumps(include_minidumps)
            .join()
            .unwrap();

        let events = install_recording_sink(&mut controller);
        controller.start_manifest_scan().join().unwrap();
        let got = events.lock().unwrap().clone();
        let [ShellEvent::DiagnosticsScanFinished { generation, scan }] = got.as_slice() else {
            panic!("expected exactly one DiagnosticsScanFinished event, got {got:?}");
        };
        controller.finish_manifest_scan(*generation, scan.clone());
        let view = controller.diagnostics_view();

        let reference =
            focusforge_diagnostics::bundle::manifest(&focusforge_diagnostics::bundle::Options {
                data_dir: Some(data_dir.clone()),
                config_dir: None,
                include_minidumps,
            });
        assert_eq!(
            view.entries
                .iter()
                .map(|e| e.name.clone())
                .collect::<Vec<_>>(),
            reference
                .entries
                .iter()
                .map(|e| e.name.clone())
                .collect::<Vec<_>>(),
            "row order/names must equal bundle::manifest's, include_minidumps={include_minidumps}"
        );
        for (row, entry) in view.entries.iter().zip(reference.entries.iter()) {
            assert_eq!(
                row.not_redacted, !entry.redacted,
                "{}: not-redacted mark must match the manifest entry",
                row.name
            );
        }
    }
}

#[test]
fn diagnostics_rescan_on_minidump_choice() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_diagnostics_dir(root.path());

    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    run_scan(&mut controller, &events);
    assert!(
        !controller.diagnostics_view().entries.is_empty(),
        "the seeded directory must show rows once the initial scan lands"
    );

    for include_minidumps in [true, false] {
        let before = events.lock().unwrap().len();
        let handle = controller.set_include_minidumps(include_minidumps);

        let mid_view = controller.diagnostics_view();
        assert!(
            mid_view.loading,
            "the view between the toggle and its report must be loading"
        );
        assert!(
            mid_view.entries.is_empty(),
            "the view between the toggle and its report must carry no rows, not the previous list"
        );

        handle.join().unwrap();
        let got = events.lock().unwrap()[before..].to_vec();
        let [ShellEvent::DiagnosticsScanFinished { generation, scan }] = got.as_slice() else {
            panic!("expected exactly one DiagnosticsScanFinished event, got {got:?}");
        };
        controller.finish_manifest_scan(*generation, scan.clone());

        let view = controller.diagnostics_view();
        let reference = reference_entry_names(&data_dir, include_minidumps);
        assert_eq!(
            view.entries
                .iter()
                .map(|e| e.name.clone())
                .collect::<Vec<_>>(),
            reference,
            "the rescanned rows must equal bundle::manifest for include_minidumps={include_minidumps}"
        );
    }
}

#[test]
fn diagnostics_scan_unreadable_dir() {
    let root = tempfile::tempdir().unwrap();
    let blocked_parent = root.path().join("blocked");
    fs::write(&blocked_parent, b"not a directory").unwrap();
    let unreadable = blocked_parent.join("focusforge");

    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(unreadable), None);
    run_scan(&mut controller, &events);
    let view = controller.diagnostics_view();

    assert!(
        !view.loading,
        "an unreadable data directory must still report a completed scan"
    );
    assert_eq!(
        view.entries
            .iter()
            .map(|e| e.name.clone())
            .collect::<Vec<_>>(),
        vec![
            "adapter.json",
            "configuration.json",
            "system.json",
            "version.json"
        ],
        "an unreadable data directory must still surface the generated entries"
    );
    assert_eq!(
        view.note.as_deref(),
        Some("No logs or crash evidence were found.")
    );
}

#[test]
fn diagnostics_scan_outlives_its_controller() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_diagnostics_dir(root.path());

    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(data_dir), None);
    let handle = controller.start_manifest_scan();
    drop(controller);

    handle
        .join()
        .expect("the scan thread must not panic once its controller is gone");
    let got = events.lock().unwrap().clone();
    assert!(
        got.len() <= 1,
        "the sink must hold at most the one scan event, never a second, got {got:?}"
    );
    if let [event] = got.as_slice() {
        assert!(
            matches!(event, ShellEvent::DiagnosticsScanFinished { .. }),
            "the one recorded event must be the scan's own, got {event:?}"
        );
    }
}

#[test]
fn diagnostics_destination_field_scrolls_to_keep_cursor_visible() {
    let mut controller = AppController::new();
    assert_eq!(
        controller.diagnostics_destination_scroll_offset(),
        0.0,
        "a fresh controller must start unscrolled"
    );

    // The cursor sits past the visible width -- the field must scroll left
    // (a negative offset) just enough to bring it back inside.
    controller.diagnostics_destination_cursor_moved(140.0, 100.0);
    let offset = controller.diagnostics_destination_scroll_offset();
    assert!(
        offset < 0.0,
        "a cursor past the visible width must scroll the field, got {offset}"
    );
    assert!(
        (0.0..=100.0).contains(&(140.0 + offset)),
        "the cursor must land back inside the visible width: cursor 140 + offset {offset}"
    );

    // The cursor moves back near the field's start (Home, or backspacing to
    // a short prefix) -- the field must scroll back to show it rather than
    // leaving it hidden behind the still-scrolled offset.
    controller.diagnostics_destination_cursor_moved(2.0, 100.0);
    let offset = controller.diagnostics_destination_scroll_offset();
    assert!(
        (0.0..=100.0).contains(&(2.0 + offset)),
        "the cursor must land back inside the visible width after scrolling left: cursor 2 + offset {offset}"
    );

    // A cursor that is already inside the visible width leaves the offset
    // untouched.
    let mut steady = AppController::new();
    steady.diagnostics_destination_cursor_moved(50.0, 100.0);
    let before = steady.diagnostics_destination_scroll_offset();
    steady.diagnostics_destination_cursor_moved(55.0, 100.0);
    assert_eq!(
        steady.diagnostics_destination_scroll_offset(),
        before,
        "a cursor still inside the visible width must not move the offset"
    );
}

// --- Crash notice: pending count, dismiss, review, timeout, bundle write --

use focusforge_gui::SettingsSection;

/// [`seed_two_crashes`] plus a third crash that already carries its
/// reported marker -- still two unreported crashes.
fn seed_notice_crashes(root: &Path) -> std::path::PathBuf {
    let data = seed_two_crashes(root);
    let crashes = data.join("crashes");
    fs::write(
        crashes.join("20260918T010205Z-focusforge-gui-old.json"),
        "{\"format\":1,\"payload\":\"already seen\"}",
    )
    .unwrap();
    fs::write(
        crashes.join("20260918T010205Z-focusforge-gui-old.reported"),
        "2026-09-18T01:02:06Z",
    )
    .unwrap();
    data
}

#[test]
fn crash_notice_counts_and_dismisses() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_notice_crashes(root.path());

    let mut controller = AppController::new();
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.scan_crash_notice();
    assert!(controller.crash_notice_visible());
    assert_eq!(controller.crash_notice_count(), 2);
    assert_eq!(controller.crash_notice_text(), "2 crashes found");

    controller.dismiss_crash_notice();
    assert!(!controller.crash_notice_visible());
    assert_eq!(controller.crash_notice_count(), 0);

    let mut after = AppController::new();
    after.set_diagnostics_dirs(Some(data_dir), None);
    after.scan_crash_notice();
    assert!(
        !after.crash_notice_visible(),
        "a fresh controller over the same directory must see nothing pending"
    );
    assert_eq!(after.crash_notice_count(), 0);

    // A second data directory with an empty crash directory counts 0 from
    // the start.
    let empty_root = tempfile::tempdir().unwrap();
    fs::create_dir_all(empty_root.path().join("focusforge/crashes")).unwrap();
    let mut empty = AppController::new();
    empty.set_diagnostics_dirs(Some(empty_root.path().join("focusforge")), None);
    empty.scan_crash_notice();
    assert!(!empty.crash_notice_visible());
    assert_eq!(empty.crash_notice_count(), 0);
}

#[test]
#[cfg(unix)]
fn crash_notice_unreadable_dir() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_notice_crashes(root.path());
    let crashes = data_dir.join("crashes");
    let writable = fs::metadata(&crashes).unwrap().permissions();
    fs::set_permissions(&crashes, fs::Permissions::from_mode(0o000)).unwrap();

    let mut controller = AppController::new();
    controller.set_diagnostics_dirs(Some(data_dir), None);
    controller.scan_crash_notice();

    fs::set_permissions(&crashes, writable).unwrap();

    assert!(
        !controller.crash_notice_visible(),
        "an unreadable crash directory must show no notice"
    );
    assert_eq!(controller.crash_notice_count(), 0);
}

#[test]
fn crash_notice_timeout_reports_nothing() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_notice_crashes(root.path());

    let mut controller = AppController::new();
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.scan_crash_notice();
    assert!(controller.crash_notice_visible());
    let count_before = controller.crash_notice_count();

    controller.crash_notice_timed_out();
    assert!(!controller.crash_notice_visible());
    assert!(
        !data_dir
            .join("crashes/20260918T010203Z-focusforge-gui-panic.reported")
            .exists(),
        "a timeout must write no reported marker"
    );
    assert!(
        !data_dir
            .join("crashes/20260918T010204Z-focusforge-gui-native.reported")
            .exists(),
        "a timeout must write no reported marker"
    );

    let mut after = AppController::new();
    after.set_diagnostics_dirs(Some(data_dir), None);
    after.scan_crash_notice();
    assert!(
        after.crash_notice_visible(),
        "the evidence must still be pending after a timeout"
    );
    assert_eq!(after.crash_notice_count(), count_before);
}

#[test]
fn crash_notice_review_opens_diagnostics() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_notice_crashes(root.path());

    let mut controller = AppController::new();
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.scan_crash_notice();
    assert!(controller.crash_notice_visible());

    let open = controller.review_crashes();
    assert!(open, "review must leave the settings modal open");
    assert_eq!(controller.settings_section(), SettingsSection::Diagnostics);
    assert!(!controller.crash_notice_visible());
    assert!(
        data_dir
            .join("crashes/20260918T010203Z-focusforge-gui-panic.reported")
            .exists(),
        "review must mark the pending evidence reported, same as dismiss"
    );
    assert!(
        data_dir
            .join("crashes/20260918T010204Z-focusforge-gui-native.reported")
            .exists()
    );

    let marker = data_dir.join("crashes/20260918T010203Z-focusforge-gui-panic.reported");
    let marked_at = fs::read_to_string(&marker).unwrap();
    let open_again = controller.review_crashes();
    assert!(open_again);
    assert_eq!(
        fs::read_to_string(&marker).unwrap(),
        marked_at,
        "a second call must mark nothing further"
    );
}

#[test]
fn crash_notice_cleared_by_a_bundle_write() {
    let root = tempfile::tempdir().unwrap();
    let data_dir = seed_two_crashes(root.path());

    let mut controller = AppController::new();
    let events = install_recording_sink(&mut controller);
    controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
    controller.scan_crash_notice();
    assert!(controller.crash_notice_visible());
    assert_eq!(controller.crash_notice_count(), 2);

    controller.set_destination(root.path().join("bundle.zip").to_str().unwrap());
    let status = run_write(&mut controller, &events);
    assert_eq!(status, DiagnosticsStatus::Written);
    assert!(
        !controller.crash_notice_visible(),
        "a bundle written from the window must hide the notice"
    );
    assert_eq!(controller.crash_notice_count(), 0);

    // A failed write leaves it showing.
    let root2 = tempfile::tempdir().unwrap();
    let data_dir2 = seed_two_crashes(root2.path());
    let mut controller2 = AppController::new();
    let events2 = install_recording_sink(&mut controller2);
    controller2.set_diagnostics_dirs(Some(data_dir2.clone()), None);
    controller2.scan_crash_notice();
    assert!(controller2.crash_notice_visible());

    let blocked_parent = data_dir2.parent().unwrap().join("blocked");
    fs::write(&blocked_parent, b"not a directory").unwrap();
    controller2.set_destination(blocked_parent.join("bundle.zip").to_str().unwrap());
    let status2 = run_write(&mut controller2, &events2);
    assert_eq!(status2, DiagnosticsStatus::Failed);
    assert!(
        controller2.crash_notice_visible(),
        "a failed write must leave the notice showing"
    );
    assert_eq!(controller2.crash_notice_count(), 2);
}
