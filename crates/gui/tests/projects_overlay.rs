// SPDX-License-Identifier: GPL-3.0-or-later

//! A stack that did not open is a visible failure: the overlay's card model
//! driven through `AppController`, with no window, over copies of
//! `fixtures/mantis-512` under `CARGO_TARGET_TMPDIR`.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use focusforge_core::io::Rect;
use focusforge_core::params::ParamValues;
use focusforge_core::project;
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};
use focusforge_gui::input_map::{Action, Binding, InputTable, Modifiers, Trigger};
use focusforge_gui::overlay::CardView;
use focusforge_gui::recent::RecentEntry;
use focusforge_gui::{AppController, ChromeKey, OpenOutcome};

use common::{controller_with_frames, copy_frame, tree};

// ---------------------------------------------------------------------
// Fixture helpers -- copies only, mirroring `tests/job.rs` and
// `tests/entry.rs`.
// ---------------------------------------------------------------------

/// A fresh directory of `count` copied fixture frames, opened as one
/// unsaved project on `controller`. Returns the frames' own directory --
/// also where a save of this tab proposes its `.ffp`, beside the frames.
fn open_frames(controller: &mut AppController, name: &str, count: usize) -> PathBuf {
    let root = tree(&format!("{name}_frames"));
    for i in 0..count {
        copy_frame(&root, &format!("{i:02}.tif"), i);
    }
    assert!(matches!(
        common::open_and_settle(controller, std::slice::from_ref(&root)),
        OpenOutcome::Opened(_)
    ));
    root
}

/// [`controller_with_frames`], plus the frames directory itself -- for a
/// test that needs to reach back into the project a fuse seals there.
fn controller_with_frames_and_dir(name: &str, count: usize) -> (AppController, PathBuf) {
    let mut controller = AppController::new();
    let dir = open_frames(&mut controller, name, count);
    controller.set_scratch_root_for_test(tree(&format!("{name}_scratch")));
    (controller, dir)
}

/// [`open_frames`] over two frames, then overwrites the second with bytes
/// the TIFF reader refuses, so a fuse of it fails inside the pipeline.
fn open_unreadable_frames(controller: &mut AppController, name: &str) -> PathBuf {
    let root = open_frames(controller, name, 2);
    controller.set_align_algorithm("dummy-align");
    std::fs::write(root.join("01.tif"), b"not a tiff").unwrap();
    root
}

/// A sealed project whose sources live outside its own directory
/// (`AsGiven`) and are then deleted from that location, so the GUI's own
/// counted `missing_sources` -- which counts only what is actually
/// unreachable, not core's `AsGiven`-inclusive `unresolved_sources` --
/// genuinely reports them missing rather than merely `AsGiven`.
fn seal_as_given_project(name: &str) -> PathBuf {
    let project_dir = tree(&format!("{name}_project"));
    let frames_dir = tree(&format!("{name}_source_frames"));
    for i in 0..2 {
        copy_frame(&frames_dir, &format!("{i:02}.tif"), i);
    }
    let sources: Vec<PathBuf> = (0..2)
        .map(|i| frames_dir.join(format!("{i:02}.tif")))
        .collect();

    let staging = tree(&format!("{name}_staging"));
    let composite = staging.join("composite.tif");
    std::fs::copy(common::fixture_frame("mantis-512", 0), &composite).unwrap();
    let zmap = staging.join("zmap.tif");
    let mut writer = ZMapWriter::create(&zmap, ZMapInfo::new(4, 4, 2).unwrap()).unwrap();
    writer
        .write_tile(
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            &[0u16; 16],
        )
        .unwrap();
    writer.finish().unwrap();

    let project_path = project_dir.join("demo.ffp");
    project::seal(project::SealRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources: &sources,
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        registration: focusforge_core::align::Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: project::Metrics::default(),
    })
    .expect("seal must succeed");
    for source in &sources {
        std::fs::remove_file(source).unwrap();
    }
    project_path
}

/// A folder holding one subfolder of real frames and one subfolder with no
/// TIFF at all -- `propose`'s own skip, opened in a single call.
fn open_folder_with_a_skipped_subfolder(controller: &mut AppController, name: &str) -> PathBuf {
    let root = tree(name);
    copy_frame(&root.join("subject"), "00.tif", 0);
    std::fs::create_dir_all(root.join("thumbs")).unwrap();
    std::fs::write(root.join("thumbs").join("readme.txt"), b"not a frame").unwrap();
    assert!(matches!(
        common::open_and_settle(controller, std::slice::from_ref(&root)),
        OpenOutcome::Opened(_)
    ));
    root
}

fn wait_for_settle(controller: &mut AppController, timeout: Duration) {
    let start = std::time::Instant::now();
    loop {
        if controller.settle_job().is_some()
            || !focusforge_gui::job::is_live(&controller.job_view().state)
        {
            common::settle_thumbnails(controller);
            return;
        }
        assert!(start.elapsed() < timeout, "job did not settle in time");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn find_project_file(dir: &Path) -> PathBuf {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("ffp"))
        .expect("a sealed project file must exist")
}

fn card_by_kind<'a>(cards: &'a [CardView], kind: &str) -> &'a CardView {
    cards
        .iter()
        .find(|c| c.kind == kind)
        .unwrap_or_else(|| panic!("no {kind:?} card among {cards:#?}"))
}

// ---------------------------------------------------------------------
// The four-square button and Escape both close the overlay.
// ---------------------------------------------------------------------

#[test]
fn toggle_and_escape_both_close() {
    let mut controller = AppController::new();
    assert!(!controller.projects_overlay_open());

    controller.toggle_projects_overlay();
    assert!(controller.projects_overlay_open());

    controller.toggle_projects_overlay();
    assert!(
        !controller.projects_overlay_open(),
        "the same button closes it again"
    );

    controller.toggle_projects_overlay();
    assert!(controller.projects_overlay_open());
    assert_eq!(
        controller.chrome_key("\u{1b}", Modifiers::NONE),
        ChromeKey::Consumed,
        "Escape must be a chrome key while the overlay is open"
    );
    assert!(!controller.projects_overlay_open(), "Escape closes it too");
}

// ---------------------------------------------------------------------
// Every open project has a card; a running one shows progress; clicking
// switches to it.
// ---------------------------------------------------------------------

#[test]
fn every_open_project_has_a_card() {
    let mut controller = controller_with_frames("every_open", 3);
    open_frames(&mut controller, "every_open_second", 2);
    controller.select_project(1);

    let cards = controller.project_cards();
    assert_eq!(cards.len(), 2);
    for card in &cards {
        assert_eq!(card.kind, "open");
        assert!(
            card.meta_text.contains("frame"),
            "meta text must carry the frame count: {}",
            card.meta_text
        );
        assert!(
            card.meta_text.contains("no checkpoint yet"),
            "an unfused project has no checkpoint yet: {}",
            card.meta_text
        );
    }
    assert_eq!(
        cards.iter().filter(|c| c.current).count(),
        1,
        "exactly the front tab reads current"
    );
    assert!(cards[1].current, "tab 1 was selected");
}

#[test]
fn running_project_shows_progress_on_its_card() {
    let mut controller = controller_with_frames("running_progress", 2);
    assert!(controller.start_fuse());
    assert!(controller.job_running());

    let cards = controller.project_cards();
    let card = card_by_kind(&cards, "open");
    assert!(card.fusing, "the card whose tab is fusing must show it");
    assert!(
        (0.0..=1.0).contains(&card.fusing_fraction),
        "fusing_fraction must be a valid fraction: {}",
        card.fusing_fraction
    );

    wait_for_settle(&mut controller, Duration::from_secs(120));
}

fn align_progress(align_done: usize, align_total: usize) -> focusforge_core::job::JobProgress {
    use focusforge_core::job::JobStage;
    focusforge_core::job::JobProgress {
        stage: JobStage::Align,
        units_done: 0,
        unit_total: 0,
        pixels_done: 0,
        pixel_total: 0,
        running: Duration::ZERO,
        eta: None,
        align_done,
        align_total,
    }
}

