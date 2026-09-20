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
            controller.set_diagnostics_dirs(Some(data_dir.clone()), None);
            controller.set_include_minidumps(include_minidumps);
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
        controller.set_include_minidumps(include_minidumps);
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
