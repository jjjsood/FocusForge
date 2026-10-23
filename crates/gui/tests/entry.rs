// SPDX-License-Identifier: GPL-3.0-or-later

//! A folder becomes projects: the picker/drop/`--open`
//! entry path, driven through `AppController` with no window and no real
//! picker. Synthetic directory trees under `CARGO_TARGET_TMPDIR`, built
//! from copies of `fixtures/mantis-512/frames/*.tif` -- every tree differs
//! from the fixture's own single-directory shape in at least one way.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use focusforge_gui::entry::ImportRefused;
use focusforge_gui::job::{JPEG_EXTENSIONS, PNG_EXTENSIONS, TIFF_EXTENSIONS};
use focusforge_gui::viewport::PaneRect;
use focusforge_gui::{AppController, EntryError, OpenAs, OpenOutcome, ShellMode};
use slint::ComponentHandle;

use common::{copy_frame, tree};

fn base_pane() -> (PaneRect, PaneRect) {
    (
        PaneRect {
            x: 0.0,
            y: 0.0,
            width: 500.0,
            height: 500.0,
            scale_factor: 1.0,
        },
        PaneRect {
            x: 500.0,
            y: 0.0,
            width: 500.0,
            height: 500.0,
            scale_factor: 1.0,
        },
    )
}

// ---------------------------------------------------------------------
// The picker path: unambiguous / ambiguous folders, the one question.
// ---------------------------------------------------------------------

#[test]
fn unambiguous_folder_opens_one_project() {
    let root = tree("entry_unambiguous");
    copy_frame(&root, "00.tif", 0);
    copy_frame(&root, "01.tif", 1);

    let mut controller = AppController::new();
    let outcome = common::open_and_settle(&mut controller, &[root]);
    assert!(
        matches!(outcome, OpenOutcome::Opened(_)),
        "a single-directory tree with no ambiguity must open straight through"
    );
    let tabs = controller.project_tabs();
    assert_eq!(tabs.len(), 1);
    assert!(tabs[0].unsaved);
}

#[test]
fn ambiguous_folder_asks_once() {
    let root = tree("entry_ambiguous");
    copy_frame(&root.join("subject-a"), "00.tif", 0);
    copy_frame(&root.join("subject-b"), "00.tif", 1);

    let mut controller = AppController::new();
    match common::open_and_settle(&mut controller, &[root]) {
        OpenOutcome::Question(question, _proposal) => {
            assert_eq!(question.project_count, 2);
            assert_eq!(question.frame_count, 2);
        }
        OpenOutcome::Opened(_) => panic!("two directories must ask, not open straight through"),
        OpenOutcome::Mismatch(_) => panic!("opening a folder must never raise a mismatch"),
        OpenOutcome::Refused(err) => panic!("must not refuse: {err}"),
    }
    assert!(
        controller.project_tabs().is_empty(),
        "no tab opens until the question is answered"
    );
    assert!(controller.pending_question().is_some());

    controller.cancel_question();
    assert!(
        controller.pending_question().is_none(),
        "cancel clears the question's own vocabulary"
    );
    assert!(controller.project_tabs().is_empty());
}

#[test]
fn both_answers_open_what_they_say() {
    let one_root = tree("entry_both_answers_one");
    copy_frame(&one_root.join("subject-a"), "00.tif", 0);
    copy_frame(&one_root.join("subject-b"), "00.tif", 1);

    let mut one = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut one, &[one_root]),
        OpenOutcome::Question(..)
    ));
    common::answer_and_settle(&mut one, OpenAs::OneProject);
    assert_eq!(
        one.project_tabs().len(),
        1,
        "OneProject folds every group's frames into one project"
    );
    assert!(one.pending_question().is_none());

    let many_root = tree("entry_both_answers_many");
    copy_frame(&many_root.join("subject-a"), "00.tif", 0);
    copy_frame(&many_root.join("subject-b"), "00.tif", 1);

    let mut many = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut many, &[many_root]),
        OpenOutcome::Question(..)
    ));
    common::answer_and_settle(&mut many, OpenAs::NProjects);
    assert_eq!(
        many.project_tabs().len(),
        2,
        "NProjects opens each group as its own tab"
    );
}

// ---------------------------------------------------------------------
// The drop route: a folder matches the picker, several files are one open.
// ---------------------------------------------------------------------

#[test]
fn drop_of_a_folder_matches_the_picker() {
    let root = tree("entry_drop_folder");
    copy_frame(&root, "00.tif", 0);
    copy_frame(&root, "01.tif", 1);

    let mut picked = AppController::new();
    common::open_and_settle(&mut picked, std::slice::from_ref(&root));

    let mut dropped = AppController::new();
    // A dropped folder arrives as one unexpanded path -- the same call the
    // picker makes.
    common::open_and_settle(&mut dropped, &[root]);

    assert_eq!(
        picked.project_tabs(),
        dropped.project_tabs(),
        "the picker and a folder drop must produce the same project list"
    );
}

#[test]
fn drop_of_several_files_is_one_open() {
    let root = tree("entry_drop_files");
    copy_frame(&root, "00.tif", 0);
    copy_frame(&root, "01.tif", 1);
    copy_frame(&root, "02.tif", 2);

    let mut controller = AppController::new();
    let paths = vec![
        root.join("00.tif"),
        root.join("01.tif"),
        root.join("02.tif"),
    ];
    let outcome = common::open_and_settle(&mut controller, &paths);
    assert!(matches!(outcome, OpenOutcome::Opened(_)));
    assert_eq!(
        controller.project_tabs().len(),
        1,
        "three dropped files become one project of three frames, not three projects"
    );
}

/// The active tab's frame paths, in the Images panel's order.
fn active_frame_paths(controller: &AppController) -> Vec<PathBuf> {
    controller
        .images_panel()
        .rows
        .into_iter()
        .map(|row| row.path)
        .collect()
}

/// A tree holding `names`, each a copy of a mantis frame.
fn tree_of_frames(label: &str, names: &[&str]) -> PathBuf {
    let root = tree(label);
    for (index, name) in names.iter().enumerate() {
        copy_frame(&root, name, index);
    }
    root
}

#[test]
fn picked_images_open_one_new_tab_of_exactly_those_frames() {
    let root = tree_of_frames(
        "entry_picked_exact",
        &["00.tif", "01.tif", "02.tif", "03.tif", "04.tif"],
    );
    let picked = vec![
        root.join("00.tif"),
        root.join("02.tif"),
        root.join("04.tif"),
    ];

    let mut empty = AppController::new();
    let outcome = common::open_and_settle(&mut empty, &picked);
    assert!(matches!(outcome, OpenOutcome::Opened(_)));
    assert_eq!(empty.project_tabs().len(), 1, "one new tab");
    assert_eq!(
        active_frame_paths(&empty),
        picked,
        "exactly the picked files"
    );

    let mut busy = with_one_project("entry_picked_exact_first");
    let first = busy.project_tabs()[0].title.clone();
    common::open_and_settle(&mut busy, &picked);
    let tabs = busy.project_tabs();
    assert_eq!(tabs.len(), 2, "the pick opens a tab beside the open one");
    assert_eq!(tabs[0].title, first, "the earlier tab is unchanged");
    assert_eq!(active_frame_paths(&busy), picked);
}