fn fuse_progress(units_done: usize, unit_total: usize) -> focusforge_core::job::JobProgress {
    use focusforge_core::job::JobStage;
    focusforge_core::job::JobProgress {
        stage: JobStage::Fuse,
        units_done,
        unit_total,
        pixels_done: 0,
        pixel_total: 0,
        running: Duration::ZERO,
        eta: None,
        align_done: 0,
        align_total: 0,
    }
}

#[test]
fn live_card_meta_names_the_phase() {
    use focusforge_gui::job::{self, RunState};

    assert_eq!(
        job::phase_line(&RunState::Running, Some(&align_progress(3, 8))).as_deref(),
        Some("aligning, 3 of 8")
    );
    assert_eq!(
        job::phase_line(&RunState::Running, Some(&align_progress(0, 0))).as_deref(),
        Some("aligning")
    );
    assert_eq!(
        job::phase_line(&RunState::Running, Some(&fuse_progress(4, 10))).as_deref(),
        Some("fusing")
    );
    assert_eq!(
        job::phase_line(&RunState::Running, Some(&fuse_progress(10, 10))).as_deref(),
        Some("writing")
    );

    let mut controller = controller_with_frames("live_card_meta", 2);
    assert!(controller.start_fuse());
    let cards = controller.project_cards();
    let card = card_by_kind(&cards, "open");
    assert!(
        card.meta_text.starts_with("2 frames · "),
        "meta text must open with the frame count: {}",
        card.meta_text
    );
    assert!(
        !card.meta_text.contains("step"),
        "meta text must never contain 'step': {}",
        card.meta_text
    );

    wait_for_settle(&mut controller, Duration::from_secs(120));
}

#[test]
fn clicking_a_card_switches_to_it() {
    let mut controller = controller_with_frames("click_switch", 2);
    open_frames(&mut controller, "click_switch_second", 2);
    controller.select_project(0);
    controller.toggle_projects_overlay();
    assert!(controller.projects_overlay_open());

    controller.select_gallery_card(1);
    assert_eq!(controller.active_project(), Some(1));
    assert!(
        !controller.projects_overlay_open(),
        "switching closes the overlay, same as the tab strip"
    );

    // A not-opened card (index past every open tab) has nowhere to switch
    // to -- a no-op, never a panic.
    controller.toggle_projects_overlay();
    open_folder_with_a_skipped_subfolder(&mut controller, "click_switch_skip");
    let active_before = controller.active_project();
    let failure_index = controller.project_cards().len() - 1;
    controller.select_gallery_card(failure_index);
    assert_eq!(
        controller.active_project(),
        active_before,
        "a failure card click changes nothing"
    );
}

// ---------------------------------------------------------------------
// A card's picture: the project's own composite, downsampled once and
// cached; an unfused card carries its first frame; nothing is written.
// ---------------------------------------------------------------------

#[test]
fn card_picture_is_the_projects_composite() {
    let mut a = controller_with_frames("picture_a", 3);
    // An algorithm that always converges keeps the alignment check out of the test.
    a.set_align_algorithm("dummy-align");
    assert!(a.start_fuse());
    wait_for_settle(&mut a, Duration::from_secs(120));
    let picture_a = card_by_kind(&a.project_cards(), "open")
        .picture
        .clone()
        .expect("a fused project must carry a picture");

    let mut b = controller_with_frames("picture_b", 4);
    // An algorithm that always converges keeps the alignment check out of the test.
    b.set_align_algorithm("dummy-align");
    assert!(b.start_fuse());
    wait_for_settle(&mut b, Duration::from_secs(120));
    let picture_b = card_by_kind(&b.project_cards(), "open")
        .picture
        .clone()
        .expect("a fused project must carry a picture");

    assert_ne!(
        picture_a.rgba, picture_b.rgba,
        "two different projects' own composites must not render the same picture"
    );
}

#[test]
fn thumbnail_is_downsampled_once_and_cached() {
    let (mut controller, project_dir) = controller_with_frames_and_dir("cached_once", 3);
    // An algorithm that always converges keeps the alignment check out of the test.
    controller.set_align_algorithm("dummy-align");
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));

    controller.toggle_projects_overlay();
    let first_open = card_by_kind(&controller.project_cards(), "open")
        .picture
        .clone()
        .expect("a fused project must carry a picture");
    controller.toggle_projects_overlay();

    assert!(controller.save_active_project(|proposed| Some(proposed.to_path_buf())));
    let project_file = find_project_file(&project_dir);
    std::fs::remove_file(&project_file).expect("the sealed project must be removable");

    controller.toggle_projects_overlay();
    let second_open = card_by_kind(&controller.project_cards(), "open")
        .picture
        .clone()
        .expect("a cached picture must survive the project file's own removal");
    assert_eq!(
        first_open.rgba, second_open.rgba,
        "the picture must not be recomputed -- there is nothing left to read it from"
    );
}

#[test]
fn an_unfused_card_shows_its_first_frame() {
    let controller = controller_with_frames("unfused_picture", 3);
    let cards = controller.project_cards();
    let card = card_by_kind(&cards, "open");
    let picture = card
        .picture
        .as_ref()
        .expect("an unfused card still has a picture");
    assert_eq!(picture.width, focusforge_gui::overlay::THUMBNAIL_WIDTH);
    assert_eq!(picture.height, focusforge_gui::overlay::THUMBNAIL_HEIGHT);
    assert!(
        picture.rgba.iter().any(|&b| b != 0),
        "a real frame, not a blank slot"
    );
}

#[test]
fn overlay_writes_nothing_to_the_project() {
    let (mut controller, project_dir) = controller_with_frames_and_dir("writes_nothing", 3);
    // An algorithm that always converges keeps the alignment check out of the test.
    controller.set_align_algorithm("dummy-align");
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));

    assert!(controller.save_active_project(|proposed| Some(proposed.to_path_buf())));
    let project_file = find_project_file(&project_dir);
    let before_bytes = std::fs::read(&project_file).unwrap();
    let before_mtime = std::fs::metadata(&project_file)
        .unwrap()
        .modified()
        .unwrap();

    controller.toggle_projects_overlay();
    let _ = controller.project_cards();
    controller.scroll_gallery(10.0);
    controller.toggle_projects_overlay();

    let after_bytes = std::fs::read(&project_file).unwrap();
    let after_mtime = std::fs::metadata(&project_file)
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        before_bytes, after_bytes,
        "the overlay must not rewrite the project"
    );
    assert_eq!(
        before_mtime, after_mtime,
        "the overlay must not even touch the project's mtime"
    );
}

// ---------------------------------------------------------------------
// The four not-opened cards, and the one property the surface exists for.
// ---------------------------------------------------------------------

#[test]
fn four_failure_cards_have_their_shapes() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(tree("four_kinds_scratch"));

    open_folder_with_a_skipped_subfolder(&mut controller, "four_kinds_skip");

    let as_given = seal_as_given_project("four_kinds_as_given");
    assert!(matches!(
        common::open_and_settle(&mut controller, &[as_given]),
        OpenOutcome::Opened(_)
    ));

    open_unreadable_frames(&mut controller, "four_kinds_fail");
    controller.select_project(controller.project_tabs().len() - 1);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(60));

    open_frames(&mut controller, "four_kinds_cancel", 2);
    controller.select_project(controller.project_tabs().len() - 1);
    assert!(controller.start_fuse());
    controller.cancel_fuse();
    wait_for_settle(&mut controller, Duration::from_secs(120));

    let cards = controller.project_cards();
    assert_eq!(
        cards.len(),
        controller.project_tabs().len() + 1,
        "one card per open tab, plus the one not-a-project card with no tab"
    );

    let not_a_project = card_by_kind(&cards, "not-a-project");
    assert_eq!(not_a_project.headline, "Not a project");
    assert_eq!(
        not_a_project.action_label, "",
        "Not a project has no action"
    );
    assert!(!not_a_project.opened, "the only unopened card");

    let sources_missing = card_by_kind(&cards, "sources-missing");
    assert_eq!(sources_missing.headline, "Sources missing");
    assert_eq!(sources_missing.action_label, "Locate frames…");
    assert!(sources_missing.opened, "merged onto its own open project");

    let failed = card_by_kind(&cards, "failed");
    assert_eq!(failed.headline, "Did not stack");
    assert_eq!(failed.action_label, "Run again");
    assert!(!failed.reason.is_empty());
    assert!(failed.opened, "merged onto its own open project");

    let cancelled = card_by_kind(&cards, "cancelled");
    assert_eq!(cancelled.headline, "Cancelled");
    assert_eq!(cancelled.action_label, "Run again");
    assert!(cancelled.opened, "merged onto its own open project");

    assert_eq!(
        controller.gallery_count_text(),
        "5 · 1 did not open",
        "did not open counts only the not-a-project card"
    );
}

