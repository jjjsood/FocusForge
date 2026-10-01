// SPDX-License-Identifier: GPL-3.0-or-later

//! The `Images` dock panel, driven through `AppController` -- fast, no
//! window: the model over `fixtures/mantis-512` and small synthetic timed
//! frame sets, mirroring `tests/job.rs`'s own fixture-copy shape.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use focusforge_core::io::test_frames::write_frame_with_capture_time;
use focusforge_gui::images::{DefaultButton, FrameSort, ImageRow, ImagesModel, ROW_HEIGHT};
use focusforge_gui::input_map::PointerKind;
use focusforge_gui::job;
use focusforge_gui::{AppController, OpenAs, OpenOutcome, ShellMode};

use common::{controller_with_frames, copy_frame, tree};

fn write_timed(dir: &Path, name: &str, when: Option<&str>) -> PathBuf {
    let path = dir.join(name);
    write_frame_with_capture_time(&path, 4, 4, when).expect("a synthetic frame must write");
    path
}

/// A controller with one unsaved project open over `count` tiny synthetic
/// frames, each carrying the same real capture time -- fast, and real enough
/// for `TiffStack`/grouping to open (`crate::io::test_frames`'s own
/// contract), unlike the committed fixture, which carries no capture time
/// at all.
fn controller_with_timed_frames(name: &str, count: usize) -> AppController {
    let root = tree(&format!("{name}_frames"));
    for i in 0..count {
        write_timed(&root, &format!("{i:02}.tif"), Some("2026:08:29 10:00:00"));
    }
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        OpenOutcome::Opened(_)
    ));
    controller
}

fn wait_for_settle(controller: &mut AppController, timeout: Duration) {
    let start = Instant::now();
    loop {
        if controller.settle_job().is_some() {
            break;
        }
        if !job::is_live(&controller.job_view().state) {
            break;
        }
        assert!(
            start.elapsed() < timeout,
            "job did not settle within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // A commit now auto-enters Editor on the viewed tab; this file's tests
    // are about Stacking-mode frame curation, so back out to a known
    // baseline (`controller_in_editor_mode` still enters Editor itself,
    // right after this call, for the few tests that want it).
    controller.set_mode(ShellMode::Stacking);
}

/// Saves the active, never-saved tab to the path the save dialog proposes,
/// and returns that path.
fn save_to_proposed(controller: &mut AppController) -> PathBuf {
    let mut target = None;
    assert!(controller.save_active_project(|proposed| {
        target = Some(proposed.to_path_buf());
        target.clone()
    }));
    target.expect("a never-saved tab asks where to save")
}

/// Seals a project directly (no real fuse) over `sources`, returning its
/// `.ffp` path -- reuses `entry.rs`'s sealing helper, widened to take real
/// sources so the reopened list has rows to show.
fn seal_project_with_sources(dir: &Path, sources: &[PathBuf]) -> PathBuf {
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

    let mut map = ZMapWriter::create(
        &zmap,
        ZMapInfo::new(4, 4, sources.len().max(1) as u32).unwrap(),
    )
    .unwrap();
    map.write_tile(full, &[0u16; 4 * 4]).unwrap();
    map.finish().unwrap();

    let project_path = dir.join("test.ffp");
    seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources,
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        registration: focusforge_core::align::Registration::default(),
        composite_description: "staged".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("a project with real sources must seal");
    project_path
}

// ---------------------------------------------------------------------
// The list itself.
// ---------------------------------------------------------------------

#[test]
fn list_shows_every_frame() {
    let controller = controller_with_timed_frames("list_shows_every_frame", 4);
    let panel = controller.images_panel();
    assert_eq!(panel.rows.len(), 4);
    for (i, row) in panel.rows.iter().enumerate() {
        assert_eq!(row.name, format!("{i:02}.tif"));
        assert_eq!(
            row.trailing_text, "10:00:00",
            "the row shows the time of day, not the full date"
        );
        assert!(!row.excluded);
        assert!(!row.unresolved);
    }
    assert!(panel.header_text.contains('4'));
}

#[test]
fn a_directory_with_a_prior_fuses_own_outputs_lists_only_its_real_frames() {
    let root = tree("directory_with_prior_outputs");
    for i in 0..3 {
        write_timed(&root, &format!("{i:02}.tif"), Some("2026:08:29 10:00:00"));
    }
    // A prior fuse's own outputs, sitting beside the sources exactly as a
    // real one leaves them -- never real TIFF content, since nothing here
    // reads it.
    std::fs::write(
        root.join(focusforge_core::io::COMPOSITE_FILENAME),
        b"a prior fuse's own composite",
    )
    .unwrap();
    std::fs::write(
        root.join(focusforge_core::io::ZMAP_FILENAME),
        b"a prior fuse's own zmap",
    )
    .unwrap();

    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        OpenOutcome::Opened(_)
    ));
    let panel = controller.images_panel();
    assert_eq!(
        panel.rows.len(),
        3,
        "the project's own prior outputs must not be listed as frames"
    );
    for row in &panel.rows {
        assert_ne!(row.name, focusforge_core::io::COMPOSITE_FILENAME);
        assert_ne!(row.name, focusforge_core::io::ZMAP_FILENAME);
    }
    assert!(panel.header_text.contains('3'));
}

#[test]
fn absent_capture_time_is_shown_as_absent() {
    let root = tree("absent_capture_time");
    write_timed(&root, "00.tif", Some("2026:08:29 10:00:00"));
    write_timed(&root, "01.tif", None);
    let mut controller = AppController::new();
    // An untimed frame beside a timed one proposes two groups (one timed,
    // one `Untimed` signal) -- answer straight back into a single project,
    // the same as a user choosing "One project" would.
    match common::open_and_settle(&mut controller, &[root]) {
        OpenOutcome::Opened(_) => {}
        OpenOutcome::Question(_, _) => {
            common::answer_and_settle(&mut controller, OpenAs::OneProject);
        }
        OpenOutcome::Refused(_) => panic!("opening two tiny synthetic frames must not be refused"),
    }
    let panel = controller.images_panel();
    assert_eq!(panel.rows[0].trailing_text, "10:00:00");
    assert_eq!(panel.rows[1].trailing_text, "", "no fabricated time");
    // `ImagesModel` names every field it has -- there is no collective note
    // field to omit.
    let ImagesModel {
        rows: _,
        header_text: _,
        no_source_row: _,
    } = panel;
}

#[test]
fn rows_carry_no_index() {
    // `ImageRow` names every field it has -- there is no index field to
    // destructure past.
    let controller = controller_with_timed_frames("rows_carry_no_index", 3);
    let panel = controller.images_panel();
    for row in &panel.rows {
        // Exhaustive: if `ImageRow` ever grows an index field, this
        // destructure stops compiling.
        let ImageRow {
            chip: _,
            name: _,
            trailing_text: _,
            excluded: _,
            unresolved: _,
            selected: _,
        } = row;
    }
}

// ---------------------------------------------------------------------
// Selection: exactly one frame selected, a position rather than an
// identity, driving the left canvas pane.
// ---------------------------------------------------------------------

fn mantis64_frame(index: usize) -> PathBuf {
    common::fixture_frame("mantis-64", index)
}

fn controller_with_mantis64_frames(name: &str) -> AppController {
    let root = tree(&format!("{name}_frames"));
    for i in 0..3 {
        std::fs::copy(mantis64_frame(i), root.join(format!("{i:02}.tif")))
            .expect("fixture frame must copy");
    }
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        OpenOutcome::Opened(_)
    ));
    controller
}

/// The selected position, the path the canvas is fed and the panel's own
/// `selected` flags are one value -- never index 0, the last row instead.
#[test]
fn frame_selection_drives_left_pane() {
    let mut controller = controller_with_mantis64_frames("frame_selection_drives_left_pane");
    let last = 2;
    controller.select_frame(last);

    let path = controller
        .selected_frame_path()
        .expect("a tab is open with frames to select");
    assert_eq!(path.file_name().unwrap().to_str().unwrap(), "02.tif");
    assert_eq!(controller.frame_pane_label(), "02.tif");

    let panel = controller.images_panel();
    for (i, row) in panel.rows.iter().enumerate() {
        assert_eq!(
            row.selected,
            i == last,
            "row {i}'s selected flag must match the position picked, not index 0"
        );
    }
}

/// One row carries `selected` after open, after removing the selected row,
/// after removing down to the list's last remaining row, and after the
/// post-run re-seed -- never zero, never two. Seeding a first-excluded set
/// skips the excluded position, checked directly against the seed function
/// tab creation and the post-run re-seed both call.
#[test]
fn selection_is_always_exactly_one() {
    assert_eq!(
        focusforge_gui::images::first_included(&[true, false, false]),
        1,
        "seeding must skip an excluded first frame"
    );

    fn selected_count(controller: &AppController) -> usize {
        controller
            .images_panel()
            .rows
            .iter()
            .filter(|r| r.selected)
            .count()
    }

    let mut controller = controller_with_timed_frames("selection_always_one", 3);
    assert_eq!(selected_count(&controller), 1, "after open");

    controller.select_frame(1);
    controller.remove_frame(1);
    assert_eq!(
        selected_count(&controller),
        1,
        "after removing the selected row"
    );

    // Down to the last remaining row: select it, then remove it.
    let last = controller.images_panel().rows.len() - 1;
    controller.select_frame(last);
    controller.remove_frame(last);
    assert_eq!(
        selected_count(&controller),
        1,
        "after removing the selected row when it was the list's last row"
    );

    // The post-run re-seed: a second fuse must land exactly one selected
    // row again, regardless of what was selected before it started.
    let mut controller = controller_with_frames("selection_always_one_run", 3);
    // An algorithm that always converges keeps the fuse out of the test.
    controller.set_align_algorithm("dummy-align");
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));
    controller.select_frame(2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));
    assert_eq!(selected_count(&controller), 1, "after the post-run re-seed");
    assert!(
        controller.images_panel().rows[0].selected,
        "the re-seed lands on the first included frame"
    );
}