#[test]
fn picked_images_order_matches_the_drop() {
    let root = tree_of_frames("entry_picked_order", &["a.tif", "b.tif", "c.tif", "d.tif"]);
    let by_name: Vec<PathBuf> = ["a.tif", "b.tif", "c.tif", "d.tif"]
        .iter()
        .map(|n| root.join(n))
        .collect();
    let reversed: Vec<PathBuf> = by_name.iter().rev().cloned().collect();
    let shuffled = vec![
        by_name[2].clone(),
        by_name[0].clone(),
        by_name[3].clone(),
        by_name[1].clone(),
    ];

    for given in [&reversed, &shuffled] {
        let mut picked = AppController::new();
        common::open_and_settle(&mut picked, given);
        assert_eq!(
            active_frame_paths(&picked),
            by_name,
            "picked: ascending file name"
        );

        let mut dropped = AppController::new();
        if let Some(handle) = dropped
            .drop_paths(given)
            .expect("an empty controller opens")
        {
            handle.join().expect("the load worker did not panic");
        }
        dropped.settle_open().expect("the drop started a load");
        assert_eq!(
            active_frame_paths(&dropped),
            by_name,
            "dropped: ascending file name"
        );
        assert_eq!(picked.project_tabs(), dropped.project_tabs());
    }
}

#[test]
fn a_picked_raw_set_opens_as_one_tab() {
    let root = tree("entry_picked_raw");
    let source = common::workspace_root().join("fixtures/blackmagic-micro-dng/frame.dng");
    std::fs::copy(&source, root.join("b.dng")).unwrap();
    std::fs::copy(&source, root.join("A.DNG")).unwrap();

    let mut controller = AppController::new();
    let outcome =
        common::open_and_settle(&mut controller, &[root.join("b.dng"), root.join("A.DNG")]);
    assert!(
        matches!(outcome, OpenOutcome::Opened(_)),
        "a RAW-only set must open"
    );
    assert_eq!(controller.project_tabs().len(), 1);
    assert_eq!(
        active_frame_paths(&controller),
        vec![root.join("A.DNG"), root.join("b.dng")]
    );
}

#[test]
fn an_upper_case_tif_extension_is_a_frame() {
    let root = tree_of_frames("entry_picked_upper", &["00.TIF", "01.TIF"]);
    let picked = vec![root.join("00.TIF"), root.join("01.TIF")];
    let mut controller = AppController::new();
    let outcome = common::open_and_settle(&mut controller, &picked);
    assert!(matches!(outcome, OpenOutcome::Opened(_)));
    assert_eq!(active_frame_paths(&controller), picked);
}

#[test]
fn picked_images_refusals_change_nothing() {
    let root = tree_of_frames("entry_picked_refused_a", &["00.tif", "01.tif"]);
    let other = tree_of_frames("entry_picked_refused_b", &["00.tif"]);
    let note = root.join("notes.txt");
    std::fs::write(&note, b"not a frame").unwrap();
    let raw = root.join("raw.dng");
    std::fs::copy(
        common::workspace_root().join("fixtures/blackmagic-micro-dng/frame.dng"),
        &raw,
    )
    .unwrap();

    let mut controller = with_one_project("entry_picked_refused_open");
    let tabs = controller.project_tabs();

    let mut refusals = Vec::new();
    for set in [
        vec![root.join("00.tif"), note.clone()],
        vec![root.join("00.tif"), other.join("00.tif")],
        vec![root.join("00.tif"), raw.clone()],
    ] {
        assert!(
            controller.open_paths(&set).is_none(),
            "{set:?} starts no load"
        );
        assert_eq!(controller.project_tabs(), tabs, "{set:?} changes nothing");
        assert_eq!(
            controller.refusal_title(),
            None,
            "the dialog keeps its own title"
        );
        refusals.push(
            controller
                .last_refusal()
                .expect("a refusal is raised")
                .to_string(),
        );
    }
    assert!(refusals[0].ends_with("not a folder of frames or a FocusForge project"));
    assert_eq!(
        refusals[1],
        "These files are in different folders. A stack's frames share one folder."
    );
    assert_eq!(
        refusals[2],
        "These files mix camera RAW and TIFF. A stack is one kind of file."
    );

    // An answer that arrives while a load is in flight opens nothing.
    controller.set_open_hold(Duration::from_millis(300));
    let first = controller
        .open_paths(&[root.join("00.tif")])
        .expect("the first pick starts a load");
    assert!(controller.open_paths(&[root.join("01.tif")]).is_none());
    first.join().unwrap();
    controller.settle_open().expect("the first load settles");
    assert_eq!(controller.project_tabs().len(), tabs.len() + 1);
}

// ---------------------------------------------------------------------
// The two refusals, and the not-an-error skip.
// ---------------------------------------------------------------------

/// The refusal body for a folder holding nothing the open path reads: it
/// names every accepted source family (checked against the extension
/// constants the open path scans with), names no PNG/JPEG, and leaves the
/// way out to the dialog's button.
fn assert_no_readable_frames_body(controller: &AppController, outcome: OpenOutcome) {
    match outcome {
        OpenOutcome::Refused(EntryError::NoReadableFrames { .. }) => {}
        OpenOutcome::Refused(other) => panic!("expected NoReadableFrames, got {other}"),
        _ => panic!("a folder without readable frames must be refused"),
    }
    let body = controller
        .last_refusal()
        .expect("a refusal message must be recorded");
    for extension in TIFF_EXTENSIONS {
        assert!(
            body.contains(&format!(".{extension}")),
            "the body must name .{extension}: {body}"
        );
    }
    assert!(
        body.contains("RAW"),
        "the body must name camera RAW: {body}"
    );
    for extension in PNG_EXTENSIONS.iter().chain(JPEG_EXTENSIONS) {
        assert!(
            !body.to_lowercase().contains(&format!(".{extension}")),
            "the body must not name .{extension}: {body}"
        );
    }
    assert!(!body.contains("PNG") && !body.contains("JPEG"), "{body}");
    assert!(
        !body.contains("Pick another folder"),
        "the button carries the offer, not the body: {body}"
    );
}

#[test]
fn empty_folder_is_refused_with_a_fix() {
    let root = tree("entry_empty_folder");
    let mut controller = AppController::new();
    let outcome = common::open_and_settle(&mut controller, &[root]);
    assert_no_readable_frames_body(&controller, outcome);
}

#[test]
fn png_and_jpeg_only_folder_is_refused() {
    let root = tree("entry_png_jpeg_folder");
    std::fs::write(root.join("a.png"), b"not decoded").unwrap();
    std::fs::write(root.join("b.jpg"), b"not decoded").unwrap();
    let mut controller = AppController::new();
    let outcome = common::open_and_settle(&mut controller, &[root]);
    assert_no_readable_frames_body(&controller, outcome);
}

#[test]
fn raw_only_folder_opens() {
    let root = tree("entry_raw_folder");
    std::fs::copy(
        common::workspace_root().join("fixtures/blackmagic-micro-dng/frame.dng"),
        root.join("frame.dng"),
    )
    .unwrap();
    let mut controller = AppController::new();
    let outcome = common::open_and_settle(&mut controller, &[root]);
    assert!(
        matches!(outcome, OpenOutcome::Opened(_)),
        "a folder holding only a camera RAW file must open"
    );
}