#[test]
fn a_failed_stack_updates_the_open_tab_not_a_second_card() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(tree("present_not_absent_scratch"));
    open_unreadable_frames(&mut controller, "present_not_absent");
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(60));

    // The tab itself stays open; its own card takes on the failure instead
    // of a second card appearing for the same attempt.
    assert_eq!(controller.project_tabs().len(), 1);
    let cards = controller.project_cards();
    assert_eq!(cards.len(), 1, "one tab, one card -- no duplicate");
    let failure = card_by_kind(&cards, "failed");
    assert_eq!(failure.headline, "Did not stack");
    assert!(failure.opened);
}

#[test]
fn a_failed_run_updates_its_own_card() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(tree("abc_scratch"));
    open_frames(&mut controller, "abc_a", 2);
    let mid_dir = open_frames(&mut controller, "abc_mid", 3);
    open_frames(&mut controller, "abc_c", 2);
    std::fs::write(mid_dir.join("01.tif"), b"not a tiff").unwrap();

    controller.select_project(1);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(60));

    let cards = controller.project_cards();
    assert_eq!(cards.len(), 3, "one card per open tab, no duplicate");
    assert_eq!(cards[0].kind, "open");
    assert_eq!(cards[1].kind, "failed");
    assert!(
        cards[1].opened,
        "the merged card stays its own open project's card"
    );
    assert!(cards[1].current, "the failed tab is still the active one");
    assert_eq!(cards[2].kind, "open");
    assert_eq!(controller.gallery_count_text(), "3 open");
}

#[test]
fn a_failure_keeps_earlier_checkpoints() {
    let (mut controller, dir) = controller_with_frames_and_dir("keeps_checkpoints", 2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(60));
    assert_eq!(controller.checkpoint_count(), Some(1));

    std::fs::write(dir.join("01.tif"), b"not a tiff").unwrap();
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(60));

    assert_eq!(
        controller.checkpoint_count(),
        Some(1),
        "the earlier checkpoint survives a later failure"
    );
    let cards = controller.project_cards();
    let card = card_by_kind(&cards, "failed");
    assert!(card.meta_text.ends_with("1 checkpoint"));
    assert!(card.unsaved, "the held checkpoint keeps the tab unsaved");
}

#[test]
fn run_again_stays_on_one_card_until_success() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(tree("run_again_scratch"));
    open_frames(&mut controller, "run_again_healthy", 2);
    let fail_dir = open_unreadable_frames(&mut controller, "run_again_fail");
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(60));

    assert!(
        !controller.run_gallery_action(0),
        "the healthy card's own tab carries no failure to replay"
    );

    assert!(controller.run_gallery_action(1));
    wait_for_settle(&mut controller, Duration::from_secs(60));
    assert_eq!(
        controller.active_project(),
        Some(1),
        "run again makes its tab active"
    );
    assert_eq!(
        controller.project_cards().len(),
        2,
        "still one card per tab"
    );

    assert!(controller.run_gallery_action(1));
    wait_for_settle(&mut controller, Duration::from_secs(60));
    assert_eq!(
        controller.project_cards().len(),
        2,
        "a repeated failure updates the same card, not a new one"
    );

    std::fs::copy(
        common::fixture_frame("mantis-512", 1),
        fail_dir.join("01.tif"),
    )
    .unwrap();
    assert!(controller.run_gallery_action(1));
    wait_for_settle(&mut controller, Duration::from_secs(60));
    let cards = controller.project_cards();
    assert_eq!(cards.len(), 2);
    assert_eq!(cards[1].kind, "open");
    assert_eq!(controller.checkpoint_count(), Some(1));
}

/// A window backend, initialized once per thread -- `AppWindow::new()`
/// otherwise panics on a second call from the same thread.
fn init_window_backend() {
    thread_local! {
        static INIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    INIT.with(|done| {
        if !done.get() {
            i_slint_backend_testing::init_no_event_loop();
            done.set(true);
        }
    });
}

/// Run again on a failed card, started from inside the open overview,
/// raises no toast for its own failure: the overview stays open, a second
/// record is kept, and the card it re-ran still reads `failed` with one
/// card per tab.
#[test]
fn run_again_failure_over_the_overview_raises_no_toast() {
    use slint::ComponentHandle;
    use std::sync::{Arc, Mutex};

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));

    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(tree("run_again_over_overview_scratch"));
    open_frames(&mut controller, "run_again_over_overview_healthy", 2);
    open_unreadable_frames(&mut controller, "run_again_over_overview_failing");
    let controller = Arc::new(Mutex::new(controller));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    app.set_project_loaded(true);

    // The first failure, from the tab's own Fuse stack -- let its toast
    // expire so the second opening below is the one under test.
    app.invoke_fuse_requested();
    common::settle_via_job_timer(&controller, Duration::from_secs(60));
    assert!(app.get_toast_open());
    controller
        .lock()
        .unwrap()
        .expire_toast(std::time::Instant::now() + Duration::from_secs(7));
    focusforge_gui::bridge::push_toast(
        &app,
        &controller,
        &std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    assert!(
        !app.get_toast_open(),
        "the first toast must have expired before the retry"
    );

    controller.lock().unwrap().toggle_projects_overlay();
    focusforge_gui::bridge::push_projects_overlay(&app, &controller.lock().unwrap());

    let failing_index = 1;
    app.invoke_projects_card_action_clicked(failing_index as i32);
    common::settle_via_job_timer(&controller, Duration::from_secs(60));

    assert!(
        !app.get_toast_open(),
        "Projects shows no toast for Run again's failure"
    );
    assert_eq!(controller.lock().unwrap().notifications().len(), 2);
    assert!(
        app.get_projects_overlay_open(),
        "the overview must stay open"
    );

    let cards = controller.lock().unwrap().project_cards();
    assert_eq!(cards.len(), 2, "still one card per tab");
    assert_eq!(cards[failing_index].kind, "failed");
}

#[test]
fn a_cancelled_run_stays_on_its_own_card() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(tree("cancel_scratch"));
    open_frames(&mut controller, "cancel_a", 2);
    open_frames(&mut controller, "cancel_b", 2);
    controller.select_project(1);
    assert!(controller.start_fuse());
    controller.cancel_fuse();
    wait_for_settle(&mut controller, Duration::from_secs(120));

    let cards = controller.project_cards();
    assert_eq!(
        cards.len(),
        2,
        "one card per tab, no separate cancelled card"
    );
    let cancelled = card_by_kind(&cards, "cancelled");
    assert!(cancelled.opened);
    assert_eq!(cancelled.project_name, "cancel_b_frames");
    assert_eq!(controller.gallery_count_text(), "2 open");
}