/// Selection is a position, not an identity: the same frame stays selected
/// across a manual reorder that moves it, and across removing a frame that
/// now sits above it.
#[test]
fn selection_survives_reorder_and_remove() {
    let mut controller = controller_with_timed_frames("selection_survives", 4);
    controller.select_frame(2);
    let selected_name = controller.frame_pane_label();
    assert_eq!(selected_name, "02.tif");

    // A manual drag from below the selection to above it.
    controller.move_frame(3, 0);
    assert_eq!(
        controller.frame_pane_label(),
        selected_name,
        "the same frame stays selected across a reorder"
    );

    let panel = controller.images_panel();
    let selected_pos = panel.rows.iter().position(|r| r.selected).unwrap();
    assert!(
        selected_pos > 0,
        "the reorder must have left at least one row above the selection"
    );

    // Removing a frame above the (moved) selection.
    controller.remove_frame(0);
    assert_eq!(
        controller.frame_pane_label(),
        selected_name,
        "the same frame stays selected across removing a frame above it"
    );
}

// ---------------------------------------------------------------------
// Exclude: reversible, changes no index, counted.
// ---------------------------------------------------------------------

#[test]
fn exclude_does_not_repoint_indices() {
    let mut controller = controller_with_timed_frames("exclude_no_repoint", 4);
    let before = controller.images_panel();
    controller.toggle_frame_excluded(1);
    let after = controller.images_panel();

    assert_eq!(after.rows.len(), before.rows.len());
    for i in [0usize, 2, 3] {
        assert_eq!(
            after.rows[i].chip, before.rows[i].chip,
            "row {i}'s legend colour must not move"
        );
        assert_eq!(after.rows[i].name, before.rows[i].name);
    }
    assert!(!before.rows[1].excluded);
    assert!(after.rows[1].excluded);
}

#[test]
fn exclude_is_reversible_and_counted() {
    let mut controller = controller_with_timed_frames("exclude_reversible", 3);
    assert_eq!(
        controller.images_panel().header_text,
        "3 frames \u{b7} 0 excluded"
    );

    controller.toggle_frame_excluded(0);
    assert_eq!(
        controller.images_panel().header_text,
        "3 frames \u{b7} 1 excluded"
    );
    assert!(controller.images_panel().rows[0].excluded);

    controller.toggle_frame_excluded(0);
    assert_eq!(
        controller.images_panel().header_text,
        "3 frames \u{b7} 0 excluded"
    );
    assert!(!controller.images_panel().rows[0].excluded);
}

// ---------------------------------------------------------------------
// Add/reorder/remove: free before a checkpoint, gated once one exists.
// ---------------------------------------------------------------------

#[test]
fn curating_is_free_before_the_first_fuse() {
    let mut controller = controller_with_timed_frames("curating_is_free", 3);
    assert_eq!(controller.checkpoint_count(), Some(0));

    let extra = write_timed(&tree("curating_is_free_extra"), "extra.tif", None);
    common::add_and_settle(&mut controller, &[extra]).expect("the add must not refuse");
    assert!(controller.pending_frame_change().is_none());
    assert_eq!(controller.images_panel().rows.len(), 4);

    controller.move_frame(0, 3);
    assert!(controller.pending_frame_change().is_none());

    controller.remove_frame(0);
    assert!(controller.pending_frame_change().is_none());
    assert_eq!(controller.images_panel().rows.len(), 3);
}

#[test]
fn frame_set_change_warns_with_a_count() {
    let mut controller = controller_with_frames("frame_set_change_warns", 2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));
    assert_eq!(controller.checkpoint_count(), Some(1));

    controller.remove_frame(0);
    let warning = controller
        .pending_frame_change()
        .expect("a checkpoint already exists, so removing must warn first");
    assert!(
        warning.body.contains('1'),
        "the warning must state the count"
    );
    // Nothing applied yet.
    assert_eq!(controller.images_panel().rows.len(), 2);
}

#[test]
fn add_and_reorder_offer_a_new_project_with_the_same_settings() {
    let mut controller = controller_with_frames("add_reorder_new_project", 2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));

    let extra = write_timed(&tree("add_reorder_extra"), "extra.tif", None);
    common::add_and_settle(&mut controller, &[extra]).expect("the add must not refuse");
    let add_warning = controller.pending_frame_change().unwrap();
    assert!(add_warning.new_project_button);
    assert!(!add_warning.exclude_instead_button);
    controller.cancel_frame_change();

    controller.move_frame(0, 1);
    let move_warning = controller.pending_frame_change().unwrap();
    assert!(move_warning.new_project_button);
    assert!(!move_warning.exclude_instead_button);
}

#[test]
fn remove_offers_exclude_instead_and_mentions_a_new_project() {
    let mut controller = controller_with_frames("remove_offers_exclude", 2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));

    controller.remove_frame(0);
    let warning = controller.pending_frame_change().unwrap();
    assert!(warning.exclude_instead_button);
    assert!(!warning.new_project_button);
    assert!(warning.info.contains("new project"));
}

#[test]
fn confirm_button_is_not_the_default() {
    let mut controller = controller_with_frames("confirm_not_default", 2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));

    controller.remove_frame(0);
    let warning = controller.pending_frame_change().unwrap();
    assert_eq!(warning.default_button, DefaultButton::Cancel);

    let before = controller.images_panel().rows.len();
    controller.cancel_frame_change();
    assert!(controller.pending_frame_change().is_none());
    assert_eq!(
        controller.images_panel().rows.len(),
        before,
        "Cancel leaves the project untouched"
    );
}

#[test]
fn confirming_a_pending_change_applies_it_and_re_fuses() {
    let mut controller = controller_with_frames("confirm_applies", 2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));
    assert_eq!(controller.checkpoint_count(), Some(1));

    controller.remove_frame(0);
    assert!(controller.pending_frame_change().is_some());
    assert!(controller.confirm_frame_change());
    assert!(controller.pending_frame_change().is_none());
    assert_eq!(controller.images_panel().rows.len(), 1);
    wait_for_settle(&mut controller, Duration::from_secs(120));
    assert_eq!(
        controller.checkpoint_count(),
        Some(2),
        "confirming appends, never discards"
    );
}

// ---------------------------------------------------------------------
// Additive import: duplicate identity by content, replace/keep/cancel.
// ---------------------------------------------------------------------

/// [`controller_with_frames`], opened twice over two fresh trees, both
/// stacking-mode tabs -- the second one active, as any newly opened tab is.
fn controller_with_two_projects(name: &str) -> AppController {
    let root_a = tree(&format!("{name}_a"));
    for i in 0..2 {
        copy_frame(&root_a, &format!("{i:02}.tif"), i);
    }
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root_a]),
        OpenOutcome::Opened(_)
    ));
    let root_b = tree(&format!("{name}_b"));
    for i in 0..2 {
        copy_frame(&root_b, &format!("{i:02}.tif"), i);
    }
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root_b]),
        OpenOutcome::Opened(_)
    ));
    controller
}

#[test]
fn import_of_a_renamed_copy_is_a_conflict_by_content() {
    let mut controller = controller_with_frames("import_conflict_by_content", 2);
    let extra_dir = tree("import_conflict_by_content_extra");
    copy_frame(&extra_dir, "renamed.tif", 0);

    common::add_and_settle(&mut controller, &[extra_dir.join("renamed.tif")])
        .expect("the add must not refuse");

    let warning = controller
        .pending_frame_change()
        .expect("a content-identical file must ask before replacing");
    assert!(
        warning.body.contains("00.tif"),
        "names the existing conflicting frame, not the incoming one"
    );
    assert_eq!(
        controller.images_panel().rows.len(),
        2,
        "nothing applied yet"
    );
}

#[test]
fn import_of_a_different_frame_under_an_existing_name_is_not_a_conflict() {
    let mut controller = controller_with_frames("import_same_name_diff_content", 2);
    let extra_dir = tree("import_same_name_diff_content_extra");
    copy_frame(&extra_dir, "00.tif", 5);

    common::add_and_settle(&mut controller, &[extra_dir.join("00.tif")])
        .expect("the add must not refuse");

    assert!(
        controller.pending_frame_change().is_none(),
        "a same-named, different-content file is not a conflict"
    );
    assert_eq!(
        controller.images_panel().rows.len(),
        3,
        "the new file was added, not merged into the existing row"
    );
}

#[test]
fn import_collapses_identical_incoming_copies_into_one_row() {
    let mut controller = controller_with_frames("import_collapse_incoming", 2);
    let extra_dir = tree("import_collapse_incoming_extra");
    copy_frame(&extra_dir, "copy-a.tif", 6);
    copy_frame(&extra_dir, "copy-b.tif", 6);

    common::add_and_settle(
        &mut controller,
        &[extra_dir.join("copy-a.tif"), extra_dir.join("copy-b.tif")],
    )
    .expect("the add must not refuse");

    assert!(controller.pending_frame_change().is_none());
    assert_eq!(
        controller.images_panel().rows.len(),
        3,
        "two identical incoming files add one row, not two"
    );
}

#[test]
fn import_replace_puts_the_new_file_at_the_same_position_and_keeps_exclusion() {
    let mut controller = controller_with_frames("import_replace_position", 3);
    controller.toggle_frame_excluded(1);
    assert!(controller.images_panel().rows[1].excluded);

    let extra_dir = tree("import_replace_position_extra");
    copy_frame(&extra_dir, "renamed-01.tif", 1);
    copy_frame(&extra_dir, "new.tif", 6);

    common::add_and_settle(
        &mut controller,
        &[extra_dir.join("renamed-01.tif"), extra_dir.join("new.tif")],
    )
    .expect("the add must not refuse");
    assert!(controller.pending_frame_change().is_some());

    assert!(
        !controller.confirm_frame_change(),
        "the duplicate question's own confirm never re-fuses directly"
    );
    assert!(controller.pending_frame_change().is_none());

    let panel = controller.images_panel();
    assert_eq!(panel.rows.len(), 4, "one replace in place, one new row");
    assert_eq!(
        panel.rows[1].name, "renamed-01.tif",
        "the replacement lands at the same position"
    );
    assert!(
        panel.rows[1].excluded,
        "a replaced excluded frame stays excluded"
    );
}

