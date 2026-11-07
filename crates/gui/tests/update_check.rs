// SPDX-License-Identifier: GPL-3.0-or-later

//! The start-up update check: it runs once, off the caller's thread, only
//! with the switch on; a newer release raises one notice whose link opens the
//! download page, and every other outcome stays invisible. Check now in the
//! About tab runs the same check on request and reports every outcome there
//! only.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use focusforge_core::update::{CheckFailure, ReleaseSource, UpdateCheck};
use focusforge_gui::notifications::{Kind, Source};
use focusforge_gui::update_settings::{self, PreviewRelease, UpdateSettings};
use focusforge_gui::{AppController, ShellEvent, StorageTone};

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
            "older than the running version",
            Ok(release("v0.0.1", false)),
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
    controller.set_diagnostics_dirs(Some(data));
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

/// Waits for the test to open its gate, then answers with a fixed result.
struct Gated {
    gate: Mutex<mpsc::Receiver<()>>,
    answer: Result<String, String>,
}

impl ReleaseSource for Gated {
    fn fetch(&self) -> Result<String, String> {
        self.gate.lock().unwrap().recv().unwrap();
        self.answer.clone()
    }
}

fn gated(answer: Result<String, String>) -> (mpsc::Sender<()>, Gated) {
    let (open, gate) = mpsc::channel();
    (
        open,
        Gated {
            gate: Mutex::new(gate),
            answer,
        },
    )
}

type Events = Arc<Mutex<Vec<ShellEvent>>>;
type Opened = Arc<Mutex<Vec<String>>>;

/// A controller on `config_dir` with a capturing sink and a recording opener.
fn window(config_dir: &Path) -> (AppController, Events, Opened) {
    let mut controller = AppController::new();
    controller.install_recent(Vec::new(), config_dir.to_path_buf());
    controller.install_updates(update_settings::load(config_dir));
    let events: Events = Arc::default();
    let sink = events.clone();
    controller.set_sink(Arc::new(move |event| sink.lock().unwrap().push(event)));
    let opened: Opened = Arc::default();
    let recorder = opened.clone();
    controller.set_url_opener(Arc::new(move |url| {
        recorder.lock().unwrap().push(url.to_owned())
    }));
    (controller, events, opened)
}

/// Installs a Check now source that answers `answer` to every click.
fn answer_with(controller: &mut AppController, answer: Result<String, String>) {
    controller.set_release_source(Arc::new(move || Box::new(Canned(answer.clone()))));
}

/// Hands the controller the one event the sink holds, as the event loop does.
fn deliver_one(controller: &mut AppController, events: &Events) -> ShellEvent {
    let mut events = events.lock().unwrap();
    assert_eq!(events.len(), 1, "exactly one event: {events:?}");
    let event = events.pop().unwrap();
    match &event {
        ShellEvent::UpdateChecked(outcome) => controller.finish_update_check(outcome.clone()),
        ShellEvent::UpdateCheckedNow(outcome) => controller.finish_check_now(outcome.clone()),
        other => panic!("not an update event: {other:?}"),
    }
    event
}

/// One click on Check now, run to its result.
fn click_check_now(controller: &mut AppController, events: &Events) -> UpdateCheck {
    controller
        .check_for_updates_now()
        .expect("a source is installed and no check runs")
        .join()
        .expect("the check thread does not panic");
    match deliver_one(controller, events) {
        ShellEvent::UpdateCheckedNow(outcome) => outcome,
        other => panic!("expected UpdateCheckedNow, got {other:?}"),
    }
}

fn settings_file(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join("settings.toml")).ok()
}

#[test]
fn update_switch_persists_and_keeps_other_tables() {
    let sibling = "# kept by hand\n[appearance]\nreduce_motion = true # note\n";
    let stored: [(&str, bool); 4] = [
        ("[updates]\ncheck_on_start = true\n", true),
        ("[updates]\ncheck_on_start = false\n", false),
        ("[updates]\nskipped_version = \"1.2.3\"\n", true),
        ("[updates]\ncheck_on_start = \"yes\"\n", true),
    ];
    for (updates, shown) in stored {
        for skipped in [None, Some("1.2.3")] {
            let body = match skipped {
                Some(version) if !updates.contains("skipped_version") => {
                    format!("{updates}skipped_version = \"{version}\"\n")
                }
                _ => updates.to_owned(),
            };
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("settings.toml"), format!("{sibling}{body}")).unwrap();
            let (mut controller, ..) = window(dir.path());
            assert_eq!(
                controller.update_controls_view().check_on_start,
                shown,
                "stored {body:?}"
            );
            let before = controller.update_settings().skipped_version.clone();

            controller.set_update_check_on_start(!shown);
            let view = controller.update_controls_view();
            assert_eq!(view.check_on_start, !shown, "stored {body:?}");
            assert_eq!(view.save_error, "");
            let saved = update_settings::load(dir.path());
            assert_eq!(saved.check_on_start, !shown, "stored {body:?}");
            assert_eq!(saved.skipped_version, before, "stored {body:?}");
            let file = settings_file(dir.path()).unwrap();
            assert!(file.contains(sibling), "sibling table changed:\n{file}");
        }
    }
}