#[test]
fn a_failure_survives_a_save_and_leaves_with_its_tab() {
    let (mut controller, dir) = controller_with_frames_and_dir("survives_save", 2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(60));
    assert_eq!(controller.checkpoint_count(), Some(1));

    std::fs::write(dir.join("01.tif"), b"not a tiff").unwrap();
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(60));
    assert_eq!(
        card_by_kind(&controller.project_cards(), "failed").project_name,
        "survives_save_frames"
    );

    assert!(controller.save_active_project(|proposed| Some(proposed.to_path_buf())));
    let cards = controller.project_cards();
    assert_eq!(cards.len(), 1);
    assert_eq!(
        cards[0].kind, "failed",
        "the failure survives the Unsaved -> Opened identity change"
    );

    controller.close_project(0);
    assert!(
        controller.project_cards().is_empty(),
        "the failure leaves with its tab"
    );
}

#[test]
fn sources_missing_is_located_on_its_own_card() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(tree("sources_missing_scratch"));
    let project_path = seal_as_given_project("sources_missing");
    let frames_dir =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sources_missing_source_frames");
    assert!(matches!(
        common::open_and_settle(&mut controller, &[project_path]),
        OpenOutcome::Opened(_)
    ));

    assert_eq!(controller.gallery_failure_kind(0), Some("sources-missing"));

    // The right folder, byte-identical to what the project was sealed
    // over -- `copy_frame` always copies from the same golden fixture, so
    // recreating it here reproduces the same content hash.
    std::fs::create_dir_all(&frames_dir).unwrap();
    for i in 0..2 {
        copy_frame(&frames_dir, &format!("{i:02}.tif"), i);
    }
    let tab_id = controller.tab_id_at(0).unwrap();
    common::locate_and_settle(
        &mut controller,
        focusforge_gui::entry::LocateTarget::Tab(tab_id),
        frames_dir,
    );
    assert_eq!(controller.gallery_failure_kind(0), None);
    assert_eq!(controller.project_cards()[0].kind, "open");
}

#[test]
fn locate_frames_refuses_a_folder_of_different_content() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(tree("sources_missing_wrong_scratch"));
    let project_path = seal_as_given_project("sources_missing_wrong");
    let wrong_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sources_missing_wrong_folder");
    std::fs::create_dir_all(&wrong_dir).unwrap();
    std::fs::write(wrong_dir.join("00.tif"), b"not the right bytes").unwrap();
    std::fs::write(wrong_dir.join("01.tif"), b"not the right bytes either").unwrap();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[project_path]),
        OpenOutcome::Opened(_)
    ));

    let tab_id = controller.tab_id_at(0).unwrap();
    common::locate_and_settle(
        &mut controller,
        focusforge_gui::entry::LocateTarget::Tab(tab_id),
        wrong_dir,
    );
    assert_eq!(
        controller.gallery_failure_kind(0),
        Some("sources-missing"),
        "a folder of different content must not clear the card"
    );
    assert!(controller.last_refusal().is_some());
}

// ---------------------------------------------------------------------
// No card shows a measure; no control acts on several cards at once.
// ---------------------------------------------------------------------

#[test]
fn no_card_shows_a_measure() {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(tree("no_measure_scratch"));
    open_folder_with_a_skipped_subfolder(&mut controller, "no_measure_skip");
    let as_given = seal_as_given_project("no_measure_as_given");
    assert!(matches!(
        common::open_and_settle(&mut controller, &[as_given]),
        OpenOutcome::Opened(_)
    ));
    open_unreadable_frames(&mut controller, "no_measure_fail");
    controller.select_project(controller.project_tabs().len() - 1);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(60));

    let forbidden = ["%", "ratio", "sharpn", "coheren"];
    let mut found = Vec::new();
    for card in controller.project_cards() {
        for field in [
            &card.project_name,
            &card.meta_text,
            &card.headline,
            &card.stage,
            &card.reason,
            &card.action_label,
        ] {
            let lower = field.to_lowercase();
            for needle in forbidden {
                if lower.contains(needle) {
                    found.push(format!("{needle:?} in {field:?}"));
                }
            }
        }
    }
    assert!(found.is_empty(), "found measure-shaped text: {found:?}");
}

#[test]
fn no_control_acts_on_several_cards() {
    // A scan, not a type check: every gallery action the controller exposes
    // takes exactly one card's own index, never a collection of them.
    let path = common::src_dir().join("controller.rs");
    let contents = std::fs::read_to_string(&path).unwrap();
    let start = contents
        .find("-- The projects overlay:")
        .expect("the overlay section must exist");
    let after = &contents[start..];
    let end = after[1..]
        .find("\n    // -- ")
        .map(|i| i + 1)
        .unwrap_or(after.len());
    let section = common::strip_line_comments(&after[..end]);

    // "selected" is deliberately not in this list: a single tab's own
    // one-frame `selected` field is a scalar, not a batch of cards.
    let forbidden = ["Vec<usize>", "[usize]", "indices", "multi"];
    let mut found = Vec::new();
    for needle in forbidden {
        if section.contains(needle) {
            found.push(needle);
        }
    }
    assert!(
        found.is_empty(),
        "the overlay's own controller section names a batch control: {found:?}"
    );
}

// ---------------------------------------------------------------------
// The rail's actions and shortcuts; `Esc`/`Ctrl+N/O/S/W` cannot be
// shadowed by a rebind; the `Recent` list persists.
// ---------------------------------------------------------------------

#[test]
fn rail_actions_and_shortcuts() {
    let mut controller = controller_with_frames("rail_actions", 2);
    controller.toggle_projects_overlay();

    assert_eq!(
        controller.chrome_key(
            "n",
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            }
        ),
        ChromeKey::NewProject
    );
    assert_eq!(
        controller.chrome_key(
            "o",
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            }
        ),
        ChromeKey::OpenProject
    );
    assert_eq!(
        controller.chrome_key(
            "s",
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            }
        ),
        ChromeKey::Consumed,
        "Ctrl+S does nothing while Save project is disabled"
    );
    let tabs_before = controller.project_tabs().len();
    assert_eq!(
        controller.chrome_key(
            "w",
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            }
        ),
        ChromeKey::Consumed
    );
    assert_eq!(
        controller.project_tabs().len(),
        tabs_before - 1,
        "Ctrl+W actually closes the active project"
    );

    // A bare, unmodified key is not a chrome key at all.
    assert_eq!(controller.chrome_key("n", Modifiers::NONE), ChromeKey::None);
}

#[test]
fn a_rebind_cannot_shadow_a_chrome_key() {
    let mut controller = controller_with_frames("rebind_shadow", 2);
    let ctrl = Modifiers {
        ctrl: true,
        ..Modifiers::NONE
    };
    let mut table = InputTable::default();
    for key in ['\u{1b}', 'n', 'o', 's', 'w'] {
        table.bindings.push(Binding {
            trigger: Trigger::Key {
                key,
                mods: if key == '\u{1b}' {
                    Modifiers::NONE
                } else {
                    ctrl
                },
            },
            stacking: Some(Action::ZoomAtCursor),
            editor: Some(Action::ZoomAtCursor),
        });
    }
    controller.install_input_table(table);

    // The overlay closed: the rebind still cannot shadow the chords.
    assert!(!controller.projects_overlay_open());
    assert_eq!(controller.chrome_key("n", ctrl), ChromeKey::NewProject);
    assert_eq!(controller.chrome_key("o", ctrl), ChromeKey::OpenProject);
    assert_eq!(controller.chrome_key("s", ctrl), ChromeKey::Consumed);
    let tabs_before = controller.project_tabs().len();
    assert_eq!(controller.chrome_key("w", ctrl), ChromeKey::Consumed);
    assert_eq!(
        controller.project_tabs().len(),
        tabs_before - 1,
        "Ctrl+W closes a tab with the overlay closed too"
    );

    // A fresh tab for the overlay-open half of the test. Past
    // `AppController::CLOSE_DEBOUNCE`, so this close is not the first one's
    // double-click guard swallowing it.
    open_frames(&mut controller, "rebind_shadow_reopen", 2);
    std::thread::sleep(Duration::from_millis(350));

    controller.toggle_projects_overlay();
    assert_eq!(controller.chrome_key("n", ctrl), ChromeKey::NewProject);
    assert_eq!(controller.chrome_key("o", ctrl), ChromeKey::OpenProject);
    assert_eq!(controller.chrome_key("s", ctrl), ChromeKey::Consumed);
    let tabs_before = controller.project_tabs().len();
    assert_eq!(controller.chrome_key("w", ctrl), ChromeKey::Consumed);
    assert_eq!(controller.project_tabs().len(), tabs_before - 1);
    assert_eq!(
        controller.chrome_key("\u{1b}", Modifiers::NONE),
        ChromeKey::Consumed
    );
    assert!(!controller.projects_overlay_open());
}

