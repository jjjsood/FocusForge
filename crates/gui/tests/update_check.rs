// SPDX-License-Identifier: GPL-3.0-or-later

//! The start-up update check: it runs once, off the caller's thread, only
//! with the switch on; a newer release raises one notice whose link opens the
//! download page, and every other outcome stays invisible.

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use focusforge_core::update::{CheckFailure, ReleaseSource, UpdateCheck};
use focusforge_gui::notifications::{Kind, Source};
use focusforge_gui::update_settings::{PreviewRelease, UpdateSettings};
use focusforge_gui::{AppController, ShellEvent};

const PAGE: &str = "https://github.com/jjjsood/FocusForge/releases/tag/v999.0.0";

fn release(tag: &str, prerelease: bool) -> String {
    format!(
        r#"{{"tag_name":"{tag}","html_url":"https://github.com/jjjsood/FocusForge/releases/tag/{tag}","prerelease":{prerelease},"draft":false}}"#
    )
}

/// Answers every request with a fixed result.
struct Canned(Result<String, String>);

impl ReleaseSource for Canned {
    fn fetch(&self) -> Result<String, String> {
        self.0.clone()
    }
}

/// Counts requests and answers with a release the running version already has
/// or passes.
struct Counting(Arc<AtomicUsize>);

impl ReleaseSource for Counting {
    fn fetch(&self) -> Result<String, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(release("v0.0.1", false))
    }
}

/// Reports the thread it runs on, then waits until the test lets it answer.
struct Blocking {
    calls: Arc<AtomicUsize>,
    entered: mpsc::Sender<ThreadId>,
    release: Mutex<mpsc::Receiver<()>>,
}

impl ReleaseSource for Blocking {
    fn fetch(&self) -> Result<String, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.send(std::thread::current().id()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
        Ok(release("v999.0.0", false))
    }
}

fn on() -> UpdateSettings {
    UpdateSettings::default()
}

/// Runs one check on its background thread and returns the single outcome the
/// sink received.
fn settled(
    controller: &mut AppController,
    settings: &UpdateSettings,
    source: impl ReleaseSource + Send + 'static,
) -> UpdateCheck {
    let events: Arc<Mutex<Vec<ShellEvent>>> = Arc::default();
    let sink = events.clone();
    controller.set_sink(Arc::new(move |event| sink.lock().unwrap().push(event)));
    controller
        .start_update_check(settings, Box::new(source))
        .expect("the switch is on, so a thread starts")
        .join()
        .expect("the check thread does not panic");
    let mut events = events.lock().unwrap();
    assert_eq!(events.len(), 1, "exactly one event: {events:?}");
    match events.pop() {
        Some(ShellEvent::UpdateChecked(outcome)) => outcome,
        other => panic!("expected UpdateChecked, got {other:?}"),
    }
}

fn assert_silent(controller: &AppController) {
    assert!(controller.notifications().is_empty(), "no record");
    assert!(controller.visible_toast().is_none(), "no toast");
    assert_eq!(controller.unread_notifications(), 0, "no unread count");
}

#[test]
fn start_check_runs_once_in_background() {
    let mut controller = AppController::new();
    let events: Arc<Mutex<Vec<ShellEvent>>> = Arc::default();
    let sink = events.clone();
    controller.set_sink(Arc::new(move |event| sink.lock().unwrap().push(event)));

    let calls = Arc::new(AtomicUsize::new(0));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let handle = controller
        .start_update_check(
            &on(),
            Box::new(Blocking {
                calls: calls.clone(),
                entered: entered_tx,
                release: Mutex::new(release_rx),
            }),
        )
        .expect("the switch is on, so a thread starts");

    let source_thread = entered_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the source is asked");
    assert_ne!(
        source_thread,
        std::thread::current().id(),
        "the request must not run on the caller's thread"
    );
    assert!(
        !handle.is_finished() && events.lock().unwrap().is_empty(),
        "start_update_check returned while the source is still blocked"
    );

    release_tx.send(()).unwrap();
    handle.join().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1, "asked exactly once");
    let events = events.lock().unwrap();
    assert!(
        matches!(
            events.as_slice(),
            [ShellEvent::UpdateChecked(UpdateCheck::Newer { .. })]
        ),
        "one UpdateChecked with the newer outcome: {events:?}"
    );
}