#[test]
fn check_now_shows_newer_or_up_to_date() {
    let dir = tempfile::tempdir().unwrap();
    let (mut controller, events, opened) = window(dir.path());
    answer_with(&mut controller, Ok(release("v999.0.0", false)));
    let outcome = click_check_now(&mut controller, &events);
    assert!(matches!(outcome, UpdateCheck::Newer { .. }), "{outcome:?}");
    let view = controller.update_controls_view();
    assert_eq!(view.result_text, "FocusForge 999.0.0 is available.");
    assert!(view.result_visible && view.skip_enabled && !view.skipped);
    assert_eq!(
        (view.status.as_str(), view.status_tone),
        ("", StorageTone::None)
    );
    controller.open_update_page();
    assert_eq!(*opened.lock().unwrap(), [PAGE], "its url, once");
    assert_silent(&controller);

    let running = format!("v{}", env!("CARGO_PKG_VERSION"));
    let mut tags = vec!["v0.0.1"];
    if !running.contains('-') {
        tags.push(running.as_str());
    }
    for tag in tags {
        answer_with(&mut controller, Ok(release(tag, false)));
        let outcome = click_check_now(&mut controller, &events);
        assert_eq!(outcome, UpdateCheck::UpToDate, "{tag}");
        let view = controller.update_controls_view();
        assert_eq!(view.status, "FocusForge is up to date.", "{tag}");
        assert_eq!(view.status_tone, StorageTone::Success, "{tag}");
        assert!(!view.result_visible && !view.skip_enabled, "{tag}");
        assert_eq!(view.result_text, "");
        assert_silent(&controller);
    }
    controller.open_update_page();
    assert_eq!(
        opened.lock().unwrap().len(),
        1,
        "no link without a newer version"
    );
}

#[test]
fn check_now_failure_shows_reason_only_here() {
    type Case = (
        Result<String, String>,
        fn(&UpdateCheck) -> bool,
        &'static str,
    );
    let source = |o: &UpdateCheck| matches!(o, UpdateCheck::Failed(CheckFailure::Source(_)));
    let cases: [Case; 5] = [
        (
            Err("connection refused".into()),
            source,
            "connection refused",
        ),
        (Err("timed out".into()), source, "timed out"),
        (Err("unexpected HTTP status 404".into()), source, "404"),
        (
            Ok("<html>not json</html>".into()),
            |o| matches!(o, UpdateCheck::Failed(CheckFailure::Unreadable(_))),
            "release",
        ),
        (
            Ok(release("v999.0.0-rc.1", true)),
            |o| matches!(o, UpdateCheck::Failed(CheckFailure::NoFinalRelease)),
            "release",
        ),
    ];
    for (answer, is_variant, fragment) in cases {
        let dir = tempfile::tempdir().unwrap();
        let stored = "[updates]\ncheck_on_start = false\n";
        fs::write(dir.path().join("settings.toml"), stored).unwrap();
        let (mut controller, events, _) = window(dir.path());
        answer_with(&mut controller, answer.clone());
        let outcome = click_check_now(&mut controller, &events);
        assert!(is_variant(&outcome), "{answer:?}: unexpected {outcome:?}");
        let view = controller.update_controls_view();
        assert!(view.result_visible, "{answer:?}");
        assert!(
            view.failure.starts_with("Could not check for updates: ")
                && view.failure.contains(fragment),
            "{answer:?}: {:?}",
            view.failure
        );
        assert_eq!(view.status, "", "{answer:?}");
        assert!(!view.skip_enabled && view.check_enabled, "{answer:?}");
        assert_silent(&controller);
        assert_eq!(settings_file(dir.path()).as_deref(), Some(stored));
    }
}