#[test]
fn chrome_keys_work_with_the_overlay_closed() {
    let ctrl = Modifiers {
        ctrl: true,
        ..Modifiers::NONE
    };
    let mut controller = controller_with_frames("closed_chords", 2);
    assert!(!controller.projects_overlay_open());

    assert_eq!(controller.chrome_key("n", ctrl), ChromeKey::NewProject);
    assert_eq!(controller.chrome_key("N", ctrl), ChromeKey::NewProject);
    assert_eq!(controller.chrome_key("o", ctrl), ChromeKey::OpenProject);
    assert_eq!(controller.chrome_key("O", ctrl), ChromeKey::OpenProject);
    assert_eq!(
        controller.chrome_key("s", ctrl),
        ChromeKey::Consumed,
        "nothing fused yet"
    );

    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));
    assert_eq!(controller.chrome_key("s", ctrl), ChromeKey::SaveProject);
    assert_eq!(controller.chrome_key("S", ctrl), ChromeKey::SaveProject);
    assert!(controller.save_active_project(|proposed| Some(proposed.to_path_buf())));
    assert!(!controller.save_enabled(), "the save leaves nothing held");

    // Boundaries: `Esc`, a bare letter, `Ctrl+W` with zero tabs.
    assert_eq!(
        controller.chrome_key("\u{1b}", Modifiers::NONE),
        ChromeKey::None
    );
    assert!(!controller.projects_overlay_open());
    assert_eq!(controller.chrome_key("n", Modifiers::NONE), ChromeKey::None);

    let tabs_before = controller.project_tabs().len();
    assert_eq!(controller.chrome_key("w", ctrl), ChromeKey::Consumed);
    assert_eq!(
        controller.project_tabs().len(),
        tabs_before - 1,
        "Ctrl+W closes a tab with nothing to lose"
    );
    assert!(controller.pending_close().is_none());

    assert_eq!(
        controller.chrome_key("w", ctrl),
        ChromeKey::Consumed,
        "Ctrl+W with no project tab open must not panic"
    );
}

#[test]
fn recent_list_persists_and_caps_at_five() {
    use focusforge_gui::recent;

    let config_dir = tree("recent_config");
    let mut controller = AppController::new();
    controller.install_recent(recent::load(&config_dir), config_dir.clone());
    controller.set_scratch_root_for_test(tree("recent_scratch"));

    for i in 0..6 {
        let project = seal_as_given_project(&format!("recent_open_{i}"));
        common::open_and_settle(&mut controller, &[project]);
    }

    let rows = controller.recent_rows(0);
    assert_eq!(rows.len(), 5, "the list caps at five entries");
    assert!(
        rows[0].folder.contains("recent_open_5_project"),
        "the newest open is first: {}",
        rows[0].folder
    );

    let mut fresh = AppController::new();
    fresh.install_recent(recent::load(&config_dir), config_dir.clone());
    assert_eq!(
        fresh.recent_rows(0),
        rows,
        "the list must survive a restart, loaded from the same config dir"
    );

    let missing_dir = tree("recent_missing");
    assert!(recent::load(&missing_dir).is_empty());

    let corrupt_dir = tree("recent_corrupt");
    std::fs::write(corrupt_dir.join("settings.toml"), b"not valid toml {{{").unwrap();
    assert!(
        recent::load(&corrupt_dir).is_empty(),
        "an unparsable file must read as an empty list, not a panic"
    );
    let mut after_corrupt = AppController::new();
    after_corrupt.install_recent(recent::load(&corrupt_dir), corrupt_dir);
    assert!(after_corrupt.recent_rows(0).is_empty());
}

#[test]
fn opening_a_folder_group_records_nothing_in_recent() {
    use focusforge_gui::recent;

    let config_dir = tree("recent_folder_group_config");
    let mut controller = AppController::new();
    controller.install_recent(recent::load(&config_dir), config_dir.clone());
    open_frames(&mut controller, "recent_folder_group", 2);
    assert!(controller.recent_rows(0).is_empty());
    assert!(!config_dir.join("settings.toml").exists());

    let project = seal_as_given_project("recent_folder_group_sealed");
    common::open_and_settle(&mut controller, std::slice::from_ref(&project));
    assert_eq!(controller.recent_rows(0).len(), 1);
    let loaded = recent::load(&config_dir);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].path, project);
}

#[test]
fn old_folder_entries_are_dropped_when_recent_loads() {
    use focusforge_gui::recent;

    let config_dir = tree("recent_old_folders_config");
    let entry = |name: &str, path: &str| RecentEntry {
        name: name.to_string(),
        folder: config_dir.clone(),
        path: config_dir.join(path),
        opened_at: 0,
    };
    let written = [
        entry("frames", "frames"),
        entry("lower", "lower.ffp"),
        entry("upper", "UPPER.FFP"),
        entry("gone", "gone/missing.ffp"),
        entry("other", "notes.txt"),
    ];
    recent::save(&config_dir, &written).unwrap();

    let names: Vec<String> = recent::load(&config_dir)
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["lower", "upper", "gone"]);
}

// ---------------------------------------------------------------------
// A click on a `Recent` row opens what that row names.
// ---------------------------------------------------------------------

#[test]
fn recent_row_opens_its_target() {
    let project_path = seal_as_given_project("recent_open_target");
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(tree("recent_open_target_scratch"));
    controller.install_recent(
        vec![RecentEntry {
            name: "demo".to_string(),
            folder: project_path.parent().unwrap().to_path_buf(),
            path: project_path.clone(),
            opened_at: 0,
        }],
        tree("recent_open_target_config"),
    );
    controller.toggle_projects_overlay();
    assert!(controller.projects_overlay_open());

    let handle = controller
        .open_recent(0)
        .expect("a target with no open tab must start a load, same as `Open project…`");
    handle.join().expect("the load worker did not panic");
    let outcome = controller.settle_open();
    assert!(
        matches!(outcome, Some(OpenOutcome::Opened(_))),
        "the same outcome `Open project…` gives for this target"
    );

    assert_eq!(
        controller.project_tabs().len(),
        1,
        "the opened project is added as a tab"
    );
    assert_eq!(
        controller.active_project(),
        Some(0),
        "the first opened project is selected"
    );
    assert!(
        !controller.projects_overlay_open(),
        "a successful open closes the overlay"
    );
}

/// A fresh directory of `count` copied fixture frames, not yet opened.
fn frames_dir(name: &str, count: usize) -> PathBuf {
    let root = tree(&format!("{name}_frames"));
    for i in 0..count {
        copy_frame(&root, &format!("{i:02}.tif"), i);
    }
    root
}

#[test]
fn overlay_opened_during_a_load_survives_it() {
    let mut controller = AppController::new();
    let first = frames_dir("overlay_during_load_first", 2);
    let second = frames_dir("overlay_during_load_second", 2);

    let handle = controller
        .open_paths(std::slice::from_ref(&first))
        .expect("the first load starts");
    assert!(controller.load_in_flight());
    controller.toggle_projects_overlay();
    handle.join().expect("the load worker did not panic");
    assert!(matches!(
        controller.settle_open(),
        Some(OpenOutcome::Opened(_))
    ));
    assert!(
        controller.projects_overlay_open(),
        "an overlay opened while the load ran stays open when it lands"
    );

    assert!(matches!(
        common::open_and_settle(&mut controller, std::slice::from_ref(&second)),
        OpenOutcome::Opened(_)
    ));
    assert!(
        controller.projects_overlay_open(),
        "a second load landing leaves it open too"
    );

    controller.select_project(0);
    assert!(
        !controller.projects_overlay_open(),
        "a tab switch closes an overlay that survived a load"
    );

    let third = frames_dir("overlay_during_load_third", 2);
    assert!(matches!(
        common::open_and_settle(&mut controller, std::slice::from_ref(&third)),
        OpenOutcome::Opened(_)
    ));
    assert!(
        !controller.projects_overlay_open(),
        "the overlay stays closed after a later load"
    );
}