#[test]
#[cfg(unix)]
fn unreadable_folder_is_refused_with_its_read_error() {
    let Some(root) = common::readonly_dir("entry_unreadable_folder", 0o000, |_| {}) else {
        return;
    };
    // The exact same call the entry path makes, so the expected message
    // carries the OS's own real error text rather than a guessed one.
    let read_err = std::fs::read_dir(&*root).unwrap_err();
    let expected = format!("{0}: {0}: {read_err}", root.display());

    let mut controller = AppController::new();
    let path = root.to_path_buf();
    let outcome = common::open_and_settle(&mut controller, std::slice::from_ref(&path));

    match outcome {
        OpenOutcome::Refused(EntryError::Read { .. }) => {}
        OpenOutcome::Refused(other) => panic!("expected Read, got {other}"),
        _ => panic!("an unreadable folder must be refused"),
    }
    let refusal = controller
        .last_refusal()
        .expect("a refusal message must be recorded");
    assert_eq!(refusal, expected);
}

#[test]
fn a_path_that_is_neither_a_folder_nor_a_project_is_refused_by_name() {
    let dir = tree("entry_unreadable_path");
    let bad = dir.join("notes.txt");
    std::fs::write(&bad, b"neither a frame nor a project").unwrap();

    let mut controller = AppController::new();
    // `classify` refuses inline, before any worker starts -- no load to
    // wait for.
    assert!(controller.open_paths(std::slice::from_ref(&bad)).is_none());
    let refusal = controller
        .last_refusal()
        .expect("a refusal message must be recorded");
    assert_eq!(
        refusal,
        format!(
            "{}: not a folder of frames or a FocusForge project",
            bad.display()
        )
    );
}

#[test]
fn skipped_subfolders_are_reported_not_failed() {
    let root = tree("entry_skipped");
    copy_frame(&root.join("subject"), "00.tif", 0);
    std::fs::create_dir_all(root.join("thumbs")).unwrap();
    std::fs::write(root.join("thumbs").join("readme.txt"), b"not a frame").unwrap();

    let mut controller = AppController::new();
    let outcome = common::open_and_settle(&mut controller, &[root]);
    assert!(matches!(outcome, OpenOutcome::Opened(_)));
    assert_eq!(controller.project_tabs().len(), 1);
    let record = controller
        .notifications()
        .first()
        .expect("the skipped subfolder must be reported");
    assert_eq!(record.title, "Some images were skipped");
    assert!(record.message.contains("thumbs"), "{}", record.message);
    assert!(record.message.starts_with("1 subfolder skipped"));
    assert!(
        controller.last_refusal().is_none(),
        "a skipped subfolder is not a failure"
    );
}

#[test]
fn two_skipped_subfolders_make_one_record_naming_both_paths() {
    let root = tree("entry_two_skipped");
    copy_frame(&root.join("subject"), "00.tif", 0);
    for name in ["thumbs", "notes"] {
        std::fs::create_dir_all(root.join(name)).unwrap();
        std::fs::write(root.join(name).join("readme.txt"), b"not a frame").unwrap();
    }

    let mut controller = AppController::new();
    let outcome = common::open_and_settle(&mut controller, std::slice::from_ref(&root));
    assert!(matches!(outcome, OpenOutcome::Opened(_)));
    assert_eq!(controller.notifications().len(), 1, "one open, one record");
    let record = &controller.notifications()[0];
    assert_eq!(record.title, "Some images were skipped");
    assert!(record.message.starts_with("2 subfolders skipped"));
    for name in ["thumbs", "notes"] {
        let full = root.join(name).display().to_string();
        assert!(
            record.message.contains(&full),
            "{full} in {}",
            record.message
        );
    }
}

// ---------------------------------------------------------------------
// No grouping vocabulary, and the drop caption's two halves.
// ---------------------------------------------------------------------

#[test]
fn no_grouping_vocabulary_after_the_answer() {
    // Every `"..."` string literal, never the surrounding markup: a
    // component named `FFGroupingQuestion` or a property named
    // `question-open` is Rust/`.slint` vocabulary, not user-facing text,
    // and must not trip this scan, whose subject is user-facing strings
    // only.
    fn string_literals(contents: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut rest = contents;
        while let Some(start) = rest.find('"') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('"') else { break };
            out.push(&after[..end]);
            rest = &after[end + 1..];
        }
        out
    }

    for path in common::find_slint_files(&common::ui_dir()) {
        let contents = std::fs::read_to_string(&path).unwrap();
        for literal in string_literals(&contents) {
            assert!(
                !literal.to_lowercase().contains("group"),
                "{}: string literal {literal:?} contains \"group\" -- the shell has no grouping vocabulary",
                path.display()
            );
        }
    }

    let empty_refusal =
        common::open_and_settle(&mut AppController::new(), &[tree("entry_no_group_empty")]);
    if let OpenOutcome::Refused(err) = empty_refusal {
        assert!(!err.to_string().to_lowercase().contains("group"));
    } else {
        panic!("an empty folder must refuse");
    }

    let ambiguous_root = tree("entry_no_group_ambiguous");
    copy_frame(&ambiguous_root.join("subject-a"), "00.tif", 0);
    copy_frame(&ambiguous_root.join("subject-b"), "00.tif", 1);
    let mut controller = AppController::new();
    match common::open_and_settle(&mut controller, &[ambiguous_root]) {
        OpenOutcome::Question(question, _) => {
            let rendered = format!(
                "This folder reads as {} stacks of {} frames. Open as one project. Open as {} projects. Cancel",
                question.project_count, question.frame_count, question.project_count,
            );
            assert!(!rendered.to_lowercase().contains("group"));
        }
        _ => panic!("expected a question"),
    }
}

#[test]
fn drop_caption_names_the_drop_and_the_wayland_line() {
    let contents =
        std::fs::read_to_string(common::ui_dir().join("shell").join("empty_state.slint")).unwrap();
    assert!(
        contents.contains("Drop a folder or stack"),
        "the caption names what a drop accepts"
    );
    assert!(
        contents.contains("Dropping is unavailable on native Wayland"),
        "the caption says so where dropping does not work"
    );
    assert!(
        !contents.contains("Open folder… works everywhere"),
        "the long platform caveat is gone"
    );
}

// ---------------------------------------------------------------------
// The tab strip scrolls to every tab (nine projects at the 1200 minimum).
// ---------------------------------------------------------------------

fn init_backend() {
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

fn push_tabs(app: &focusforge_gui::AppWindow, controller: &AppController) {
    let tabs: Vec<focusforge_gui::ProjectTabView> = controller
        .project_tabs()
        .into_iter()
        .map(|t| focusforge_gui::ProjectTabView {
            title: t.title.into(),
            unsaved: t.unsaved,
            selected: t.selected,
            run_mark_icon: t.run_mark_icon.into(),
        })
        .collect();
    app.set_project_tabs(std::rc::Rc::new(slint::VecModel::from(tabs)).into());
    app.set_tab_scroll_x(controller.tab_scroll_x());
    app.set_tabs_overflow(controller.tabs_overflow());
    app.set_can_scroll_left(controller.can_scroll_left());
    app.set_can_scroll_right(controller.can_scroll_right());
    // Past `Theme.motion-fast` (150 ms): positions read next are the settled
    // scroll offset, not a frame of its animation.
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(200));
}