#[test]
fn check_now_runs_one_request_in_background() {
    let dir = tempfile::tempdir().unwrap();
    let (mut controller, events, _) = window(dir.path());
    assert!(
        controller.check_for_updates_now().is_none(),
        "no source installed"
    );
    assert_eq!(controller.update_controls_view().status, "");
    assert!(controller.update_controls_view().check_enabled);

    let calls = Arc::new(AtomicUsize::new(0));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let source = Mutex::new(Some(Blocking {
        calls: calls.clone(),
        entered: entered_tx,
        release: Mutex::new(release_rx),
    }));
    controller.set_release_source(Arc::new(move || {
        Box::new(source.lock().unwrap().take().expect("one source per click"))
    }));
    let handle = controller.check_for_updates_now().expect("a thread starts");
    let source_thread = entered_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the source is asked");
    assert_ne!(source_thread, std::thread::current().id());
    assert!(!handle.is_finished() && events.lock().unwrap().is_empty());
    let view = controller.update_controls_view();
    assert_eq!(view.status, "Checking for updates…");
    assert!(view.checking && !view.check_enabled);
    assert!(
        controller.check_for_updates_now().is_none(),
        "one in flight"
    );

    release_tx.send(()).unwrap();
    handle.join().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1, "asked exactly once");
    assert!(matches!(
        events.lock().unwrap().as_slice(),
        [ShellEvent::UpdateCheckedNow(UpdateCheck::Newer { .. })]
    ));
    deliver_one(&mut controller, &events);
    let view = controller.update_controls_view();
    assert!(view.check_enabled && !view.checking);
}

#[test]
fn skip_version_silences_next_start_for_that_version() {
    let dir = tempfile::tempdir().unwrap();
    let (mut controller, events, _) = window(dir.path());
    controller.skip_update_version();
    assert_eq!(settings_file(dir.path()), None, "nothing to skip yet");

    answer_with(&mut controller, Ok(release("v999.0.0", false)));
    click_check_now(&mut controller, &events);
    controller.skip_update_version();
    let view = controller.update_controls_view();
    assert!(view.skipped && !view.skip_enabled, "{view:?}");
    assert_eq!(
        update_settings::load(dir.path()).skipped_version.as_deref(),
        Some("999.0.0")
    );

    for (tag, notified) in [("v999.0.0", false), ("v1000.0.0", true)] {
        let mut next = AppController::new();
        let settings = update_settings::load(dir.path());
        let outcome = settled(&mut next, &settings, Canned(Ok(release(tag, false))));
        next.finish_update_check(outcome);
        assert_eq!(next.notifications().len(), usize::from(notified), "{tag}");
    }
}

#[test]
fn check_now_with_switch_off() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("settings.toml"),
        "[updates]\ncheck_on_start = false\n",
    )
    .unwrap();
    let (mut controller, events, _) = window(dir.path());
    let settings = controller.update_settings().clone();
    let calls = Arc::new(AtomicUsize::new(0));
    assert!(
        controller
            .start_update_check(&settings, Box::new(Counting(calls.clone())))
            .is_none()
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the start-up check asks nothing"
    );
    let counter = calls.clone();
    controller.set_release_source(Arc::new(move || Box::new(Counting(counter.clone()))));
    click_check_now(&mut controller, &events);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "Check now asks once");

    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("settings.toml"),
        "[updates]\nskipped_version = \"999.0.0\"\n",
    )
    .unwrap();
    let (mut controller, events, _) = window(dir.path());
    answer_with(&mut controller, Ok(release("v999.0.0", false)));
    click_check_now(&mut controller, &events);
    let view = controller.update_controls_view();
    assert_eq!(
        view.result_text,
        "FocusForge 999.0.0 is available (skipped)."
    );
    assert!(view.skipped && !view.skip_enabled);
}