#[test]
fn import_keep_existing_retains_the_conflict_and_still_adds_new_frames() {
    let mut controller = controller_with_frames("import_keep_existing", 2);
    let extra_dir = tree("import_keep_existing_extra");
    copy_frame(&extra_dir, "renamed.tif", 0);
    copy_frame(&extra_dir, "new.tif", 6);

    common::add_and_settle(
        &mut controller,
        &[extra_dir.join("renamed.tif"), extra_dir.join("new.tif")],
    )
    .expect("the add must not refuse");
    let existing_name = controller.images_panel().rows[0].name.clone();

    assert!(controller.keep_existing());
    assert!(controller.pending_frame_change().is_none());

    let panel = controller.images_panel();
    assert_eq!(
        panel.rows.len(),
        3,
        "the new frame is added even when the conflict is declined"
    );
    assert_eq!(
        panel.rows[0].name, existing_name,
        "the existing frame is untouched"
    );
}

#[test]
fn import_cancel_leaves_the_project_unchanged() {
    let mut controller = controller_with_frames("import_cancel", 2);
    let before = controller.images_panel();

    let extra_dir = tree("import_cancel_extra");
    copy_frame(&extra_dir, "renamed.tif", 0);
    copy_frame(&extra_dir, "new.tif", 6);

    common::add_and_settle(
        &mut controller,
        &[extra_dir.join("renamed.tif"), extra_dir.join("new.tif")],
    )
    .expect("the add must not refuse");
    assert!(controller.pending_frame_change().is_some());

    controller.cancel_frame_change();
    assert!(controller.pending_frame_change().is_none());
    assert_eq!(
        controller.images_panel(),
        before,
        "cancelling changes nothing"
    );
}

#[test]
fn import_all_declined_opens_no_checkpoint_warning_and_starts_no_run() {
    let mut controller = controller_with_frames("import_all_declined", 2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));
    assert_eq!(controller.checkpoint_count(), Some(1));

    let extra_dir = tree("import_all_declined_extra");
    copy_frame(&extra_dir, "renamed.tif", 0);

    common::add_and_settle(&mut controller, &[extra_dir.join("renamed.tif")])
        .expect("the add must not refuse");
    assert!(controller.pending_frame_change().is_some());

    assert!(
        controller.keep_existing(),
        "declining the only conflict resolves to nothing"
    );
    assert!(
        controller.pending_frame_change().is_none(),
        "an empty resolved change never opens the checkpoint warning"
    );
    assert_eq!(controller.checkpoint_count(), Some(1), "no run started");
}

#[test]
fn import_of_the_same_paths_accepted_resolves_to_nothing_and_never_re_fuses() {
    let mut controller = controller_with_frames("import_same_paths_accepted", 2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));
    assert_eq!(controller.checkpoint_count(), Some(1));

    let same_path = controller
        .selected_frame_path()
        .expect("a frame is always selected");

    common::add_and_settle(&mut controller, &[same_path]).expect("the add must not refuse");
    assert!(
        controller.pending_frame_change().is_some(),
        "re-dropping the very same file still asks first"
    );

    assert!(
        !controller.confirm_frame_change(),
        "a resolved-to-nothing import never re-fuses"
    );
    assert!(controller.pending_frame_change().is_none());
    assert_eq!(controller.checkpoint_count(), Some(1), "no run started");
    assert_eq!(controller.images_panel().rows.len(), 2, "nothing changed");
}

#[test]
fn import_with_a_checkpoint_shows_the_duplicate_question_before_the_add_warning() {
    let mut controller = controller_with_frames("import_checkpoint_order", 2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));
    assert_eq!(controller.checkpoint_count(), Some(1));

    let extra_dir = tree("import_checkpoint_order_extra");
    copy_frame(&extra_dir, "renamed.tif", 0);
    copy_frame(&extra_dir, "new.tif", 6);

    common::add_and_settle(
        &mut controller,
        &[extra_dir.join("renamed.tif"), extra_dir.join("new.tif")],
    )
    .expect("the add must not refuse");

    let duplicate = controller
        .pending_frame_change()
        .expect("the duplicate question comes first");
    assert!(
        duplicate.keep_existing_button,
        "this is the duplicate question, not the add warning"
    );

    assert!(
        !controller.confirm_frame_change(),
        "the duplicate question's own confirm never re-fuses directly"
    );
    let add_warning = controller
        .pending_frame_change()
        .expect("a checkpoint exists, so the ordinary add warning follows");
    assert!(
        !add_warning.keep_existing_button,
        "the add warning follows the duplicate question, and is not it"
    );
    assert!(add_warning.new_project_button);
}

#[test]
fn import_settled_after_a_tab_switch_lands_on_neither_tab() {
    let mut controller = controller_with_two_projects("import_after_tab_switch");
    assert_eq!(controller.active_project(), Some(1));

    let extra_dir = tree("import_after_tab_switch_extra");
    copy_frame(&extra_dir, "new.tif", 6);
    let handle = controller
        .add_frames(&[extra_dir.join("new.tif")])
        .expect("the add must not refuse")
        .expect("adding a real frame spawns a worker");
    handle.join().expect("the import worker did not panic");

    // The scan targeted tab 1; switch away from it before it settles.
    controller.select_project(0);
    controller.settle_import();

    assert_eq!(
        controller.images_panel().rows.len(),
        2,
        "tab 0 never asked for this import"
    );
    controller.select_project(1);
    assert_eq!(
        controller.images_panel().rows.len(),
        2,
        "the scan's own tab is no longer active, so it is discarded too"
    );
}

#[test]
fn import_settled_after_a_frame_was_removed_meanwhile_changes_nothing() {
    let mut controller = controller_with_frames("import_after_frame_removed", 3);

    let extra_dir = tree("import_after_frame_removed_extra");
    copy_frame(&extra_dir, "new.tif", 6);
    let handle = controller
        .add_frames(&[extra_dir.join("new.tif")])
        .expect("the add must not refuse")
        .expect("adding a real frame spawns a worker");
    handle.join().expect("the import worker did not panic");

    // The frame set moved on while the scan was in flight.
    controller.remove_frame(0);
    assert_eq!(controller.images_panel().rows.len(), 2);

    controller.settle_import();

    assert_eq!(
        controller.images_panel().rows.len(),
        2,
        "a scan over a since-changed frame list is discarded, not applied on top of it"
    );
    assert!(controller.pending_frame_change().is_none());
}

#[test]
fn import_wiring_shows_the_duplicate_question_and_keep_existing_button() {
    use std::sync::{Arc, Mutex};

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();

    let mut controller = controller_with_frames("import_wiring_duplicate", 2);
    let extra_dir = tree("import_wiring_duplicate_extra");
    copy_frame(&extra_dir, "renamed.tif", 0);
    common::add_and_settle(&mut controller, &[extra_dir.join("renamed.tif")])
        .expect("the add must not refuse");
    assert!(controller.pending_frame_change().is_some());

    let controller = Arc::new(Mutex::new(controller));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    // The bridge's sink needs a running event loop; a queue drained after
    // the callback stands in for its `ShellEvent::ImagesChanged` push.
    let queued = Arc::new(Mutex::new(Vec::new()));
    controller.lock().unwrap().set_sink(Arc::new({
        let queued = queued.clone();
        move |e| queued.lock().unwrap().push(e)
    }));
    app.set_project_loaded(true);
    focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());

    assert!(app.get_frame_warning_open());
    assert!(app.get_frame_warning_body().contains("00.tif"));
    assert_eq!(app.get_frame_warning_secondary_label(), "Keep existing");

    app.invoke_frame_warning_secondary_pressed();
    if std::mem::take(&mut *queued.lock().unwrap())
        .contains(&focusforge_gui::ShellEvent::ImagesChanged)
    {
        focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());
    }
    assert!(
        !app.get_frame_warning_open(),
        "Keep existing closes the question even when it adds nothing"
    );

    let c = controller.lock().unwrap();
    assert!(
        c.pending_frame_change().is_none(),
        "Keep existing must clear the question"
    );
    assert_eq!(
        c.images_panel().rows.len(),
        2,
        "no new frame in this import, so nothing is added either"
    );
}

/// A mixed drop -- one renamed copy of an existing frame plus one new frame
/// -- answered by clicking the dialog's own "Keep existing" button in a real
/// window. The bridge's sink is swapped for a queue drained after the click,
/// standing in for the running event loop's `ShellEvent::ImagesChanged` push.
#[test]
fn import_wiring_keep_existing_click_adds_the_new_frame_and_closes_the_question() {
    use focusforge_gui::ShellEvent;
    use i_slint_backend_testing::ElementQuery;
    use slint::platform::{PointerEventButton, WindowEvent};
    use slint::{ComponentHandle, Model};
    use std::sync::{Arc, Mutex};

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));

    let mut controller = controller_with_frames("import_wiring_keep_click", 2);
    let extra_dir = tree("import_wiring_keep_click_extra");
    copy_frame(&extra_dir, "renamed.tif", 0);
    copy_frame(&extra_dir, "new.tif", 6);
    common::add_and_settle(
        &mut controller,
        &[extra_dir.join("renamed.tif"), extra_dir.join("new.tif")],
    )
    .expect("the add must not refuse");

    let controller = Arc::new(Mutex::new(controller));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    let queued = Arc::new(Mutex::new(Vec::new()));
    controller.lock().unwrap().set_sink(Arc::new({
        let queued = queued.clone();
        move |e| queued.lock().unwrap().push(e)
    }));
    app.set_project_loaded(true);
    focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
    assert!(app.get_frame_warning_open());

    let button = ElementQuery::from_root(&app)
        .match_predicate(|e| e.accessible_label().as_deref() == Some("Keep existing"))
        .find_first()
        .expect("the duplicate question shows a Keep existing button");
    let at = element_center(&button);
    app.window().dispatch_event(WindowEvent::PointerPressed {
        position: at,
        button: PointerEventButton::Left,
    });
    app.window().dispatch_event(WindowEvent::PointerReleased {
        position: at,
        button: PointerEventButton::Left,
    });
    if std::mem::take(&mut *queued.lock().unwrap()).contains(&ShellEvent::ImagesChanged) {
        focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());
    }

    assert!(
        !app.get_frame_warning_open(),
        "Keep existing closes the question"
    );
    assert_eq!(
        app.get_images_rows().row_count(),
        3,
        "the new frame is added even though the conflict is declined"
    );
}