#[test]
fn tab_strip_scrolls_to_every_tab() {
    init_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1200, 760));

    let mut controller = AppController::new();
    let root = tree("entry_nine_tabs");
    for i in 0..9 {
        copy_frame(&root.join(format!("subject-{i}")), "00.tif", i % 8);
    }
    match common::open_and_settle(&mut controller, &[root]) {
        OpenOutcome::Question(..) => {
            common::answer_and_settle(&mut controller, OpenAs::NProjects);
        }
        OpenOutcome::Opened(_) => {}
        OpenOutcome::Mismatch(_) => panic!("opening a folder must never raise a mismatch"),
        OpenOutcome::Refused(err) => panic!("nine subject subdirectories must not refuse: {err}"),
    }
    assert_eq!(controller.project_tabs().len(), 9);
    controller.set_tab_region_width(app.get_tab_region_width());
    push_tabs(&app, &controller);

    let region = i_slint_backend_testing::ElementQuery::from_root(&app)
        .match_id("FFTabStrip::tabs-viewport")
        .find_first()
        .expect("no element matched id \"FFTabStrip::tabs-viewport\"");
    let region_pos = region.absolute_position();
    let region_size = region.size();

    // Element order alone cannot tell two tabs apart once some have
    // scrolled out of the clip -- every `FFTab` instance shares one
    // element id -- so each carries its own loop index back out via its
    // accessible label (`"tab-<i>"`, `ui/shell/tab_strip.slint`).
    let visible_tab_indices = |app: &focusforge_gui::AppWindow| -> Vec<usize> {
        i_slint_backend_testing::ElementQuery::from_root(app)
            .match_id("FFTabStrip::tab-instance")
            .find_all()
            .into_iter()
            .filter_map(|el| {
                let p = el.absolute_position();
                let s = el.size();
                let visible = p.x >= region_pos.x - 1.0
                    && p.x + s.width <= region_pos.x + region_size.width + 1.0;
                if !visible {
                    return None;
                }
                el.accessible_label()?.strip_prefix("tab-")?.parse().ok()
            })
            .collect()
    };

    let mut ever_visible = vec![false; 9];
    for _ in 0..12 {
        for i in visible_tab_indices(&app) {
            ever_visible[i] = true;
        }
        controller.scroll_tabs(1);
        push_tabs(&app, &controller);
    }
    assert!(
        ever_visible.iter().all(|v| *v),
        "every one of the nine tabs must become visible while scrolling right: {ever_visible:?}"
    );

    // Scroll back to the first tab.
    for _ in 0..12 {
        controller.scroll_tabs(-1);
    }
    push_tabs(&app, &controller);
    assert!(
        visible_tab_indices(&app).contains(&0),
        "scrolling all the way left must show the first tab again"
    );
}

/// A freshly opened project must not read as overflowing before the strip's
/// real width is ever pushed back from Slint. A `changed` handler fires
/// only for a *later* change, never for the width's initial value, so a
/// `tab_region_width` left at its untouched default would understate the
/// strip as narrower than it is: one 228 px tab would read as overflowing
/// a strip that was never actually measured, lighting both scroll arrows
/// over a single tab.
#[test]
fn a_single_tab_does_not_read_as_overflowing_before_any_resize() {
    let root = tree("entry_single_tab_no_resize");
    copy_frame(&root, "00.tif", 0);

    let mut controller = AppController::new();
    let outcome = common::open_and_settle(&mut controller, &[root]);
    assert!(matches!(outcome, OpenOutcome::Opened(_)));
    assert_eq!(controller.project_tabs().len(), 1);

    assert!(
        !controller.can_scroll_right(),
        "a single 228 px tab must not overflow the strip's real initial width"
    );
    assert!(
        !controller.can_scroll_left(),
        "scroll offset starts at zero, so left must never read as scrollable"
    );
}

/// An arrow click or a resize must push the new offset to the window.
#[test]
fn tab_scroll_changes_reach_the_sink_and_opening_scrolls_into_view() {
    use focusforge_gui::{ShellEvent, max_tab_scroll, tab_content_width, tab_viewport_width};
    use std::sync::{Arc, Mutex};

    let root = tree("entry_tab_scroll_events");
    for i in 0..10 {
        copy_frame(&root.join(format!("subject-{i}")), "00.tif", i % 8);
    }
    let mut controller = AppController::new();
    controller.set_tab_region_width(1254.0);
    if let OpenOutcome::Question(..) = common::open_and_settle(&mut controller, &[root]) {
        common::answer_and_settle(&mut controller, OpenAs::NProjects);
    }
    assert_eq!(controller.project_tabs().len(), 10);
    assert!(controller.tabs_overflow());
    // The first opened tab is active, so the strip starts at zero.
    assert_eq!(controller.tab_scroll_x(), 0.0);
    assert!(!controller.can_scroll_left() && controller.can_scroll_right());

    let events = Arc::new(Mutex::new(Vec::new()));
    controller.set_sink(Arc::new({
        let events = events.clone();
        move |e| events.lock().unwrap().push(e)
    }));

    controller.scroll_tabs(1);
    assert_eq!(controller.tab_scroll_x(), 228.0);
    assert!(controller.can_scroll_left() && controller.can_scroll_right());
    assert_eq!(*events.lock().unwrap(), [ShellEvent::TabScrollChanged]);

    controller.scroll_tabs(-5);
    assert_eq!(controller.tab_scroll_x(), 0.0);
    controller.scroll_tabs(-1);
    assert_eq!(
        events.lock().unwrap().len(),
        2,
        "a no-op step pushes nothing"
    );

    let content = tab_content_width(10);
    let max_at = |region: f32| max_tab_scroll(content, tab_viewport_width(10, region));
    controller.scroll_tabs(100);
    assert_eq!(controller.tab_scroll_x(), max_at(1254.0));
    assert!(!controller.can_scroll_right() && controller.can_scroll_left());

    // Wider: re-clamped so the last tab still meets the viewport's right edge.
    events.lock().unwrap().clear();
    controller.set_tab_region_width(1414.0);
    assert_eq!(controller.tab_scroll_x(), max_at(1414.0));
    assert_eq!(*events.lock().unwrap(), [ShellEvent::TabScrollChanged]);

    // Narrower again: the offset stays, the right arrow lights -- the
    // window must still be told.
    events.lock().unwrap().clear();
    let offset = controller.tab_scroll_x();
    controller.set_tab_region_width(1254.0);
    assert_eq!(controller.tab_scroll_x(), offset);
    assert!(controller.can_scroll_right());
    assert_eq!(*events.lock().unwrap(), [ShellEvent::TabScrollChanged]);

    // Selecting the last tab from zero brings it into view.
    controller.scroll_tabs(-100);
    controller.select_project(9);
    assert_eq!(controller.tab_scroll_x(), max_at(1254.0));

    // Closing the last tab at full-right scroll re-clamps against 9 tabs.
    controller.close_project(9);
    assert_eq!(
        controller.tab_scroll_x(),
        max_tab_scroll(tab_content_width(9), tab_viewport_width(9, 1254.0))
    );

    // A region wide enough for every tab plus "+" leaves overflow at zero.
    controller.set_tab_region_width(9.0 * 228.0 + 30.0);
    assert!(!controller.tabs_overflow());
    assert_eq!(controller.tab_scroll_x(), 0.0);
    assert!(!controller.can_scroll_left() && !controller.can_scroll_right());
}