#[test]
fn overlay_opened_before_a_load_still_closes_when_it_lands() {
    let mut controller = AppController::new();
    let first = frames_dir("overlay_before_load_first", 2);
    controller.toggle_projects_overlay();
    assert!(matches!(
        common::open_and_settle(&mut controller, std::slice::from_ref(&first)),
        OpenOutcome::Opened(_)
    ));
    assert!(
        !controller.projects_overlay_open(),
        "an overlay open before the load started closes on arrival"
    );
}

#[test]
fn recent_row_switches_to_an_open_tab() {
    let (mut controller, first_dir) = controller_with_frames_and_dir("recent_switch", 2);
    open_frames(&mut controller, "recent_switch_second", 2);
    assert_eq!(controller.active_project(), Some(1));

    controller.install_recent(
        vec![RecentEntry {
            name: "first".to_string(),
            folder: first_dir.clone(),
            path: first_dir,
            opened_at: 0,
        }],
        tree("recent_switch_config"),
    );
    controller.toggle_projects_overlay();

    assert!(
        controller.open_recent(0).is_none(),
        "a target already open in a tab starts no new load"
    );
    assert_eq!(
        controller.active_project(),
        Some(0),
        "the click switches to the tab already holding that target"
    );
    assert_eq!(
        controller.project_tabs().len(),
        2,
        "no second tab is opened for the same target"
    );
    assert!(
        !controller.projects_overlay_open(),
        "switching tabs closes the overlay too"
    );
}

#[test]
fn recent_row_missing_target_is_refused() {
    let mut controller = controller_with_frames("recent_missing_target", 1);
    let missing_dir = tree("recent_missing_target_gone");
    std::fs::remove_dir_all(&missing_dir).expect("the target must exist, then vanish");
    controller.install_recent(
        vec![RecentEntry {
            name: "gone".to_string(),
            folder: missing_dir.clone(),
            path: missing_dir.clone(),
            opened_at: 0,
        }],
        tree("recent_missing_target_config"),
    );
    controller.toggle_projects_overlay();
    let tabs_before = controller.project_tabs();
    let active_before = controller.active_project();

    assert!(
        controller.open_recent(0).is_none(),
        "a missing target starts no worker"
    );
    assert_eq!(
        controller.project_tabs(),
        tabs_before,
        "the tab list is unchanged"
    );
    assert_eq!(
        controller.active_project(),
        active_before,
        "the active tab is unchanged"
    );

    let mut baseline = AppController::new();
    baseline.open_paths(&[missing_dir]);
    assert_eq!(
        controller.last_refusal(),
        baseline.last_refusal(),
        "the same refusal `open_paths` raises for the same missing path"
    );
}

#[test]
fn recent_row_out_of_range_or_busy_is_ignored() {
    let mut controller = controller_with_frames("recent_out_of_range", 1);
    let only_dir = tree("recent_out_of_range_folder");
    controller.install_recent(
        vec![RecentEntry {
            name: "only".to_string(),
            folder: only_dir.clone(),
            path: only_dir,
            opened_at: 0,
        }],
        tree("recent_out_of_range_config"),
    );
    let tabs_before = controller.project_tabs();
    let active_before = controller.active_project();

    assert!(
        controller.open_recent(5).is_none(),
        "an index past the end of the list starts no worker"
    );
    assert_eq!(controller.project_tabs(), tabs_before);
    assert_eq!(controller.active_project(), active_before);

    let busy_dir = tree("recent_out_of_range_busy_target");
    copy_frame(&busy_dir, "00.tif", 0);
    let second_dir = tree("recent_out_of_range_second_target");
    copy_frame(&second_dir, "00.tif", 1);
    let mut busy_controller = AppController::new();
    busy_controller.set_scratch_root_for_test(tree("recent_out_of_range_busy_scratch"));
    busy_controller.install_recent(
        vec![RecentEntry {
            name: "second".to_string(),
            folder: second_dir.clone(),
            path: second_dir,
            opened_at: 0,
        }],
        tree("recent_out_of_range_busy_config"),
    );
    let handle = busy_controller
        .open_paths(&[busy_dir])
        .expect("the first load must start");

    assert!(
        busy_controller.open_recent(0).is_none(),
        "a click while a load is in flight starts no second worker"
    );
    assert!(
        busy_controller.project_tabs().is_empty(),
        "the dropped click adds no tab"
    );

    handle.join().expect("the load worker did not panic");
    busy_controller.settle_open();
}

#[test]
fn recent_row_opens_its_path_not_its_folder() {
    use focusforge_gui::recent;

    let project_path = seal_as_given_project("recent_path_project");

    let config_dir = tree("recent_path_config");
    recent::save(
        &config_dir,
        &[RecentEntry {
            name: "demo".to_string(),
            folder: config_dir.join("no_such_folder"),
            path: project_path,
            opened_at: 0,
        }],
    )
    .unwrap();

    let entries = recent::load(&config_dir);
    assert_eq!(entries.len(), 1, "the saved row loads back");

    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(tree("recent_path_scratch"));
    controller.install_recent(entries, config_dir);
    controller.toggle_projects_overlay();

    let handle = controller
        .open_recent(0)
        .expect("the row's `path` starts a load although its `folder` does not exist");
    handle.join().expect("the load worker did not panic");
    let outcome = controller.settle_open();
    assert!(
        matches!(outcome, Some(OpenOutcome::Opened(_))),
        "the path opens the same as any other target"
    );
    assert_eq!(controller.project_tabs().len(), 1);
}

// ---------------------------------------------------------------------
// The grid arithmetic and the scroll offset it clamps against.
// ---------------------------------------------------------------------

#[test]
fn gallery_lays_out_and_scrolls() {
    use focusforge_gui::overlay::{
        CARD_GAP, CARD_HEIGHT, CARD_WIDTH, card_origin, columns, content_height,
    };

    // A width that fits two whole columns and half of a third.
    let width = 2.0 * CARD_WIDTH + CARD_GAP + CARD_WIDTH / 2.0;
    assert_eq!(columns(width), 2);
    assert_eq!(content_height(5, 2), 3.0 * CARD_HEIGHT + 2.0 * CARD_GAP);
    assert_eq!(card_origin(2, 2), (0.0, CARD_HEIGHT + CARD_GAP));
    assert_eq!(
        card_origin(3, 2),
        (CARD_WIDTH + CARD_GAP, CARD_HEIGHT + CARD_GAP)
    );

    let mut controller = controller_with_frames("gallery_scroll", 1);
    for i in 0..5 {
        open_frames(&mut controller, &format!("gallery_scroll_{i}"), 1);
    }
    controller.set_gallery_width(width);
    controller.set_gallery_viewport_height(300.0);

    controller.scroll_gallery(100_000.0);
    let content = controller.gallery_content_height_px();
    let max_scroll = (content - 300.0).max(0.0);
    assert_eq!(
        controller.gallery_scroll_y(),
        max_scroll,
        "scroll clamps at the bottom"
    );

    controller.scroll_gallery(-100_000.0);
    assert_eq!(
        controller.gallery_scroll_y(),
        0.0,
        "scroll clamps at the top"
    );
}

// ---------------------------------------------------------------------
// Scrolling and progress reuse every picture; a hidden overlay publishes
// nothing until it opens again.
// ---------------------------------------------------------------------