// ---------------------------------------------------------------------
// Sorting and manual order.
// ---------------------------------------------------------------------

#[test]
fn sort_menu_has_two_entries() {
    // Exhaustive, no wildcard arm: a third `FrameSort` variant would stop
    // this file compiling, not just fail a runtime assertion.
    fn label(sort: FrameSort) -> &'static str {
        match sort {
            FrameSort::Name => "Name",
            FrameSort::CaptureTime => "Capture time",
        }
    }
    assert_eq!(label(FrameSort::Name), "Name");
    assert_eq!(label(FrameSort::CaptureTime), "Capture time");
}

#[test]
fn manual_order_clears_the_sort_selection() {
    let mut controller = controller_with_timed_frames("manual_order_clears_sort", 3);
    controller.set_frame_sort(FrameSort::Name);
    assert_eq!(controller.frame_sort(), Some(FrameSort::Name));

    controller.move_frame(0, 2);
    assert_eq!(
        controller.frame_sort(),
        None,
        "a manual drag leaves neither entry checked"
    );
}

// ---------------------------------------------------------------------
// Stacking-mode-only.
// ---------------------------------------------------------------------

/// [`controller_with_mantis64_frames`], fused once and switched into editor
/// mode through `set_mode`, the same path the mode switch takes.
fn controller_in_editor_mode(name: &str) -> AppController {
    let mut controller = controller_with_mantis64_frames(name);
    controller.set_scratch_root_for_test(tree(&format!("{name}_scratch")));
    // An algorithm that always converges keeps the fuse out of the test.
    controller.set_align_algorithm("dummy-align");
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));
    assert!(
        controller.set_mode(ShellMode::Editor),
        "a fused tab enters editor mode"
    );
    controller
}

#[test]
fn mutations_are_stacking_mode_only() {
    let mut controller = controller_in_editor_mode("mutations_stacking_only");

    let before = controller.images_panel();
    controller.toggle_frame_excluded(0);
    controller.remove_frame(0);
    controller.move_frame(0, 1);
    controller.set_frame_sort(FrameSort::Name);
    let _ = controller.add_frames(&[PathBuf::from("/tmp/does-not-matter.tif")]);
    assert_eq!(
        controller.images_panel(),
        before,
        "no mutation reaches the model outside stacking mode"
    );
    assert!(controller.pending_frame_change().is_none());
}

#[test]
fn mutations_are_refused_while_this_tabs_job_is_running() {
    let mut controller = controller_with_frames("mutations_refused_during_job", 3);
    assert!(controller.start_fuse());
    assert!(controller.job_running());

    let before = controller.images_panel();
    controller.toggle_frame_excluded(0);
    controller.remove_frame(0);
    controller.move_frame(0, 1);
    controller.set_frame_sort(FrameSort::Name);
    let _ = controller.add_frames(&[PathBuf::from("/tmp/does-not-matter.tif")]);
    controller.frame_drag_pointer(0, PointerKind::Down, 0.0);
    assert_eq!(
        controller.images_panel(),
        before,
        "no mutation reaches the model while this tab's own job is running"
    );
    assert!(controller.pending_frame_change().is_none());
    assert_eq!(
        controller.dragging_row(),
        None,
        "a drag must not even start while the job is running"
    );

    wait_for_settle(&mut controller, Duration::from_secs(120));
}

// ---------------------------------------------------------------------
// The `Opened` half of the list.
// ---------------------------------------------------------------------

#[test]
fn a_reopened_project_lists_its_sources() {
    let dir = tree("reopened_lists_sources");
    let source_dir = tree("reopened_lists_sources_frames");
    let sources: Vec<PathBuf> = (0..3)
        .map(|i| {
            write_timed(
                &source_dir,
                &format!("{i:02}.tif"),
                Some("2026:08:29 10:00:00"),
            )
        })
        .collect();
    let project_path = seal_project_with_sources(&dir, &sources);

    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[project_path]),
        OpenOutcome::Opened(_)
    ));
    let panel = controller.images_panel();
    assert_eq!(panel.rows.len(), 3);
    for row in &panel.rows {
        assert!(!row.unresolved);
    }
}

#[test]
fn an_unreachable_source_renders_greyed() {
    let dir = tree("reopened_unreachable_source");
    let source_dir = tree("reopened_unreachable_source_frames");
    let sources: Vec<PathBuf> = (0..2)
        .map(|i| {
            write_timed(
                &source_dir,
                &format!("{i:02}.tif"),
                Some("2026:08:29 10:00:00"),
            )
        })
        .collect();
    let project_path = seal_project_with_sources(&dir, &sources);
    std::fs::remove_file(&sources[0]).unwrap();

    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[project_path]),
        OpenOutcome::Opened(_)
    ));
    let panel = controller.images_panel();
    assert!(panel.rows[0].unresolved);
    assert!(!panel.rows[1].unresolved);
}

#[test]
fn a_sealed_tab_reads_as_having_a_checkpoint() {
    let mut controller = controller_with_frames("sealed_tab_checkpoint_gate", 2);
    assert_eq!(controller.checkpoint_count(), Some(0));
    controller.remove_frame(0);
    assert!(
        controller.pending_frame_change().is_none(),
        "no checkpoint exists yet, so removing must act immediately"
    );

    // A fresh controller over the same fixture frames, this time actually
    // fused -- the point already made above is that `Some(0)` gates nothing.
    let mut controller = controller_with_frames("sealed_tab_checkpoint_gate", 2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));
    controller.remove_frame(0);
    let warning = controller
        .pending_frame_change()
        .expect("a checkpoint now exists, so removing must warn");
    assert!(warning.body.contains('1'));

    let project_path = save_to_proposed(&mut controller);
    let mut reopened = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut reopened, &[project_path]),
        OpenOutcome::Opened(_)
    ));
    reopened.remove_frame(0);
    let reopened_warning = reopened
        .pending_frame_change()
        .expect("the count read back from disk must gate the same way");
    assert!(reopened_warning.body.contains('1'));
}

// ---------------------------------------------------------------------
// Row metadata: read when a source joins, then carried with it.
// ---------------------------------------------------------------------

fn row_names_and_times(controller: &AppController) -> Vec<(String, String)> {
    controller
        .images_panel()
        .rows
        .into_iter()
        .map(|r| (r.name, r.trailing_text))
        .collect()
}

fn owned_pairs(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(name, time)| (name.to_string(), time.to_string()))
        .collect()
}

#[test]
fn sorting_by_capture_time_uses_the_cached_times() {
    let root = tree("images_sort_cached_times_frames");
    write_timed(&root, "a.tif", Some("2026:08:29 10:00:30"));
    write_timed(&root, "b.tif", Some("2026:08:29 10:00:10"));
    write_timed(&root, "c.tif", Some("2026:08:29 10:00:20"));
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, std::slice::from_ref(&root)),
        OpenOutcome::Opened(_)
    ));
    controller.set_frame_sort(FrameSort::Name);
    std::fs::remove_dir_all(&root).unwrap();

    controller.set_frame_sort(FrameSort::CaptureTime);
    assert_eq!(
        row_names_and_times(&controller),
        owned_pairs(&[
            ("b.tif", "10:00:10"),
            ("c.tif", "10:00:20"),
            ("a.tif", "10:00:30"),
        ])
    );
}

#[test]
fn metadata_follows_the_source_through_reorder_and_remove() {
    let root = tree("images_meta_follows_source_frames");
    for i in 0..4 {
        write_timed(
            &root,
            &format!("{i:02}.tif"),
            Some(&format!("2026:08:29 10:00:0{i}")),
        );
    }
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, std::slice::from_ref(&root)),
        OpenOutcome::Opened(_)
    ));
    std::fs::remove_dir_all(&root).unwrap();

    controller.frame_drag_pointer(0, PointerKind::Down, 10.0);
    controller.frame_drag_pointer(0, PointerKind::Up, 10.0 + 2.0 * ROW_HEIGHT);
    controller.remove_frame(0);

    assert_eq!(
        row_names_and_times(&controller),
        owned_pairs(&[
            ("02.tif", "10:00:02"),
            ("00.tif", "10:00:00"),
            ("03.tif", "10:00:03"),
        ])
    );
}

#[test]
fn added_frames_bring_their_own_capture_time() {
    let mut controller = controller_with_timed_frames("images_added_frame_time", 2);
    let extra = write_timed(
        &tree("images_added_frame_time_extra"),
        "added.tif",
        Some("2026:08:29 10:00:07"),
    );
    common::add_and_settle(&mut controller, std::slice::from_ref(&extra))
        .expect("the frame imports");

    let rows = controller.images_panel().rows;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].name, "added.tif");
    assert!(!rows[2].unresolved);
    assert_eq!(rows[2].trailing_text, "10:00:07");
}