// ---------------------------------------------------------------------
// Reopening a saved project through the same picker.
// ---------------------------------------------------------------------

/// Seals a minimal project into `dir`, returning its `.ffp` path.
fn seal_test_project(dir: &Path) -> PathBuf {
    use focusforge_core::io::{Rect, TiffSpec, TiffWriter};
    use focusforge_core::params::ParamValues;
    use focusforge_core::project::{Metrics, SealRequest, seal};
    use focusforge_core::zmap::{ZMapInfo, ZMapWriter};

    std::fs::create_dir_all(dir).unwrap();
    let composite = dir.join("composite.tif");
    let zmap = dir.join("zmap.tif");
    let full = Rect {
        x: 0,
        y: 0,
        width: 4,
        height: 4,
    };

    let mut writer = TiffWriter::create(
        &composite,
        TiffSpec {
            width: 4,
            height: 4,
            samples_per_pixel: 3,
            description: Some("staged".to_string()),
        },
    )
    .unwrap();
    writer.write_tile(full, &[0u16; 4 * 4 * 3]).unwrap();
    writer.finish().unwrap();

    let mut map = ZMapWriter::create(&zmap, ZMapInfo::new(4, 4, 1).unwrap()).unwrap();
    map.write_tile(full, &[0u16; 4 * 4]).unwrap();
    map.finish().unwrap();

    let project_path = dir.join("test.ffp");
    seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources: &[],
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        registration: focusforge_core::align::Registration::default(),
        composite_description: "staged".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("a minimal project must seal");
    project_path
}

#[test]
fn a_saved_project_reopens_through_the_same_picker() {
    let dir = tree("entry_reopen_project");
    let project_path = seal_test_project(&dir);

    let mut controller = AppController::new();
    let outcome = common::open_and_settle(&mut controller, &[project_path]);
    assert!(matches!(outcome, OpenOutcome::Opened(_)));
    let tabs = controller.project_tabs();
    assert_eq!(tabs.len(), 1);
    assert!(
        !tabs[0].unsaved,
        "a reopened project carries no unsaved chip"
    );
}

#[test]
fn a_file_that_is_not_a_project_is_refused_by_name() {
    let dir = tree("entry_not_a_project");
    let bad = dir.join("notes.ffp");
    std::fs::write(&bad, b"not a zip container").unwrap();

    let mut controller = AppController::new();
    match common::open_and_settle(&mut controller, std::slice::from_ref(&bad)) {
        OpenOutcome::Refused(EntryError::ProjectRefused { path, .. }) => {
            assert_eq!(path, bad);
        }
        _ => panic!("a non-project .ffp must be refused as ProjectRefused"),
    };
    let refusal = controller.last_refusal().unwrap();
    assert_eq!(refusal, "This file is not a FocusForge project.");
}

// ---------------------------------------------------------------------
// Each tab keeps its own viewport.
// ---------------------------------------------------------------------

#[test]
fn each_tab_keeps_its_own_viewport() {
    let root_a = tree("entry_viewport_a");
    copy_frame(&root_a, "00.tif", 0);
    let root_b = tree("entry_viewport_b");
    copy_frame(&root_b, "00.tif", 1);

    let mut controller = AppController::new();
    let (left, right) = base_pane();

    common::open_and_settle(&mut controller, &[root_a]);
    controller.set_pane_geometry(left, right);
    controller.zoom_in();
    let readout_a = controller.zoom_readout();

    common::open_and_settle(&mut controller, &[root_b]);
    controller.set_pane_geometry(left, right);
    let readout_b_fresh = controller.zoom_readout();
    assert_ne!(
        readout_a, readout_b_fresh,
        "a newly opened tab must not inherit another tab's zoom"
    );

    controller.select_project(0);
    controller.set_pane_geometry(left, right);
    assert_eq!(
        controller.zoom_readout(),
        readout_a,
        "switching back must restore that tab's own zoom, not reset or leak it"
    );
}

// ---------------------------------------------------------------------
// "+" is a selectable pseudo-tab.
// ---------------------------------------------------------------------

/// A controller with one project already open from `name`'s one-frame tree.
fn with_one_project(name: &str) -> AppController {
    let root = tree(name);
    copy_frame(&root, "00.tif", 0);
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        OpenOutcome::Opened(_)
    ));
    controller
}

#[test]
fn plus_selects_the_new_project_placeholder() {
    let mut controller = with_one_project("entry_plus_selects");
    controller.set_tab_region_width(1000.0);
    assert!(!controller.plus_selected());
    let scroll = controller.tab_scroll_x();

    controller.select_plus();

    let tabs = controller.project_tabs();
    assert_eq!(tabs.len(), 1, "+ adds no tab");
    assert!(tabs.iter().all(|t| !t.selected));
    assert!(controller.plus_selected());
    assert_eq!(controller.active_project(), None);
    assert_eq!(controller.frame_count(), None);
    assert_eq!(controller.tab_scroll_x(), scroll, "+ does not scroll");
    let (left, right) = base_pane();
    controller.set_pane_geometry(left, right);
    assert_eq!(controller.zoom_readout(), "");
    assert!(controller.frame_draw().is_none());
}

#[test]
fn zero_tabs_selects_plus() {
    let controller = AppController::new();
    assert!(controller.project_tabs().is_empty());
    assert!(controller.plus_selected());
    assert_eq!(controller.active_project(), None);
}

#[test]
fn opening_a_folder_from_plus_appends_and_activates_the_first() {
    let mut controller = with_one_project("entry_plus_open_first");
    controller.set_tab_region_width(400.0);
    let first_title = controller.project_tabs()[0].title.clone();
    controller.select_plus();

    let root = tree("entry_plus_open");
    copy_frame(&root, "00.tif", 1);
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        OpenOutcome::Opened(_)
    ));

    let tabs = controller.project_tabs();
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs[0].title, first_title);
    assert!(tabs[1].unsaved && tabs[1].selected);
    assert!(!controller.plus_selected());
    assert_eq!(controller.active_project(), Some(1));
    assert_eq!(
        controller.tab_scroll_x(),
        focusforge_gui::tab_scroll_into_view(
            0.0,
            1,
            2,
            focusforge_gui::tab_viewport_width(2, 400.0)
        ),
        "the new tab is scrolled into view"
    );
}

#[test]
fn a_multi_stack_open_from_plus_appends_all_in_order() {
    let mut controller = with_one_project("entry_plus_multi_first");
    controller.select_plus();

    let root = tree("entry_plus_multi");
    copy_frame(&root.join("subject-a"), "00.tif", 0);
    copy_frame(&root.join("subject-b"), "00.tif", 1);
    copy_frame(&root.join("subject-c"), "00.tif", 2);
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        OpenOutcome::Question(..)
    ));
    assert!(
        controller.plus_selected(),
        "+ stays selected while the question is open"
    );
    common::answer_and_settle(&mut controller, OpenAs::NProjects);

    let tabs = controller.project_tabs();
    assert_eq!(tabs.len(), 4, "1 project + 3 stacks appended");
    assert!(tabs[1..].iter().all(|t| t.unsaved));
    let titles: Vec<&str> = tabs[1..].iter().map(|t| t.title.as_str()).collect();
    let mut sorted = titles.clone();
    sorted.sort();
    assert_eq!(titles, sorted, "stacks append in the proposal's order");
    assert!(tabs[1].selected);
    assert_eq!(controller.active_project(), Some(1));
    assert!(!controller.plus_selected());
}