fn element_center(handle: &i_slint_backend_testing::ElementHandle) -> slint::LogicalPosition {
    let pos = handle.absolute_position();
    let size = handle.size();
    slint::LogicalPosition::new(pos.x + size.width / 2.0, pos.y + size.height / 2.0)
}

/// Runs the calling test again alone in a child process -- timing
/// accounting is process-global. Returns true in the parent, which is done.
fn ran_in_child(test: &str) -> bool {
    const CHILD: &str = "FOCUSFORGE_GUI_OVERLAY_ACCOUNTING_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return false;
    }
    let status =
        std::process::Command::new(std::env::current_exe().expect("the test binary has a path"))
            .args(["--exact", test, "--test-threads=1"])
            .env(CHILD, "1")
            .status()
            .expect("the isolated accounting test starts");
    assert!(
        status.success(),
        "the isolated accounting test passes: {status}"
    );
    true
}

/// `(row_updates, thumbnail_bytes)` so far.
fn overlay_counters() -> (u64, u64) {
    let snapshot = focusforge_core::timing::timing_snapshot().expect("trace accounting is enabled");
    (snapshot.row_updates, snapshot.thumbnail_bytes)
}

type Captured = std::sync::Arc<std::sync::Mutex<Vec<focusforge_gui::ShellEvent>>>;

/// A 1100x500 window over three two-frame projects with project 0 active,
/// the bridge installed and every event captured rather than performed.
fn overlay_window(
    name: &str,
    recent_dir: Option<&Path>,
) -> (
    focusforge_gui::AppWindow,
    std::sync::Arc<std::sync::Mutex<AppController>>,
    Captured,
    focusforge_gui::bridge::Installed,
) {
    use slint::ComponentHandle;
    use std::sync::{Arc, Mutex};

    focusforge_core::timing::enable_trace_accounting();
    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1100, 500));
    let mut controller = AppController::new();
    if let Some(dir) = recent_dir {
        controller.install_recent(focusforge_gui::recent::load(dir), dir.to_path_buf());
    }
    controller.set_scratch_root_for_test(tree(&format!("{name}_scratch")));
    for i in 0..3 {
        open_frames(&mut controller, &format!("{name}_{i}"), 2);
    }
    controller.select_project(0);
    let controller = Arc::new(Mutex::new(controller));
    let installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    let events: Captured = Arc::new(Mutex::new(Vec::new()));
    controller.lock().unwrap().set_sink(Arc::new({
        let events = events.clone();
        move |event| events.lock().unwrap().push(event)
    }));
    app.set_project_loaded(true);
    (app, controller, events, installed)
}

fn shown_pictures(app: &focusforge_gui::AppWindow) -> Vec<slint::Image> {
    use slint::Model;
    app.get_project_cards().iter().map(|c| c.picture).collect()
}