#[test]
fn relinking_refreshes_the_rows_metadata() {
    let dir = tree("images_relink_project");
    let old_dir = tree("images_relink_old_sources");
    let sources: Vec<PathBuf> = (0..2)
        .map(|i| {
            write_timed(
                &old_dir,
                &format!("{i:02}.tif"),
                Some(&format!("2026:08:29 10:00:0{i}")),
            )
        })
        .collect();
    let project_path = seal_project_with_sources(&dir, &sources);
    std::fs::remove_dir_all(&old_dir).unwrap();
    let new_dir = tree("images_relink_new_sources");
    for i in 0..2 {
        write_timed(
            &new_dir,
            &format!("{i:02}.tif"),
            Some(&format!("2026:08:29 11:00:0{i}")),
        );
    }

    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[project_path]),
        OpenOutcome::Opened(_)
    ));
    assert!(controller.images_panel().rows.iter().all(|r| r.unresolved));

    // The open tab itself carries the `Sources missing` failure.
    controller.locate_missing_sources(0, new_dir);
    let rows = controller.images_panel().rows;
    assert!(rows.iter().all(|r| !r.unresolved), "{rows:?}");
    assert_eq!(
        rows.iter()
            .map(|r| r.trailing_text.as_str())
            .collect::<Vec<_>>(),
        vec!["11:00:00", "11:00:01"]
    );
}

#[test]
fn a_source_deleted_after_load_is_refused_on_fuse_then_shown_unresolved() {
    use std::sync::{Arc, Mutex};

    let mut controller = controller_with_timed_frames("images_deleted_after_load", 3);
    let events = Arc::new(Mutex::new(Vec::new()));
    controller.set_sink(Arc::new({
        let events = events.clone();
        move |event| events.lock().unwrap().push(event)
    }));
    std::fs::remove_file(
        PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("images_deleted_after_load_frames")
            .join("01.tif"),
    )
    .unwrap();

    let row = &controller.images_panel().rows[1];
    assert!(!row.unresolved, "nothing polls the disk before use");
    assert_eq!(row.trailing_text, "10:00:00");

    assert!(!controller.start_fuse());
    let refusal = controller.last_refusal().expect("the fuse is refused");
    assert!(
        refusal.contains("01.tif") && refusal.contains("is no longer on disk"),
        "{refusal}"
    );
    let rows = controller.images_panel().rows;
    assert!(rows[1].unresolved);
    assert!(!rows[0].unresolved && !rows[2].unresolved);
    assert!(
        events
            .lock()
            .unwrap()
            .contains(&focusforge_gui::ShellEvent::ImagesChanged)
    );
}

// ---------------------------------------------------------------------
// The sort menu's own open/closed state and a grip drag.
// ---------------------------------------------------------------------

#[test]
fn the_sort_menu_toggles_open_and_closed() {
    let mut controller = controller_with_timed_frames("sort_menu_toggles", 3);
    assert!(!controller.images_sort_menu_open());
    controller.toggle_images_sort_menu();
    assert!(controller.images_sort_menu_open());
    controller.toggle_images_sort_menu();
    assert!(!controller.images_sort_menu_open());
}

#[test]
fn selecting_a_sort_closes_the_menu() {
    let mut controller = controller_with_timed_frames("sort_menu_closes_on_pick", 3);
    controller.toggle_images_sort_menu();
    assert!(controller.images_sort_menu_open());
    controller.set_frame_sort(FrameSort::Name);
    assert!(!controller.images_sort_menu_open());
}

#[test]
fn dragging_reports_a_live_pixel_offset_clamped_to_the_list_bounds() {
    let mut controller = controller_with_timed_frames("grip_drag_live_offset", 4);
    assert_eq!(controller.drag_offset(), 0.0, "no drag yet");

    controller.frame_drag_pointer(0, PointerKind::Down, 10.0);
    assert_eq!(controller.drag_offset(), 0.0, "no movement since the press");

    controller.frame_drag_pointer(0, PointerKind::Move, 10.0 + 1.6 * ROW_HEIGHT);
    assert_eq!(
        controller.drag_offset(),
        1.6 * ROW_HEIGHT,
        "the offset follows the pointer continuously, not in whole-row steps"
    );

    // Row 0 dragged toward the end of a 4-row list cannot float past the
    // last row's own slot.
    controller.frame_drag_pointer(0, PointerKind::Move, 10.0 + 50.0 * ROW_HEIGHT);
    assert_eq!(controller.drag_offset(), 3.0 * ROW_HEIGHT);

    controller.frame_drag_pointer(0, PointerKind::Up, 10.0 + 50.0 * ROW_HEIGHT);
    assert_eq!(
        controller.drag_offset(),
        0.0,
        "the release clears the offset"
    );
}

#[test]
fn dragging_reports_its_live_landing_row() {
    let mut controller = controller_with_timed_frames("grip_drag_live_target", 4);
    assert_eq!(controller.drag_target_row(), None, "no drag yet");

    controller.frame_drag_pointer(0, PointerKind::Down, 10.0);
    assert_eq!(
        controller.drag_target_row(),
        Some(0),
        "releasing now would land it back where it started"
    );

    // The same halfway-point rounding `a_grip_drag_moves_the_row_by_whole_row_steps`
    // proves against the release path.
    controller.frame_drag_pointer(0, PointerKind::Move, 10.0 + 1.6 * ROW_HEIGHT);
    assert_eq!(controller.drag_target_row(), Some(2));

    // Row 0 dragged toward the end of a 4-row list cannot target past the
    // last row.
    controller.frame_drag_pointer(0, PointerKind::Move, 10.0 + 50.0 * ROW_HEIGHT);
    assert_eq!(controller.drag_target_row(), Some(3));

    controller.frame_drag_pointer(0, PointerKind::Up, 10.0 + 50.0 * ROW_HEIGHT);
    assert_eq!(
        controller.drag_target_row(),
        None,
        "the release ends the drag"
    );
}

#[test]
fn a_grip_drag_moves_the_row_by_whole_row_steps() {
    let mut controller = controller_with_timed_frames("grip_drag_moves_row", 4);
    let before: Vec<String> = controller
        .images_panel()
        .rows
        .iter()
        .map(|r| r.name.clone())
        .collect();
    assert_eq!(before, vec!["00.tif", "01.tif", "02.tif", "03.tif"]);

    controller.frame_drag_pointer(0, PointerKind::Down, 10.0);
    assert_eq!(controller.dragging_row(), Some(0));
    controller.frame_drag_pointer(0, PointerKind::Move, 10.0 + ROW_HEIGHT * 1.6);
    // A drag past the halfway point of the second row's own step commits to
    // moving two rows down once released.
    controller.frame_drag_pointer(0, PointerKind::Up, 10.0 + ROW_HEIGHT * 1.6);
    assert_eq!(controller.dragging_row(), None);

    let after: Vec<String> = controller
        .images_panel()
        .rows
        .iter()
        .map(|r| r.name.clone())
        .collect();
    assert_eq!(after, vec!["01.tif", "02.tif", "00.tif", "03.tif"]);
}

#[test]
fn releasing_a_grip_drag_on_the_same_row_moves_nothing() {
    let mut controller = controller_with_timed_frames("grip_drag_no_move", 3);
    controller.frame_drag_pointer(1, PointerKind::Down, 5.0);
    controller.frame_drag_pointer(1, PointerKind::Up, 5.0 + ROW_HEIGHT * 0.2);
    let names: Vec<String> = controller
        .images_panel()
        .rows
        .iter()
        .map(|r| r.name.clone())
        .collect();
    assert_eq!(names, vec!["00.tif", "01.tif", "02.tif"]);
}

#[test]
fn sort_menu_and_drag_are_stacking_mode_only() {
    let mut controller = controller_in_editor_mode("sort_and_drag_stacking_only");
    controller.toggle_images_sort_menu();
    assert!(!controller.images_sort_menu_open());
    controller.frame_drag_pointer(0, PointerKind::Down, 0.0);
    assert_eq!(controller.dragging_row(), None);
}

// ---------------------------------------------------------------------
// The grip's real Slint-to-Rust wiring, through an actual window -- a
// controller-level test of `frame_drag_pointer` alone cannot see the
// pointer capture outlive the row it started on.
// ---------------------------------------------------------------------

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

fn element_center(handle: &i_slint_backend_testing::ElementHandle) -> slint::LogicalPosition {
    let pos = handle.absolute_position();
    let size = handle.size();
    slint::LogicalPosition::new(pos.x + size.width / 2.0, pos.y + size.height / 2.0)
}

/// A point in the panel's grip gutter at window height `y`.
fn grip_gutter_point(app: &focusforge_gui::AppWindow, y: f32) -> slint::LogicalPosition {
    let overlay = i_slint_backend_testing::ElementQuery::from_root(app)
        .match_id("FFImagesPanel::grip-overlay")
        .find_first()
        .expect("the grip gutter is always mounted");
    slint::LogicalPosition::new(element_center(&overlay).x, y)
}