#[test]
fn selecting_a_tab_deselects_plus() {
    let mut controller = with_one_project("entry_plus_deselect");
    controller.select_plus();
    controller.select_project(0);
    assert!(!controller.plus_selected());
    assert_eq!(controller.active_project(), Some(0));
    assert!(controller.project_tabs()[0].selected);
}

#[test]
fn closing_the_last_tab_selects_plus() {
    use focusforge_gui::ShellEvent;
    use std::sync::{Arc, Mutex};

    let mut controller = with_one_project("entry_close_last");
    let events = Arc::new(Mutex::new(Vec::new()));
    controller.set_sink(Arc::new({
        let events = events.clone();
        move |e| events.lock().unwrap().push(e)
    }));

    controller.close_project(0);
    assert!(controller.project_tabs().is_empty());
    assert!(controller.plus_selected());
    assert_eq!(controller.active_project(), None);
    assert!(controller.pending_question().is_none());
    assert_eq!(*events.lock().unwrap(), [ShellEvent::ProjectsChanged]);
}

/// Four tabs (`with_one_project` plus three appended stacks) for the close
/// tests below.
fn with_four_projects(name: &str) -> focusforge_gui::AppController {
    let mut controller = with_one_project(&format!("{name}_first"));
    let root = tree(name);
    copy_frame(&root.join("subject-a"), "00.tif", 0);
    copy_frame(&root.join("subject-b"), "00.tif", 1);
    copy_frame(&root.join("subject-c"), "00.tif", 2);
    common::open_and_settle(&mut controller, &[root]);
    common::answer_and_settle(&mut controller, OpenAs::NProjects);
    assert_eq!(controller.project_tabs().len(), 4);
    controller
}

#[test]
fn closing_a_tab_after_the_active_one_keeps_the_active_tab() {
    let mut controller = with_four_projects("entry_close_after");
    controller.select_project(0);
    let active_title = controller.project_tabs()[0].title.clone();

    controller.close_project(2);
    assert_eq!(controller.active_project(), Some(0));
    assert_eq!(controller.project_tabs()[0].title, active_title);
}

#[test]
fn closing_a_tab_before_the_active_one_keeps_the_active_tab() {
    let mut controller = with_four_projects("entry_close_before");
    controller.select_project(2);
    let active_title = controller.project_tabs()[2].title.clone();

    controller.close_project(0);
    assert_eq!(controller.active_project(), Some(1));
    assert_eq!(controller.project_tabs()[1].title, active_title);
    assert!(controller.project_tabs()[1].selected);
}

#[test]
fn closing_a_tab_while_plus_is_selected_keeps_plus_selected() {
    let mut controller = with_four_projects("entry_close_plus");
    controller.select_plus();

    controller.close_project(1);
    assert!(controller.plus_selected());
    assert_eq!(controller.active_project(), None);
}

/// Close requests are debounced by 300 ms.
#[test]
fn a_second_close_inside_the_debounce_window_is_ignored() {
    let mut controller = with_four_projects("entry_close_debounce_inside");
    let t0 = std::time::Instant::now();

    controller.close_project_at(1, t0);
    controller.close_project_at(
        1,
        t0 + AppController::CLOSE_DEBOUNCE - std::time::Duration::from_millis(1),
    );
    assert_eq!(controller.project_tabs().len(), 3);
}

#[test]
fn a_close_after_the_debounce_window_closes() {
    let mut controller = with_four_projects("entry_close_debounce_after");
    let t0 = std::time::Instant::now();

    controller.close_project_at(1, t0);
    controller.close_project_at(1, t0 + AppController::CLOSE_DEBOUNCE);
    assert_eq!(controller.project_tabs().len(), 2);
    assert_eq!(AppController::CLOSE_DEBOUNCE.as_millis(), 300);
}

/// The rendered strip: "+" carries the selected marker exactly while it is
/// selected, and the canvas shows the empty state with it.
#[test]
fn plus_shows_the_selected_marker_only_when_selected() {
    init_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));

    let marker_height = |app: &focusforge_gui::AppWindow| {
        i_slint_backend_testing::ElementQuery::from_root(app)
            .match_id("FFTabStrip::plus-button")
            .find_first()
            .expect("the strip carries +")
            .query_descendants()
            .match_id("FFStripBox::marker")
            .find_first()
            .expect("+ carries the strip box marker")
            .size()
            .height
    };
    let push = |app: &focusforge_gui::AppWindow, controller: &AppController| {
        push_tabs(app, controller);
        // `bridge::push_projects`/`repaint`'s own reads.
        app.set_plus_selected(controller.plus_selected());
        app.set_show_empty_state(controller.active_project().is_none());
        app.set_project_loaded(controller.active_project().is_some());
    };

    let controller = AppController::new();
    push(&app, &controller);
    assert!(app.get_plus_selected() && app.get_show_empty_state());
    assert_eq!(marker_height(&app), 2.0, "zero tabs: + is selected");

    let mut controller_loaded = with_one_project("entry_render_plus");
    push(&app, &controller_loaded);
    assert!(!app.get_show_empty_state() && app.get_project_loaded());
    assert_eq!(marker_height(&app), 0.0, "a project tab is active");

    controller_loaded.select_plus();
    push(&app, &controller_loaded);
    assert_eq!(marker_height(&app), 2.0);
    assert!(app.get_show_empty_state() && !app.get_project_loaded());
    let empty = i_slint_backend_testing::ElementQuery::from_root(&app)
        .match_id("FFCanvas::empty-state")
        .find_first()
        .expect("the canvas carries the empty state");
    assert!(empty.size().width > 0.0);
}

/// `project-loaded` (the dock's own gate for the tab row, the settings panel
/// and the "No project open." note) must flip the moment "+" or a project
/// tab is selected, with no canvas gesture in between. Exercises the real
/// `push_projects`, not a hand-rolled copy of what it should do.
#[test]
fn selecting_plus_and_back_flips_project_loaded_without_a_canvas_gesture() {
    use std::sync::{Arc, Mutex};

    init_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));

    let controller = Arc::new(Mutex::new(with_one_project(
        "entry_project_loaded_no_gesture",
    )));
    focusforge_gui::bridge::push_projects(&app, &controller);
    assert!(app.get_project_loaded(), "a project is active");

    controller.lock().unwrap().select_plus();
    focusforge_gui::bridge::push_projects(&app, &controller);
    assert!(
        !app.get_project_loaded(),
        "\"+\" holds no project, with no gesture in between"
    );

    controller.lock().unwrap().select_project(0);
    focusforge_gui::bridge::push_projects(&app, &controller);
    assert!(
        app.get_project_loaded(),
        "back on the tab, again with no gesture in between"
    );
}