#[test]
fn switch_off_makes_no_request() {
    let controller = AppController::new();
    let calls = Arc::new(AtomicUsize::new(0));
    for skipped_version in [None, Some("999.0.0".to_owned())] {
        let settings = UpdateSettings {
            check_on_start: false,
            skipped_version,
        };
        let handle = controller.start_update_check(&settings, Box::new(Counting(calls.clone())));
        assert!(handle.is_none(), "{settings:?} must start no thread");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the source is never asked");
}

#[test]
fn newer_version_raises_one_notice() {
    let mut controller = AppController::new();
    controller.set_window_focused(1, true);
    let outcome = settled(
        &mut controller,
        &on(),
        Canned(Ok(release("v999.0.0", false))),
    );
    assert!(
        matches!(&outcome, UpdateCheck::Newer { version, url } if version.to_string() == "999.0.0" && url == PAGE),
        "{outcome:?}"
    );
    controller.finish_update_check(outcome);

    let records = controller.notifications();
    assert_eq!(records.len(), 1, "exactly one record");
    let record = &records[0];
    assert_eq!((record.kind, record.source), (Kind::Info, Source::Startup));
    assert!(record.title.contains("999.0.0"), "{}", record.title);
    assert_eq!(record.url.as_deref(), Some(PAGE));
    assert_eq!(
        controller.visible_toast().map(|n| n.id),
        Some(record.id),
        "the record is the toast on screen"
    );

    for skipped in ["999.0.0", "v999.0.0"] {
        let mut controller = AppController::new();
        let settings = UpdateSettings {
            check_on_start: true,
            skipped_version: Some(skipped.to_owned()),
        };
        let outcome = settled(
            &mut controller,
            &settings,
            Canned(Ok(release("v999.0.0", false))),
        );
        assert_eq!(outcome, UpdateCheck::UpToDate, "skip {skipped:?}");
        controller.finish_update_check(outcome);
        assert_silent(&controller);
    }
}

#[test]
fn failure_is_silent() {
    let source_failure = |fragment: &'static str| move |outcome: &UpdateCheck| matches!(outcome, UpdateCheck::Failed(CheckFailure::Source(m)) if m.contains(fragment));
    type Case = (
        &'static str,
        Result<String, String>,
        Box<dyn Fn(&UpdateCheck) -> bool>,
    );
    let cases: Vec<Case> = vec![
        (
            "connection refused",
            Err("connection refused".into()),
            Box::new(source_failure("refused")),
        ),
        (
            "timed out",
            Err("timed out".into()),
            Box::new(source_failure("timed out")),
        ),
        (
            "http 404",
            Err("unexpected HTTP status 404".into()),
            Box::new(source_failure("404")),
        ),
        (
            "non-JSON body",
            Ok("<html>not json</html>".into()),
            Box::new(|o| matches!(o, UpdateCheck::Failed(CheckFailure::Unreadable(_)))),
        ),
        (
            "prerelease",
            Ok(release("v999.0.0-rc.1", true)),
            Box::new(|o| matches!(o, UpdateCheck::Failed(CheckFailure::NoFinalRelease))),
        ),
        (
            "equal to the running version",
            Ok(release(&format!("v{}", env!("CARGO_PKG_VERSION")), false)),
            Box::new(|o| matches!(o, UpdateCheck::UpToDate)),
        ),
    ];
    for (name, answer, is_expected) in cases {
        let mut controller = AppController::new();
        let outcome = settled(&mut controller, &on(), Canned(answer));
        assert!(
            is_expected(&outcome),
            "{name}: unexpected outcome {outcome:?}"
        );
        controller.finish_update_check(outcome);
        assert_silent(&controller);
    }
}

#[test]
fn notice_link_opens_download_page() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("focusforge");
    fs::create_dir_all(data.join("crashes")).unwrap();
    fs::write(
        data.join("crashes/20260918T010203Z-focusforge-gui-panic.json"),
        "{\"format\":1,\"payload\":\"boom\"}",
    )
    .unwrap();

    let mut controller = AppController::new();
    controller.set_diagnostics_dirs(Some(data), None);
    let opened: Arc<Mutex<Vec<String>>> = Arc::default();
    let recorder = opened.clone();
    controller.set_url_opener(Arc::new(move |url| {
        recorder.lock().unwrap().push(url.to_owned())
    }));

    let outcome = settled(
        &mut controller,
        &on(),
        Canned(Ok(release("v999.0.0", false))),
    );
    controller.finish_update_check(outcome);
    controller.scan_crash_notice();
    let update = controller
        .visible_toast()
        .expect("the update toast is up")
        .id;
    let crash = controller
        .notifications()
        .iter()
        .find(|n| n.kind == Kind::Crash)
        .expect("the crash record exists")
        .id;
    assert_ne!(update, crash);

    controller.activate_toast(update);
    assert_eq!(*opened.lock().unwrap(), [PAGE], "its url, once");
    assert!(!controller.notification_center_open());
    assert_ne!(
        controller.visible_toast().map(|n| n.id),
        Some(update),
        "the update toast is closed"
    );

    controller.activate_toast(crash);
    assert_eq!(opened.lock().unwrap().len(), 1, "no second open");
    assert!(controller.notification_center_open());
    assert_eq!(controller.notification_selection(), (Some(crash), true));
}

#[test]
fn preview_flag_publishes_the_notice_unless_switched_off() {
    let mut controller = AppController::new();
    let outcome = settled(&mut controller, &on(), PreviewRelease::new("999.0.0"));
    controller.finish_update_check(outcome);
    let record = controller
        .visible_toast()
        .expect("the preview raises the notice");
    assert!(record.title.contains("999.0.0"), "{}", record.title);
    assert_eq!(record.url.as_deref(), Some(PAGE));
    assert_eq!(controller.notifications().len(), 1);

    let off = UpdateSettings {
        check_on_start: false,
        skipped_version: None,
    };
    let quiet = AppController::new();
    assert!(
        quiet
            .start_update_check(&off, Box::new(PreviewRelease::new("999.0.0")))
            .is_none(),
        "the switch off starts nothing"
    );
    assert_silent(&quiet);
}