#[test]
fn dragging_a_row_by_its_grip_reorders_it_through_the_real_window() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    use slint::platform::{PointerEventButton, WindowEvent};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));

    let controller = Arc::new(Mutex::new(controller_with_timed_frames(
        "images_grip_drag_wire",
        5,
    )));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    app.set_project_loaded(true);
    focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());

    // The headless backend neither re-renders nor pumps the event loop on
    // its own: `mock_elapsed_time` instantiates any conditional element the
    // event just made true, and the state push below stands in for the
    // `invoke_from_event_loop` callback this backend never runs.
    let dispatch = |event: WindowEvent| {
        app.window().dispatch_event(event);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
        let c = controller.lock().unwrap();
        focusforge_gui::bridge::push_images_panel(&app, &c);
    };

    let mut rows = ElementQuery::from_root(&app)
        .match_id("FFImageRow::area")
        .find_all();
    rows.sort_by(|a, b| {
        a.absolute_position()
            .y
            .partial_cmp(&b.absolute_position().y)
            .unwrap()
    });
    assert_eq!(rows.len(), 5, "all five rows must be on screen");
    let row0_center = element_center(&rows[0]);

    let press_pos = grip_gutter_point(&app, row0_center.y);

    dispatch(WindowEvent::PointerMoved {
        position: press_pos,
    });
    dispatch(WindowEvent::PointerPressed {
        position: press_pos,
        button: PointerEventButton::Left,
    });
    assert_eq!(
        controller.lock().unwrap().dragging_row(),
        Some(0),
        "the press must have started a drag"
    );

    let target = slint::LogicalPosition::new(press_pos.x, press_pos.y + 3.0 * ROW_HEIGHT);
    dispatch(WindowEvent::PointerMoved { position: target });
    dispatch(WindowEvent::PointerReleased {
        position: target,
        button: PointerEventButton::Left,
    });

    assert_eq!(
        controller.lock().unwrap().dragging_row(),
        None,
        "the release must have ended the drag"
    );
    let after: Vec<String> = controller
        .lock()
        .unwrap()
        .images_panel()
        .rows
        .iter()
        .map(|r| r.name.clone())
        .collect();
    assert_eq!(
        after,
        vec!["01.tif", "02.tif", "03.tif", "00.tif", "04.tif"],
        "the panel's gutter capture must survive the drag it started"
    );
}

#[test]
fn dragging_a_row_by_its_grip_floats_a_copy_that_follows_the_cursor() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    use slint::platform::{PointerEventButton, WindowEvent};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));

    let controller = Arc::new(Mutex::new(controller_with_timed_frames(
        "images_grip_drag_ghost",
        5,
    )));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    app.set_project_loaded(true);
    focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());

    let dispatch = |event: WindowEvent| {
        app.window().dispatch_event(event);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
        let c = controller.lock().unwrap();
        focusforge_gui::bridge::push_images_panel(&app, &c);
    };

    let find_ghost = || {
        ElementQuery::from_root(&app)
            .match_id("FFImagesPanel::drag-ghost")
            .find_first()
    };
    let find_landing_line = || {
        ElementQuery::from_root(&app)
            .match_id("FFImagesPanel::landing-line")
            .find_first()
    };

    assert!(
        find_ghost().is_none(),
        "no floating copy before a drag starts"
    );
    assert!(
        find_landing_line().is_none(),
        "no landing line before a drag starts"
    );

    let mut rows = ElementQuery::from_root(&app)
        .match_id("FFImageRow::area")
        .find_all();
    rows.sort_by(|a, b| {
        a.absolute_position()
            .y
            .partial_cmp(&b.absolute_position().y)
            .unwrap()
    });
    let row0_center = element_center(&rows[0]);
    let last_row_top = rows[4].absolute_position().y;

    let press_pos = grip_gutter_point(&app, row0_center.y);

    dispatch(WindowEvent::PointerMoved {
        position: press_pos,
    });
    dispatch(WindowEvent::PointerPressed {
        position: press_pos,
        button: PointerEventButton::Left,
    });
    let at_press = find_ghost()
        .expect("a floating copy of the grabbed row appears once the drag starts")
        .absolute_position()
        .y;
    assert!(
        find_landing_line().is_none(),
        "releasing right after the press would move nothing, so no line is drawn yet"
    );

    let target = slint::LogicalPosition::new(press_pos.x, press_pos.y + 3.0 * ROW_HEIGHT);
    dispatch(WindowEvent::PointerMoved { position: target });
    let at_target = find_ghost()
        .expect("the floating copy stays mounted while the drag continues")
        .absolute_position()
        .y;
    assert!(
        (at_target - at_press - 3.0 * ROW_HEIGHT).abs() < 1.0,
        "the floating copy must have moved down by the same pixels the cursor did: \
         from {at_press} to {at_target}"
    );

    // Row 0 dragged down past row 3 lands at index 3 in a 5-row list --
    // the boundary above the last row, where the dragged row would end up.
    let landing_y = find_landing_line()
        .expect("a 3-row move away from its own slot draws the landing line")
        .absolute_position()
        .y;
    assert!(
        (landing_y - last_row_top).abs() < 2.0,
        "the landing line must sit on the boundary above the last row: \
         line at {landing_y}, boundary at {last_row_top}"
    );

    dispatch(WindowEvent::PointerReleased {
        position: target,
        button: PointerEventButton::Left,
    });
    assert!(
        find_ghost().is_none(),
        "the floating copy disappears once the drag is released"
    );
    assert!(
        find_landing_line().is_none(),
        "the landing line disappears once the drag is released"
    );
}

#[test]
fn images_panel_disable_is_whole_panel() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    app.set_project_loaded(true);
    app.set_dock_tab("images".into());

    let panel_opacity = || {
        ElementQuery::from_root(&app)
            .match_id("FFDock::images-panel")
            .find_first()
            .expect("a loaded project on the images tab always shows the panel")
            .computed_opacity()
    };
    let row_opacities = || {
        ElementQuery::from_root(&app)
            .match_id("FFImageRow::area")
            .find_all()
            .into_iter()
            .map(|row| row.computed_opacity())
            .collect::<Vec<_>>()
    };

    app.set_images_enabled(true);
    assert_eq!(panel_opacity(), 1.0, "enabled draws at full opacity");
    app.set_images_enabled(false);
    assert_eq!(
        panel_opacity(),
        0.5,
        "disabling reads as one whole-panel opacity drop, not a per-row one"
    );
    app.set_images_enabled(true);
    assert_eq!(panel_opacity(), 1.0, "re-enabling restores full opacity");

    let controller = controller_with_frames("images_panel_disable_is_whole_panel", 4);
    focusforge_gui::bridge::push_images_panel(&app, &controller);
    assert!(
        !row_opacities().is_empty(),
        "the pushed frames must show as rows"
    );
    assert_eq!(
        panel_opacity(),
        1.0,
        "a loaded, unlocked list draws at full opacity"
    );

    app.set_images_enabled(false);
    assert_eq!(
        panel_opacity(),
        0.5,
        "disabling dims the panel with rows present"
    );
    assert!(
        row_opacities().iter().all(|&o| o == 0.5),
        "every row inherits the panel's dim, not just the panel background"
    );
    app.set_images_enabled(true);
    assert_eq!(panel_opacity(), 1.0);
    assert!(row_opacities().iter().all(|&o| o == 1.0));
}

#[test]
fn images_panel_editor_mode_is_read_only_not_dimmed() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    app.set_project_loaded(true);
    app.set_dock_tab("images".into());
    app.set_mode("editor".into());

    let panel_opacity = || {
        ElementQuery::from_root(&app)
            .match_id("FFDock::images-panel")
            .find_first()
            .expect("a loaded project on the images tab always shows the panel")
            .computed_opacity()
    };

    app.set_images_enabled(true);
    assert_eq!(
        panel_opacity(),
        1.0,
        "editor mode's read-only list is not shown as locked"
    );
    assert!(
        ElementQuery::from_root(&app)
            .match_id("FFImagesPanel::add-button")
            .find_first()
            .is_none(),
        "editor mode hides the mutating chrome regardless of the dim"
    );

    app.set_images_enabled(false);
    assert_eq!(
        panel_opacity(),
        0.5,
        "a job running in editor mode still dims the panel"
    );
}

#[test]
fn a_running_job_makes_its_own_tabs_images_and_settings_non_interactive() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    use slint::platform::WindowEvent;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));

    let controller = Arc::new(Mutex::new(controller_with_frames(
        "job_disables_images_and_settings",
        3,
    )));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    app.set_project_loaded(true);
    app.set_dock_tab("images".into());
    let push = |c: &AppController| {
        focusforge_gui::bridge::push_images_panel(&app, c);
        focusforge_gui::bridge::push_settings_panel(&app, c);
    };
    push(&controller.lock().unwrap());

    let mut rows = ElementQuery::from_root(&app)
        .match_id("FFImageRow::area")
        .find_all();
    rows.sort_by(|a, b| {
        a.absolute_position()
            .y
            .partial_cmp(&b.absolute_position().y)
            .unwrap()
    });
    let gutter = grip_gutter_point(&app, element_center(&rows[0]).y);
    app.window()
        .dispatch_event(WindowEvent::PointerMoved { position: gutter });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
    assert!(
        ElementQuery::from_root(&app)
            .match_id("FFImageRow::exclude-area")
            .find_first()
            .is_some(),
        "hovering a row's grip gutter shows its controls before any job runs"
    );
    assert!(
        ElementQuery::from_root(&app)
            .match_id("FFImagesPanel::add-button")
            .find_first()
            .is_some(),
        "the add button shows before any job runs"
    );

    assert!(controller.lock().unwrap().start_fuse());
    // The sink that would otherwise carry `ShellEvent::JobChanged` into
    // these pushes no-ops under `init_no_event_loop()` -- done by hand
    // here, exactly what a live window's own event loop does on that event.
    push(&controller.lock().unwrap());

    app.window()
        .dispatch_event(WindowEvent::PointerMoved { position: gutter });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
    assert!(
        ElementQuery::from_root(&app)
            .match_id("FFImageRow::exclude-area")
            .find_first()
            .is_none(),
        "no row's grip or controls may show while this tab's own job is running, even hovered"
    );
    assert!(
        ElementQuery::from_root(&app)
            .match_id("FFImagesPanel::add-button")
            .find_first()
            .is_none(),
        "the add button must not show while this tab's own job is running"
    );
    let images_opacity = ElementQuery::from_root(&app)
        .match_id("FFDock::images-panel")
        .find_first()
        .expect("the images panel is always in the tree")
        .computed_opacity();
    assert_eq!(
        images_opacity, 0.5,
        "the images panel reads as one whole-panel disable while this tab's own job runs"
    );
    app.set_dock_tab("settings".into());
    let settings_opacity = ElementQuery::from_root(&app)
        .match_id("FFDock::settings-panel")
        .find_first()
        .expect("the settings panel is always in the tree")
        .computed_opacity();
    assert_eq!(
        settings_opacity, 0.5,
        "the settings panel reads as one whole-panel disable while this tab's own job runs"
    );

    let start = std::time::Instant::now();
    loop {
        if controller.lock().unwrap().settle_job().is_some() {
            break;
        }
        if !job::is_live(&controller.lock().unwrap().job_view().state) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "job did not settle in time"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    push(&controller.lock().unwrap());
    app.set_dock_tab("images".into());
    let images_opacity = ElementQuery::from_root(&app)
        .match_id("FFDock::images-panel")
        .find_first()
        .expect("the images panel is always in the tree")
        .computed_opacity();
    assert_eq!(
        images_opacity, 1.0,
        "the look follows the job's end, not a ui event"
    );
}