/// Answering the grouping question must close its dialog: `push_projects`
/// propagates the controller's cleared `pending_question` to Slint's
/// `question_open` -- exercises the real `bridge::push_projects`, not the
/// controller alone.
#[test]
fn answering_the_question_closes_its_dialog() {
    use std::sync::{Arc, Mutex};

    init_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));

    let root = tree("entry_question_dialog_closes");
    copy_frame(&root.join("subject-a"), "00.tif", 0);
    copy_frame(&root.join("subject-b"), "00.tif", 1);

    let controller = Arc::new(Mutex::new(AppController::new()));
    let outcome = common::open_and_settle(&mut controller.lock().unwrap(), &[root]);
    assert!(matches!(outcome, OpenOutcome::Question(..)));
    app.set_question_open(true);
    focusforge_gui::bridge::push_projects(&app, &controller);
    assert!(
        app.get_question_open(),
        "still pending -- push_projects must not close a real question"
    );

    controller
        .lock()
        .unwrap()
        .answer_question(OpenAs::OneProject);
    focusforge_gui::bridge::push_projects(&app, &controller);
    assert!(
        !app.get_question_open(),
        "answered -- the dialog must close on the very next push_projects"
    );
}

/// Opening a folder while a loaded tab is active leaves it untouched and
/// appends the opened stack(s), the first one active and scrolled into
/// view -- `AppController::open_paths` directly, not the drop route
/// (`AppController::drop_paths`, exercised by the `a_drop_*` tests below).
#[test]
fn opening_a_folder_onto_a_loaded_tab_appends_new_tabs() {
    let mut controller = with_one_project("entry_drop_loaded_first");
    controller.set_tab_region_width(400.0);
    let first_title = controller.project_tabs()[0].title.clone();

    let root = tree("entry_drop_loaded");
    for i in 0..3 {
        copy_frame(&root.join(format!("subject-{i}")), "00.tif", i);
    }
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        OpenOutcome::Question(..)
    ));
    common::answer_and_settle(&mut controller, OpenAs::NProjects);

    let tabs = controller.project_tabs();
    assert_eq!(tabs.len(), 4);
    assert_eq!(tabs[0].title, first_title, "the loaded tab is untouched");
    assert!(tabs[1..].iter().all(|t| t.unsaved));
    assert_eq!(controller.active_project(), Some(1));
    assert_eq!(
        controller.tab_scroll_x(),
        focusforge_gui::tab_scroll_into_view(
            0.0,
            1,
            4,
            focusforge_gui::tab_viewport_width(4, 400.0)
        ),
        "the first new tab is scrolled into view"
    );
    assert!(controller.tab_scroll_x() > 0.0);
}

#[test]
fn a_drop_while_plus_is_selected_appends() {
    let mut controller = with_one_project("entry_drop_plus_first");
    controller.select_plus();

    let root = tree("entry_drop_plus");
    copy_frame(&root, "00.tif", 1);
    copy_frame(&root, "01.tif", 2);
    let dropped = vec![root.join("00.tif"), root.join("01.tif")];
    assert!(matches!(
        common::open_and_settle(&mut controller, &dropped),
        OpenOutcome::Opened(_)
    ));

    let tabs = controller.project_tabs();
    assert_eq!(tabs.len(), 2, "the dropped frames append one tab");
    assert!(!tabs[0].selected);
    assert!(tabs[1].unsaved && tabs[1].selected);
    assert_eq!(controller.active_project(), Some(1));
    assert!(!controller.plus_selected());
}

// ---------------------------------------------------------------------
// The drop route itself (`AppController::drop_paths`): additive into an
// already-open tab, opening outright for the shapes that never were
// additive, and refused when adding is blocked.
// ---------------------------------------------------------------------

/// [`with_one_project`], fused once into one held checkpoint, with its
/// scratch root set and an always-converging align algorithm -- what a
/// mid-run or outside-Stacking drop test needs to reach that state.
fn with_fused_project(name: &str) -> AppController {
    let root = tree(name);
    copy_frame(&root, "00.tif", 0);
    copy_frame(&root, "01.tif", 1);
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        OpenOutcome::Opened(_)
    ));
    controller.set_scratch_root_for_test(tree(&format!("{name}_scratch")));
    controller.set_align_algorithm("dummy-align");
    controller
}

#[test]
fn a_drop_of_a_folder_with_a_frames_subfolder_adds_to_the_active_tab() {
    let mut controller = with_one_project("entry_drop_frames_subfolder_first");
    let before_tabs = controller.project_tabs().len();
    let before_frames = controller.images_panel().rows.len();

    let root = tree("entry_drop_frames_subfolder");
    copy_frame(&root.join("frames"), "00.tif", 2);
    copy_frame(&root.join("frames"), "01.tif", 3);

    common::add_and_settle(&mut controller, &[root]).expect("an add onto an active tab");

    assert_eq!(
        controller.project_tabs().len(),
        before_tabs,
        "no new tab opens"
    );
    assert_eq!(controller.images_panel().rows.len(), before_frames + 2);
}

#[test]
fn a_multi_file_drop_spanning_two_parent_directories_adds_to_the_active_tab() {
    let mut controller = with_one_project("entry_drop_multi_parent_first");
    let before_frames = controller.images_panel().rows.len();

    let dir_a = tree("entry_drop_multi_parent_a");
    let dir_b = tree("entry_drop_multi_parent_b");
    copy_frame(&dir_a, "00.tif", 4);
    copy_frame(&dir_b, "00.tif", 5);
    let dropped = vec![dir_a.join("00.tif"), dir_b.join("00.tif")];

    common::add_and_settle(&mut controller, &dropped).expect("an add onto an active tab");

    assert_eq!(controller.project_tabs().len(), 1, "no new tab opens");
    assert_eq!(controller.images_panel().rows.len(), before_frames + 2);
}

#[test]
fn a_drop_of_a_lone_ffp_onto_an_active_tab_still_opens_its_own_tab() {
    let mut controller = with_one_project("entry_drop_ffp_first");
    let project_path = seal_test_project(&tree("entry_drop_ffp"));

    let handle = controller
        .drop_paths(&[project_path])
        .expect("a lone project drop must not refuse");
    if let Some(handle) = handle {
        handle.join().expect("the load worker did not panic");
    }
    let outcome = controller
        .settle_open()
        .expect("a load was started, so one outcome is parked");
    assert!(matches!(outcome, OpenOutcome::Opened(_)));
    assert_eq!(
        controller.project_tabs().len(),
        2,
        "the dropped project opens its own tab rather than adding to the active one"
    );
}

#[test]
fn a_drop_with_the_plus_tab_selected_still_opens_through_drop_paths() {
    let mut controller = with_one_project("entry_drop_plus_route_first");
    controller.select_plus();

    let root = tree("entry_drop_plus_route");
    copy_frame(&root, "00.tif", 6);
    copy_frame(&root, "01.tif", 7);
    let dropped = vec![root.join("00.tif"), root.join("01.tif")];

    let handle = controller
        .drop_paths(&dropped)
        .expect("a plus-selected drop must not refuse");
    if let Some(handle) = handle {
        handle.join().expect("the load worker did not panic");
    }
    let outcome = controller
        .settle_open()
        .expect("a load was started, so one outcome is parked");
    assert!(matches!(outcome, OpenOutcome::Opened(_)));

    let tabs = controller.project_tabs();
    assert_eq!(tabs.len(), 2, "the dropped frames append one tab");
    assert!(tabs[1].selected);
}