#[test]
fn update_controls_save_error_keeps_file() {
    let root = tempfile::tempdir().unwrap();
    let invalid = root.path().join("invalid");
    fs::create_dir(&invalid).unwrap();
    fs::write(invalid.join("settings.toml"), "updates = [oops").unwrap();
    let blocked = root.path().join("blocked");
    fs::write(&blocked, "a file where the directory should be").unwrap();
    for (dir, fragment) in [
        (&invalid, "not valid TOML"),
        (&blocked, "could not create the config directory"),
    ] {
        let before = fs::read(dir.join("settings.toml")).ok();
        let (mut controller, events, _) = window(dir);
        controller.set_update_check_on_start(false);
        assert!(!controller.update_settings().check_on_start, "{fragment}");
        let view = controller.update_controls_view();
        assert!(!view.check_on_start, "{fragment}");
        assert!(view.save_error.contains(fragment), "{:?}", view.save_error);

        answer_with(&mut controller, Ok(release("v999.0.0", false)));
        click_check_now(&mut controller, &events);
        controller.skip_update_version();
        assert_eq!(
            controller.update_settings().skipped_version.as_deref(),
            Some("999.0.0"),
            "{fragment}"
        );
        assert!(
            controller
                .update_controls_view()
                .save_error
                .contains(fragment)
        );
        assert_eq!(
            fs::read(dir.join("settings.toml")).ok(),
            before,
            "{fragment}"
        );
    }
}

#[test]
fn startup_notice_and_check_now_stay_apart() {
    for startup_first in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let (mut controller, events, _) = window(dir.path());
        let (open_startup, startup) = gated(Ok(release("v999.0.0", false)));
        let (open_now, now) = gated(Err("connection refused".into()));
        let now = Mutex::new(Some(now));
        controller.set_release_source(Arc::new(move || {
            Box::new(now.lock().unwrap().take().expect("one click"))
        }));
        let startup_handle = controller
            .start_update_check(&on(), Box::new(startup))
            .expect("the switch is on");
        let now_handle = controller.check_for_updates_now().expect("a click starts");
        assert!(controller.update_controls_view().checking);

        let mut order = [
            (open_startup, startup_handle, true),
            (open_now, now_handle, false),
        ];
        if !startup_first {
            order.reverse();
        }
        for (open, handle, is_startup) in order {
            open.send(()).unwrap();
            handle.join().unwrap();
            let event = deliver_one(&mut controller, &events);
            let view = controller.update_controls_view();
            if is_startup {
                assert!(matches!(event, ShellEvent::UpdateChecked(_)));
                assert_eq!(controller.notifications().len(), 1, "one record");
                assert_eq!(controller.notifications()[0].source, Source::Startup);
                assert_eq!(view.checking, startup_first, "the click is still in flight");
                assert_eq!(
                    view.failure.is_empty(),
                    startup_first,
                    "the start-up result leaves the click's failure in place"
                );
            } else {
                assert!(matches!(event, ShellEvent::UpdateCheckedNow(_)));
                assert!(view.failure.contains("connection refused") && !view.checking);
                assert_eq!(
                    controller.notifications().len(),
                    usize::from(startup_first),
                    "the click adds no record"
                );
            }
        }
        assert_eq!(controller.notifications().len(), 1);
    }
}

#[test]
fn about_preview_reaches_every_state() {
    for state in [
        "idle",
        "checking",
        "up-to-date",
        "newer",
        "skipped",
        "failed",
        "save-error",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (mut controller, events, _) = window(dir.path());
        let handle = controller.set_about_preview(state);
        assert_eq!(
            handle.is_some(),
            !matches!(state, "idle" | "save-error"),
            "{state}: a check starts only for the states that show a result"
        );
        if let Some(handle) = handle
            && state != "checking"
        {
            handle.join().unwrap();
            deliver_one(&mut controller, &events);
        }
        let view = controller.update_controls_view();
        match state {
            "idle" => assert!(!view.checking && !view.result_visible && view.status.is_empty()),
            "checking" => {
                assert!(view.checking && !view.check_enabled, "{view:?}");
                assert_eq!(view.status, "Checking for updates…");
                assert!(events.lock().unwrap().is_empty(), "the slow answer parks");
            }
            "up-to-date" => assert_eq!(view.status, "FocusForge is up to date."),
            "newer" => {
                assert_eq!(view.result_text, "FocusForge 999.0.0 is available.");
                assert!(view.skip_enabled);
            }
            "skipped" => {
                assert_eq!(
                    view.result_text,
                    "FocusForge 999.0.0 is available (skipped)."
                );
                assert!(view.skipped && !view.skip_enabled);
            }
            "failed" => assert!(view.failure.contains("connection refused"), "{view:?}"),
            "save-error" => assert!(
                view.save_error.contains(&dir.path().display().to_string()),
                "{:?}",
                view.save_error
            ),
            _ => unreachable!(),
        }
        assert_silent(&controller);
        assert_eq!(settings_file(dir.path()), None, "{state}: nothing written");
    }
}