// ---------------------------------------------------------------------
// The virtualized list in a real window: scrolling, instantiation and a
// drag that outlives its row.
// ---------------------------------------------------------------------

/// The first element with id `id`, which the test needs mounted.
fn mounted(app: &focusforge_gui::AppWindow, id: &str) -> i_slint_backend_testing::ElementHandle {
    i_slint_backend_testing::ElementQuery::from_root(app)
        .match_id(id)
        .find_first()
        .unwrap_or_else(|| panic!("{id} is mounted"))
}

/// How many visible elements carry `label`; element queries skip items
/// clipped away, so a row scrolled out of the list does not count.
fn labelled(app: &focusforge_gui::AppWindow, label: &str) -> usize {
    i_slint_backend_testing::ElementHandle::find_by_accessible_label(app, label).count()
}

#[test]
fn scrolling_reads_nothing_and_selection_notifies_only_changed_rows() {
    use focusforge_core::timing::{enable_trace_accounting, timing_snapshot};
    use focusforge_gui::ShellEvent;
    use slint::ComponentHandle;
    use slint::platform::{PointerEventButton, WindowEvent};
    use std::sync::{Arc, Mutex};

    // Timing accounting is process-global, so this runs alone in a child.
    const CHILD: &str = "FOCUSFORGE_GUI_IMAGES_SCROLL_ACCOUNTING_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(
            std::env::current_exe().expect("the test binary has a path"),
        )
        .args([
            "--exact",
            "scrolling_reads_nothing_and_selection_notifies_only_changed_rows",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .status()
        .expect("the isolated accounting test starts");
        assert!(
            status.success(),
            "the isolated accounting test passes: {status}"
        );
        return;
    }

    enable_trace_accounting();
    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    let controller = Arc::new(Mutex::new(controller_with_timed_frames(
        "images_scroll_accounting",
        100,
    )));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    controller.lock().unwrap().set_sink(Arc::new({
        let events = events.clone();
        move |event| events.lock().unwrap().push(event)
    }));
    app.set_project_loaded(true);
    app.set_dock_tab("images".into());
    let push = || focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());
    let settle = || {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
        push();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
    };
    let dispatch = |event: WindowEvent| {
        app.window().dispatch_event(event);
        settle();
    };
    let counters = || {
        let snapshot = timing_snapshot().expect("trace accounting is enabled");
        (snapshot.metadata_reads, snapshot.row_updates)
    };
    push();
    settle();
    std::fs::remove_dir_all(
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("images_scroll_accounting_frames"),
    )
    .unwrap();
    events.lock().unwrap().clear();

    let list_center = element_center(&mounted(&app, "FFImagesPanel::list"));
    dispatch(WindowEvent::PointerScrolled {
        position: list_center,
        delta_x: 0.0,
        delta_y: -100_000.0,
    });
    let selected = || {
        controller
            .lock()
            .unwrap()
            .images_panel()
            .rows
            .iter()
            .position(|r| r.selected)
    };
    assert_eq!(
        controller.lock().unwrap().images_scroll_y(),
        0.0,
        "the wheel over a row steps frames and leaves the list where it is"
    );
    assert_eq!(selected(), Some(99), "the wheel stepped to the last frame");
    dispatch(WindowEvent::PointerScrolled {
        position: list_center,
        delta_x: 0.0,
        delta_y: 100_000.0,
    });
    assert_eq!(controller.lock().unwrap().images_scroll_y(), 0.0);
    assert_eq!(selected(), Some(0), "the opposite wheel stepped back");
    assert!(labelled(&app, "01.tif") > 0 && labelled(&app, "99.tif") == 0);
    let before = counters();
    events.lock().unwrap().clear();

    let bar = mounted(&app, "FFImagesPanel::scrollbar");
    let (bar_pos, bar_size) = (bar.absolute_position(), bar.size());
    let track = slint::LogicalPosition::new(
        bar_pos.x + bar_size.width / 2.0,
        bar_pos.y + bar_size.height * 0.75,
    );
    for event in [
        WindowEvent::PointerMoved { position: track },
        WindowEvent::PointerPressed {
            position: track,
            button: PointerEventButton::Left,
        },
        WindowEvent::PointerReleased {
            position: track,
            button: PointerEventButton::Left,
        },
    ] {
        dispatch(event);
    }
    let jumped = controller.lock().unwrap().images_scroll_y();
    assert!(jumped > 0.0, "a track press jumps the list");
    assert_eq!(-app.get_images_list_viewport_y(), jumped);
    assert_eq!(labelled(&app, "01.tif"), 0, "the list itself moved");

    // The jump centred the thumb on `track`; drag it back up.
    let up = slint::LogicalPosition::new(track.x, track.y - bar_size.height * 0.25);
    for event in [
        WindowEvent::PointerPressed {
            position: track,
            button: PointerEventButton::Left,
        },
        WindowEvent::PointerMoved { position: up },
        WindowEvent::PointerReleased {
            position: up,
            button: PointerEventButton::Left,
        },
    ] {
        dispatch(event);
    }
    let dragged = controller.lock().unwrap().images_scroll_y();
    assert!(dragged < jumped, "the thumb drag moved the list back up");
    assert_eq!(-app.get_images_list_viewport_y(), dragged);

    assert_eq!(
        counters(),
        before,
        "scrolling reads no metadata and updates no row"
    );
    {
        let seen = events.lock().unwrap();
        assert!(!seen.contains(&ShellEvent::ImagesChanged), "{seen:?}");
        assert!(seen.contains(&ShellEvent::ImagesScrolled), "{seen:?}");
    }
    let rows = controller.lock().unwrap().images_panel().rows;
    assert!(
        rows.iter()
            .all(|r| !r.unresolved && r.trailing_text == "10:00:00"),
        "deleted sources keep their cached metadata"
    );

    let row_updates = |edit: &dyn Fn(&mut AppController)| {
        edit(&mut controller.lock().unwrap());
        let before = counters().1;
        push();
        counters().1 - before
    };
    row_updates(&|c| c.select_frame(3));
    assert_eq!(row_updates(&|c| c.select_frame(40)), 2);
    assert_eq!(row_updates(&|c| c.toggle_frame_excluded(40)), 1);

    // A chip's hue is its index over the frame count, so a removal repaints
    // most chips: it notifies the dropped tail slot plus every row whose
    // shown data differs, and nothing else.
    let shown = || {
        use slint::Model;
        app.get_images_rows().iter().collect::<Vec<_>>()
    };
    let old = shown();
    let removed = row_updates(&|c| c.remove_frame(98));
    let new = shown();
    assert_eq!(new.len(), old.len() - 1);
    let changed = new.iter().zip(&old).filter(|(n, o)| n != o).count();
    assert_eq!(removed, (changed + 1) as u64);
    assert_eq!(counters().0, before.0, "no edit re-read the disk");
}

/// The panel's row model with every index the list reads recorded. The
/// bridge still finds and edits the retained `VecModel` inside, since
/// `as_any` forwards to it.
#[derive(Default)]
struct ReadCountingRows {
    rows: slint::VecModel<focusforge_gui::ImageRowView>,
    read: std::rc::Rc<ReadIndices>,
}

#[derive(Default)]
struct ReadIndices(std::cell::RefCell<std::collections::BTreeSet<usize>>);

impl ReadIndices {
    fn take(&self) -> std::collections::BTreeSet<usize> {
        std::mem::take(&mut self.0.borrow_mut())
    }
}

impl slint::Model for ReadCountingRows {
    type Data = focusforge_gui::ImageRowView;

    fn row_count(&self) -> usize {
        self.rows.row_count()
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        self.read.0.borrow_mut().insert(row);
        self.rows.row_data(row)
    }

    fn model_tracker(&self) -> &dyn slint::ModelTracker {
        self.rows.model_tracker()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        &self.rows
    }
}

#[test]
fn a_thousand_rows_instantiate_only_the_visible_window() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    use std::sync::{Arc, Mutex};

    let root = tree("images_thousand_rows_frames");
    for i in 0..1000 {
        write_timed(&root, &format!("{i:04}.tif"), Some("2026:08:29 10:00:00"));
    }
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        OpenOutcome::Opened(_)
    ));
    let last = controller
        .images_panel()
        .rows
        .last()
        .expect("a thousand rows")
        .name
        .clone();

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    let controller = Arc::new(Mutex::new(controller));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    app.set_project_loaded(true);
    app.set_dock_tab("images".into());
    let rows = ReadCountingRows::default();
    let read = rows.read.clone();
    app.set_images_rows(slint::ModelRc::new(rows));
    focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));

    let list = mounted(&app, "FFImagesPanel::list");
    let height = list.size().height;
    assert!(
        height > 0.0 && height < 900.0,
        "the list is bounded by the window: {height}"
    );
    let bound = (height / ROW_HEIGHT).ceil() as usize + 2;
    let instantiated = || {
        ElementQuery::from_root(&app)
            .match_id("FFImageRow::area")
            .find_all()
            .len()
    };
    let shown = instantiated();
    let at_top = read.take();
    assert!(
        !at_top.is_empty() && at_top.len() <= bound && shown <= bound,
        "{} rows read at the top, bound {bound}",
        at_top.len()
    );

    app.set_images_list_viewport_y(-1.0e7);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
    let shown = instantiated();
    let at_end = read.take();
    assert!(
        at_end.len() <= bound && shown <= bound,
        "{} rows read at the end, bound {bound}",
        at_end.len()
    );
    assert!(at_end.contains(&999), "the end was read: {at_end:?}");
    assert!(
        labelled(&app, &last) > 0,
        "the last row, {last}, is reachable"
    );
}