#[test]
fn a_drop_outside_stacking_mode_is_refused_and_changes_nothing() {
    let mut controller = with_fused_project("entry_drop_outside_stacking");
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120));
    assert!(
        controller.set_mode(ShellMode::Editor),
        "a fused tab enters editor mode"
    );
    let before_frames = controller.frame_count();

    let root = tree("entry_drop_outside_stacking_extra");
    copy_frame(&root, "00.tif", 0);
    let result = controller.drop_paths(&[root.join("00.tif")]);
    assert!(matches!(result, Err(ImportRefused::NotStacking)));
    assert_eq!(controller.project_tabs().len(), 1);
    assert_eq!(controller.frame_count(), before_frames);
}

#[test]
fn a_drop_while_this_tabs_job_is_running_is_refused_and_changes_nothing() {
    let mut controller = with_fused_project("entry_drop_mid_run");
    assert!(controller.start_fuse());
    assert!(controller.job_running());
    let before_frames = controller.frame_count();

    let root = tree("entry_drop_mid_run_extra");
    copy_frame(&root, "00.tif", 1);
    let result = controller.drop_paths(&[root.join("00.tif")]);
    assert!(matches!(result, Err(ImportRefused::JobRunning)));
    assert_eq!(controller.frame_count(), before_frames);

    common::wait_for_settle(&mut controller, Duration::from_secs(120));
}

#[test]
fn a_second_drop_while_the_first_scan_is_still_being_checked_is_refused() {
    let mut controller = with_one_project("entry_drop_still_checking_first");
    let before_frames = controller.frame_count();

    let root_a = tree("entry_drop_still_checking_a");
    copy_frame(&root_a, "00.tif", 2);
    let handle = controller
        .drop_paths(&[root_a.join("00.tif")])
        .expect("the first drop must not refuse");
    let handle = handle.expect("adding a real frame spawns a worker");

    let root_b = tree("entry_drop_still_checking_b");
    copy_frame(&root_b, "00.tif", 3);
    let second = controller.drop_paths(&[root_b.join("00.tif")]);
    assert!(matches!(second, Err(ImportRefused::StillChecking)));
    assert_eq!(controller.frame_count(), before_frames);

    handle.join().expect("the import worker did not panic");
    controller.settle_import();
}

#[test]
fn a_drop_of_neither_a_folder_nor_a_frame_is_refused_and_changes_nothing() {
    let mut controller = with_one_project("entry_drop_non_tiff");
    let dir = tree("entry_drop_non_tiff_extra");
    let bad = dir.join("notes.txt");
    std::fs::write(&bad, b"not a frame").unwrap();
    let before_frames = controller.frame_count();

    let result = controller.drop_paths(&[bad]);
    assert!(matches!(result, Err(ImportRefused::NoFrames { .. })));
    assert_eq!(controller.frame_count(), before_frames);
}

// ---------------------------------------------------------------------
// One double-click on a tab's close glyph closes one tab.
// ---------------------------------------------------------------------

/// A double-click on a tab's `×`, as real pointer events through the real
/// `bridge::install` wiring: the first click closes the tab, the next tab
/// slides under the pointer, and the second click must not close that one
/// too -- close requests are debounced by 300 ms.
///
/// The bridge's own sink needs `slint::invoke_from_event_loop`, which
/// `init_no_event_loop` disables, so the sink is swapped for a queue drained
/// after every dispatched event -- the order the running event loop
/// performs a pushed `ShellEvent::ProjectsChanged` in.
#[test]
fn a_double_click_on_a_close_glyph_closes_one_tab() {
    use focusforge_gui::ShellEvent;
    use slint::platform::{PointerEventButton, WindowEvent};
    use std::sync::{Arc, Mutex};

    for (case, close, active) in [(0, 1, 0), (1, 0, 0), (2, 1, 1), (3, 3, 3)] {
        init_backend();
        let app = focusforge_gui::AppWindow::new().unwrap();
        app.window().set_size(slint::PhysicalSize::new(1440, 900));

        let mut four = with_four_projects(&format!("entry_double_click_close_{case}"));
        four.select_project(active);
        let controller = Arc::new(Mutex::new(four));
        let canvas = std::rc::Rc::new(std::cell::RefCell::new(
            focusforge_gui::bridge::Canvas::default(),
        ));
        focusforge_gui::bridge::install(
            &app,
            controller.clone(),
            canvas,
            std::rc::Rc::new(std::cell::RefCell::new(None)),
        );
        let queued = Arc::new(Mutex::new(Vec::new()));
        controller.lock().unwrap().set_sink(Arc::new({
            let queued = queued.clone();
            move |e| queued.lock().unwrap().push(e)
        }));
        // Never hold the controller lock while the window runs: a property
        // push re-enters the bridge's own callbacks, which lock it too.
        let push = || {
            let (tabs, scroll, overflow, left, right) = {
                let c = controller.lock().unwrap();
                let tabs: Vec<focusforge_gui::ProjectTabView> = c
                    .project_tabs()
                    .into_iter()
                    .map(|t| focusforge_gui::ProjectTabView {
                        title: t.title.into(),
                        unsaved: t.unsaved,
                        selected: t.selected,
                        run_mark_icon: t.run_mark_icon.into(),
                    })
                    .collect();
                let flags = (c.tab_scroll_x(), c.tabs_overflow());
                (
                    tabs,
                    flags.0,
                    flags.1,
                    c.can_scroll_left(),
                    c.can_scroll_right(),
                )
            };
            app.set_project_tabs(std::rc::Rc::new(slint::VecModel::from(tabs)).into());
            app.set_tab_scroll_x(scroll);
            app.set_tabs_overflow(overflow);
            app.set_can_scroll_left(left);
            app.set_can_scroll_right(right);
        };
        push();
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(200));

        // Drained after every event, never mocking time in between: the
        // strip is mid-animation exactly as in the live window.
        let dispatch = |event: WindowEvent| {
            app.window().dispatch_event(event);
            let events = std::mem::take(&mut *queued.lock().unwrap());
            if events.contains(&ShellEvent::ProjectsChanged) {
                push();
            }
        };

        let close_area = i_slint_backend_testing::ElementQuery::from_root(&app)
            .match_id("FFTabStrip::tab-instance")
            .find_all()
            .into_iter()
            .find(|el| el.accessible_label().as_deref() == Some(&format!("tab-{close}")))
            .expect("the tab to close is rendered")
            .query_descendants()
            .match_id("FFTab::close-area")
            .find_first()
            .expect("every tab carries a close glyph");
        let (pos, size) = (close_area.absolute_position(), close_area.size());
        let position =
            slint::LogicalPosition::new(pos.x + size.width / 2.0, pos.y + size.height / 2.0);
        let button = PointerEventButton::Left;

        dispatch(WindowEvent::PointerMoved { position });
        for _ in 0..2 {
            dispatch(WindowEvent::PointerPressed { position, button });
            dispatch(WindowEvent::PointerReleased { position, button });
        }

        assert_eq!(
            controller.lock().unwrap().project_tabs().len(),
            3,
            "case {case}: closing tab {close} with tab {active} active -- one \
             double-click must close exactly one tab"
        );
    }
}