#[test]
fn gallery_scrolling_and_progress_reuse_every_picture() {
    use focusforge_gui::ShellEvent;
    use slint::platform::{PointerEventButton, WindowEvent};
    use slint::{ComponentHandle, Model};

    if ran_in_child("gallery_scrolling_and_progress_reuse_every_picture") {
        return;
    }
    let (app, controller, events, _installed) = overlay_window("gallery_reuse", None);
    let push = || {
        focusforge_gui::bridge::push_projects_overlay(&app, &controller.lock().unwrap());
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
    };
    // What the event loop does with each captured event.
    let perform = || {
        let drained: Vec<ShellEvent> = events.lock().unwrap().drain(..).collect();
        for event in &drained {
            match event {
                ShellEvent::GalleryScrolled => {
                    app.set_gallery_scroll_y(controller.lock().unwrap().gallery_scroll_y())
                }
                ShellEvent::OverlayChanged => push(),
                _ => {}
            }
        }
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
        drained
    };
    let dispatch = |event: WindowEvent| {
        app.window().dispatch_event(event);
        perform()
    };

    controller.lock().unwrap().toggle_projects_overlay();
    perform();
    perform();
    let before = overlay_counters();
    let pictures = shown_pictures(&app);
    assert_eq!(pictures.len(), 3);

    let hit = i_slint_backend_testing::ElementQuery::from_root(&app)
        .match_id("FFProjectsGallery::hit-area")
        .find_first()
        .expect("the open gallery is mounted");
    let mut seen = dispatch(WindowEvent::PointerScrolled {
        position: element_center(&hit),
        delta_x: 0.0,
        delta_y: -100_000.0,
    });
    let bottom = controller.lock().unwrap().gallery_scroll_y();
    assert!(bottom > 0.0, "the wheel moved the gallery");
    assert_eq!(app.get_gallery_scroll_y(), bottom);

    // A press on the bare track above the thumb jumps back to the top. The
    // gallery's bar is the nearest one right of its viewport.
    let (hit_pos, hit_size) = (hit.absolute_position(), hit.size());
    let right = hit_pos.x + hit_size.width;
    let track = i_slint_backend_testing::ElementQuery::from_root(&app)
        .match_id("FFScrollbar::track-area")
        .find_all()
        .into_iter()
        .filter(|t| {
            let c = element_center(t);
            c.x > right && c.y > hit_pos.y && c.y < hit_pos.y + hit_size.height
        })
        .min_by(|a, b| element_center(a).x.total_cmp(&element_center(b).x))
        .expect("the gallery scrollbar is mounted");
    let top =
        slint::LogicalPosition::new(element_center(&track).x, track.absolute_position().y + 1.0);
    for event in [
        WindowEvent::PointerMoved { position: top },
        WindowEvent::PointerPressed {
            position: top,
            button: PointerEventButton::Left,
        },
        WindowEvent::PointerReleased {
            position: top,
            button: PointerEventButton::Left,
        },
    ] {
        seen.extend(dispatch(event));
    }
    assert_eq!(controller.lock().unwrap().gallery_scroll_y(), 0.0);
    assert_eq!(app.get_gallery_scroll_y(), 0.0);

    controller
        .lock()
        .unwrap()
        .set_gallery_viewport_height(120.0);
    seen.extend(perform());

    assert_eq!(overlay_counters(), before, "scrolling touches no card");
    assert!(!seen.contains(&ShellEvent::OverlayChanged), "{seen:?}");
    assert!(seen.contains(&ShellEvent::GalleryScrolled), "{seen:?}");
    assert!(shown_pictures(&app) == pictures);

    // A live fuse on project 0: progress pushes convert nothing.
    assert!(controller.lock().unwrap().start_fuse());
    let mut saw_progress = false;
    let start = std::time::Instant::now();
    while controller.lock().unwrap().settle_job().is_none() {
        push();
        saw_progress |= app.get_project_cards().row_data(0).unwrap().fusing;
        assert_eq!(overlay_counters().1, before.1, "progress converts nothing");
        assert!(shown_pictures(&app) == pictures);
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "the fuse settles"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(saw_progress, "a push showed the running card");
    common::settle_thumbnails(&mut controller.lock().unwrap());
    push();
    let settled = shown_pictures(&app);
    let thumbnail = u64::from(
        focusforge_gui::overlay::THUMBNAIL_WIDTH * focusforge_gui::overlay::THUMBNAIL_HEIGHT * 4,
    );
    assert_eq!(overlay_counters().1 - before.1, thumbnail);
    assert!(
        settled[0] != pictures[0],
        "the fused card's picture changed"
    );
    assert!(
        settled[1..] == pictures[1..],
        "no other card's picture changed"
    );

    // Closing project 1 drops its card and its cached handle alone.
    let closed = controller.lock().unwrap().project_cards()[1]
        .picture
        .clone()
        .expect("an open project has a picture");
    assert!(
        std::sync::Arc::weak_count(&closed) > 0,
        "the handle is cached"
    );
    controller.lock().unwrap().close_project(1);
    push();
    let remaining = shown_pictures(&app);
    assert_eq!(remaining.len(), 2);
    assert!(remaining[0] == settled[0] && remaining[1] == settled[2]);
    assert_eq!(
        std::sync::Arc::weak_count(&closed),
        0,
        "its handle is released"
    );
}

#[test]
fn a_hidden_overlay_publishes_nothing_until_reopened() {
    use slint::Model;

    if ran_in_child("a_hidden_overlay_publishes_nothing_until_reopened") {
        return;
    }
    let recent_dir = tree("hidden_overlay_recent");
    let project = seal_as_given_project("hidden_overlay_recent_project");
    focusforge_gui::recent::save(
        &recent_dir,
        &[RecentEntry {
            name: "demo".to_string(),
            folder: project.parent().unwrap().to_path_buf(),
            path: project,
            opened_at: 0,
        }],
    )
    .unwrap();
    let (app, controller, _events, _installed) =
        overlay_window("hidden_overlay", Some(&recent_dir));
    let push = || {
        focusforge_gui::bridge::push_projects_overlay(&app, &controller.lock().unwrap());
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
    };
    controller.lock().unwrap().toggle_projects_overlay();
    push();
    controller.lock().unwrap().toggle_projects_overlay();
    push();
    assert!(!app.get_projects_overlay_open());
    let before = overlay_counters();

    controller.lock().unwrap().select_project(2);
    push();
    assert!(controller.lock().unwrap().start_fuse());
    let start = std::time::Instant::now();
    while controller.lock().unwrap().settle_job().is_none() {
        push();
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "the fuse settles"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    push();
    assert_eq!(
        overlay_counters(),
        before,
        "a hidden overlay publishes nothing"
    );

    controller.lock().unwrap().toggle_projects_overlay();
    push();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let controller = controller.lock().unwrap();
    let expected = controller.project_cards();
    let shown: Vec<_> = app.get_project_cards().iter().collect();
    assert_eq!(shown.len(), expected.len());
    for (card, want) in shown.iter().zip(&expected) {
        assert_eq!(card.current, want.current);
        assert_eq!(card.fusing, want.fusing);
        assert_eq!(card.fusing_fraction, want.fusing_fraction);
        assert_eq!(card.meta_text.as_str(), want.meta_text);
    }
    assert!(expected[2].current, "the selection made while hidden shows");
    let recent: Vec<_> = app
        .get_recent_rows()
        .iter()
        .map(|r| {
            (
                r.name.to_string(),
                r.folder.to_string(),
                r.date_text.to_string(),
            )
        })
        .collect();
    let want: Vec<_> = controller
        .recent_rows(now)
        .into_iter()
        .map(|r| (r.name, r.folder, r.date_text))
        .collect();
    assert!(!want.is_empty(), "the seeded project is recent");
    assert_eq!(recent, want);
}

/// Every rail entry is found by its own id as a labelled button; Save
/// follows `projects-save-enabled`, and Recent has one entry per row.
#[test]
fn rail_entries_are_labelled_buttons() {
    use i_slint_backend_testing::{AccessibleRole, ElementQuery};
    use slint::ComponentHandle;

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    app.set_projects_overlay_open(true);

    let find = |id: &str| {
        ElementQuery::from_root(&app)
            .match_id(format!("FFProjectsRail::{id}"))
            .find_all()
    };
    let only = |id: &str, label: &str| {
        let found = find(id);
        assert_eq!(found.len(), 1, "one {id}");
        assert_eq!(found[0].accessible_role(), Some(AccessibleRole::Button));
        assert_eq!(found[0].accessible_label().as_deref(), Some(label));
        found[0].accessible_enabled()
    };

    app.set_projects_save_enabled(false);
    assert_eq!(only("new-project", "New project"), Some(true));
    assert_eq!(only("open-project", "Open project…"), Some(true));
    assert_eq!(only("save-project", "Save project"), Some(false));
    assert_eq!(only("close-project", "Close project"), Some(true));
    assert!(find("recent-row").is_empty(), "no Recent rows, no entries");

    app.set_projects_save_enabled(true);
    assert_eq!(only("save-project", "Save project"), Some(true));

    let recent = |name: &str| focusforge_gui::RecentRowView {
        name: name.into(),
        ..Default::default()
    };
    app.set_recent_rows(
        std::rc::Rc::new(slint::VecModel::from(vec![recent("alpha"), recent("beta")])).into(),
    );
    let rows = find("recent-row");
    assert_eq!(rows.len(), 2);
    for (row, name) in rows.iter().zip(["alpha", "beta"]) {
        assert_eq!(row.accessible_role(), Some(AccessibleRole::Button));
        assert_eq!(row.accessible_label().as_deref(), Some(name));
        assert_eq!(row.accessible_enabled(), Some(true), "{name}");
    }
}

/// The bell stays reachable over the overview: a pointer press, or Tab to it
/// and Return, closes the overview and opens the center with the keyboard
/// inside it, and opening the overview again closes the center without
/// touching the unread count.
#[test]
fn bell_in_projects_opens_the_center_with_focus_inside() {
    use slint::ComponentHandle;
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    app.window()
        .dispatch_event(WindowEvent::WindowActiveChanged(true));

    let mut controller = controller_with_frames("bell_in_projects", 4);
    controller.set_align_algorithm("dummy-align");
    assert!(controller.start_fuse());
    for _ in 0..2 {
        assert!(
            controller
                .add_frames(&[common::fixture_frame("mantis-64", 4)])
                .is_err(),
            "a running job refuses an add"
        );
    }
    wait_for_settle(&mut controller, Duration::from_secs(60));
    let newest = controller.notifications().last().unwrap().id;
    let controller = Arc::new(Mutex::new(controller));
    let slot = Rc::new(RefCell::new(None));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        slot.clone(),
    );
    focusforge_gui::bridge::push_toast(&app, &controller, &slot);

    let event = |event: WindowEvent| {
        app.window().dispatch_event(event);
        slint::platform::update_timers_and_animations();
    };
    let key = |key: Key| {
        let text: slint::SharedString = key.into();
        event(WindowEvent::KeyPressed { text: text.clone() });
        event(WindowEvent::KeyReleased { text });
    };
    let settle_slide = || {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
        slint::platform::update_timers_and_animations();
    };
    let open_projects = || {
        controller.lock().unwrap().toggle_projects_overlay();
        focusforge_gui::bridge::push_projects_overlay(&app, &controller.lock().unwrap());
        focusforge_gui::bridge::push_toast(&app, &controller, &slot);
        settle_slide();
    };
    let bell_ringed = || {
        i_slint_backend_testing::ElementQuery::from_root(&app)
            .match_id("FFIconButton::ring")
            .find_all()
            .iter()
            .any(|ring| {
                let at = ring.absolute_position();
                (at.x, at.y) == (8.0, 820.0)
            })
    };
    let selection = || controller.lock().unwrap().notification_selection();
    let center_open = || controller.lock().unwrap().notification_center_open();
    let projects_open = || controller.lock().unwrap().projects_overlay_open();

    open_projects();
    assert!(app.get_projects_overlay_open() && !app.get_toast_open());
    assert_eq!(
        app.get_notifications_unread(),
        2,
        "the badge stays over Projects"
    );

    // Pointer.
    let position = slint::LogicalPosition::new(24.0, 836.0);
    event(WindowEvent::PointerMoved { position });
    event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
    settle_slide();
    assert!(!projects_open() && !app.get_projects_overlay_open());
    assert!(center_open() && app.get_notification_center_open());
    assert_eq!(app.get_notifications_unread(), 0);
    key(Key::DownArrow);
    assert_eq!(
        selection(),
        (Some(newest), false),
        "the keys reach the center"
    );

    // Keyboard.
    open_projects();
    assert!(!center_open(), "Projects closes the center");
    assert_eq!(
        app.get_notifications_unread(),
        0,
        "and changes no unread state"
    );
    let mut tabs = 0;
    while !bell_ringed() {
        assert!(tabs < 60, "Tab never reached the bell");
        key(Key::Tab);
        tabs += 1;
    }
    key(Key::Return);
    settle_slide();
    assert!(!projects_open() && center_open());
    let before = selection();
    key(Key::DownArrow);
    assert_ne!(selection(), before, "Down moves the center's selection");
}