#[test]
fn a_grip_drag_survives_leaving_the_viewport_and_row_recycling() {
    use slint::ComponentHandle;
    use slint::platform::{PointerEventButton, WindowEvent};
    use std::sync::{Arc, Mutex};

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    let controller = Arc::new(Mutex::new(controller_with_timed_frames(
        "images_grip_drag_recycling",
        40,
    )));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    app.set_project_loaded(true);
    app.set_dock_tab("images".into());
    let settle = || {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
        focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
    };
    let dispatch = |event: WindowEvent| {
        app.window().dispatch_event(event);
        settle();
    };
    settle();

    let list = mounted(&app, "FFImagesPanel::list");
    let (top, height) = (list.absolute_position().y, list.size().height);
    let at = |y: f32| grip_gutter_point(&app, y);
    let press = at(top + 1.5 * ROW_HEIGHT);
    dispatch(WindowEvent::PointerMoved { position: press });
    dispatch(WindowEvent::PointerPressed {
        position: press,
        button: PointerEventButton::Left,
    });
    assert_eq!(controller.lock().unwrap().dragging_row(), Some(1));

    dispatch(WindowEvent::PointerMoved {
        position: at(top - 200.0),
    });
    assert_eq!(
        controller.lock().unwrap().drag_offset(),
        -ROW_HEIGHT,
        "moves above the list still arrive, clamped to the first slot"
    );
    dispatch(WindowEvent::PointerMoved {
        position: at(top + height + 200.0),
    });
    assert!(
        controller.lock().unwrap().drag_offset() > 0.0,
        "moves below the list still arrive"
    );
    dispatch(WindowEvent::PointerMoved { position: press });
    assert_eq!(controller.lock().unwrap().drag_offset(), 0.0);
    assert_eq!(labelled(&app, "01.tif"), 2, "the row and its floating copy");

    {
        let mut c = controller.lock().unwrap();
        c.images_scrollbar_pointer(PointerKind::Down, height);
        c.images_scrollbar_pointer(PointerKind::Up, height);
    }
    settle();
    assert!(controller.lock().unwrap().images_scroll_y() > 3.0 * ROW_HEIGHT);
    assert_eq!(
        labelled(&app, "01.tif"),
        0,
        "row 1 scrolled out of the list"
    );
    assert_eq!(controller.lock().unwrap().dragging_row(), Some(1));

    let release = at(top + height - 0.5 * ROW_HEIGHT);
    dispatch(WindowEvent::PointerMoved { position: release });
    dispatch(WindowEvent::PointerReleased {
        position: release,
        button: PointerEventButton::Left,
    });
    let mut c = controller.lock().unwrap();
    assert_eq!(c.dragging_row(), None);
    let names: Vec<String> = c.images_panel().rows.into_iter().map(|r| r.name).collect();
    assert_eq!(
        names[39], "01.tif",
        "the dragged source lands on the last row"
    );
    assert_eq!(names[1], "02.tif");

    c.frame_drag_pointer(40, PointerKind::Down, 0.0);
    assert_eq!(c.dragging_row(), None, "no drag starts past the end");
    std::fs::remove_file(
        PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("images_grip_drag_recycling_frames")
            .join("00.tif"),
    )
    .unwrap();
    assert!(!c.start_fuse());
    assert!(c.images_panel().rows[0].unresolved);
    c.frame_drag_pointer(0, PointerKind::Down, 5.0);
    assert_eq!(
        c.dragging_row(),
        None,
        "no drag starts on an unresolved row"
    );
}

// ---------------------------------------------------------------------
// The two non-destructive footer alternatives.
// ---------------------------------------------------------------------

#[test]
fn exclude_instead_excludes_the_frame_and_clears_the_warning() {
    let mut controller = controller_with_frames("exclude_instead_excludes", 2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));

    controller.remove_frame(0);
    assert!(controller.pending_frame_change().is_some());
    assert!(controller.exclude_instead());
    assert!(controller.pending_frame_change().is_none());

    let panel = controller.images_panel();
    assert_eq!(
        panel.rows.len(),
        2,
        "excluding keeps both rows, unlike removing"
    );
    assert!(panel.rows[0].excluded);
    assert_eq!(
        controller.checkpoint_count(),
        Some(1),
        "excluding never re-fuses"
    );
}

#[test]
fn exclude_instead_is_a_no_op_off_a_remove_warning() {
    let mut controller = controller_with_frames("exclude_instead_wrong_kind", 2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));

    let extra = write_timed(&tree("exclude_instead_wrong_kind_extra"), "extra.tif", None);
    common::add_and_settle(&mut controller, &[extra]).expect("the add must not refuse");
    assert!(controller.pending_frame_change().is_some());
    assert!(!controller.exclude_instead());
    assert!(
        controller.pending_frame_change().is_some(),
        "an Add warning is untouched by Exclude instead"
    );
}

#[test]
fn new_project_instead_opens_a_second_tab_and_leaves_the_first_alone() {
    let mut controller = controller_with_frames("new_project_instead", 2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));
    assert_eq!(controller.checkpoint_count(), Some(1));

    controller.remove_frame(0);
    assert!(controller.pending_frame_change().is_some());
    assert!(controller.new_project_instead());
    assert!(controller.pending_frame_change().is_none());

    let tabs = controller.project_tabs();
    assert_eq!(tabs.len(), 2, "a second tab opened");
    assert!(tabs[1].unsaved);
    assert!(tabs[1].selected, "the new tab becomes active");

    // The first tab is untouched: still 2 frames, still one checkpoint.
    controller.select_project(0);
    assert_eq!(controller.images_panel().rows.len(), 2);
    assert_eq!(controller.checkpoint_count(), Some(1));

    controller.select_project(1);
    assert_eq!(
        controller.images_panel().rows.len(),
        2,
        "the new tab starts from the same frame set"
    );
    assert_eq!(controller.checkpoint_count(), Some(0));
}

#[test]
fn switching_tabs_dismisses_a_stale_frame_set_warning() {
    let mut controller = controller_with_frames("switching_tabs_dismisses_warning", 2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));
    assert_eq!(controller.checkpoint_count(), Some(1));

    controller.remove_frame(0);
    assert!(
        controller.pending_frame_change().is_some(),
        "removing on a fused tab queues the warning"
    );

    let root = tree("switching_tabs_dismisses_warning_second");
    for i in 0..2 {
        copy_frame(&root, &format!("{i:02}.tif"), i);
    }
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(
        controller.checkpoint_count(),
        Some(0),
        "the newly opened tab starts fresh"
    );
    assert!(
        controller.pending_frame_change().is_none(),
        "a warning queued on a different tab must not bleed into this one"
    );

    controller.select_project(0);
    assert!(
        controller.pending_frame_change().is_none(),
        "switching back does not resurrect the dismissed warning"
    );
}

#[test]
fn the_frame_set_warnings_footer_sits_at_the_panels_bottom_edge() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    use std::sync::{Arc, Mutex};

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));

    let mut controller = controller_with_frames("dialog_footer_pinned", 2);
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));
    controller.remove_frame(0);
    assert!(controller.pending_frame_change().is_some());

    let controller = Arc::new(Mutex::new(controller));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    app.set_project_loaded(true);
    focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());

    let panel = ElementQuery::from_root(&app)
        .match_id("FFDialog::panel")
        .find_first()
        .expect("the frame-set warning's own panel must be in the tree while it is open");
    let footer = ElementQuery::from_root(&app)
        .match_type_name("FFDialogFooter")
        .find_first()
        .expect("the warning shows its own footer");

    let panel_bottom = panel.absolute_position().y + panel.size().height;
    let footer_bottom = footer.absolute_position().y + footer.size().height;
    // `Theme.space-4`, 16px -- the panel's own uniform padding, which is
    // what the footer's bottom margin must equal once it is pinned.
    assert!(
        (panel_bottom - footer_bottom - 16.0).abs() < 1.0,
        "the footer must sit flush `space-4` above the panel's own bottom edge \
         regardless of how little body content precedes it: \
         panel bottom {panel_bottom}, footer bottom {footer_bottom}"
    );
}

// ---------------------------------------------------------------------
// Manual evidence generator: a 61-frame tree with real capture times, a
// handful absent, for `--open .scratch/timed-61/ --dock-tab images`.
// ---------------------------------------------------------------------

#[test]
#[ignore]
fn write_the_61_frame_manual_evidence_tree() {
    let dir = common::workspace_root().join(".scratch").join("timed-61");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut sources = Vec::new();
    for i in 0..61u32 {
        let name = format!("IMG_98{i:02}.tif");
        // Every fifth frame carries no capture time -- the absent state the
        // rest of the tree's timed rows sit beside.
        let when = if i % 5 == 0 {
            None
        } else {
            Some(format!(
                "2026:08:29 14:{:02}:{:02}",
                12 + i / 60,
                (12 + i) % 60
            ))
        };
        let path = dir.join(name);
        write_frame_with_capture_time(&path, 4, 4, when.as_deref()).unwrap();
        sources.push(path);
    }
    // A directory mixing timed and untimed frames always proposes more than
    // one group, since `group_one_directory` appends the untimed frames as
    // their own group. Sealing directly into a sibling directory answers
    // that grouping question as "open as one project" would, without the
    // composite/Z-Map ever sitting beside the source frames.
    seal_project_with_sources(&dir.join("sealed"), &sources);
}
