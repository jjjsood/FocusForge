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

// ---------------------------------------------------------------------
// Fixture helpers.
// ---------------------------------------------------------------------

fn mantis_frame(index: usize) -> PathBuf {
    common::workspace_root()
        .join("fixtures")
        .join("mantis-512")
        .join("frames")
        .join(format!("{index:02}.tif"))
}

fn tree(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp tree must be creatable");
    dir
}

fn copy_frame(dest_dir: &Path, name: &str, source_index: usize) {
    std::fs::create_dir_all(dest_dir).unwrap();
    std::fs::copy(mantis_frame(source_index), dest_dir.join(name))
        .expect("fixture frame must copy");
}

fn controller_with_frames(name: &str, count: usize) -> AppController {
    let root = tree(&format!("{name}_frames"));
    for i in 0..count {
        copy_frame(&root, &format!("{i:02}.tif"), i);
    }
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        OpenOutcome::Opened(_)
    ));
    controller.set_scratch_root_for_test(tree(&format!("{name}_scratch")));
    controller
}

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
            return;
        }
        if !job::is_live(&controller.job_view().state) {
            return;
        }
        assert!(
            start.elapsed() < timeout,
            "job did not settle within {timeout:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn opened_project_path(root_name: &str, controller: &AppController) -> PathBuf {
    let title = controller.project_tabs()[0].title.clone();
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(root_name)
        .join(format!("{title}.ffp"))
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
            row.time_text, "10:00:00",
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
    assert_eq!(panel.rows[0].time_text, "10:00:00");
    assert_eq!(panel.rows[1].time_text, "", "no fabricated time");
    // `ImagesModel` names every field it has -- there is no collective note
    // field to omit.
    let ImagesModel {
        rows: _,
        header_text: _,
        content_height: _,
        scroll_y: _,
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
            time_text: _,
            excluded: _,
            unresolved: _,
            top: _,
        } = row;
    }
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
        assert_eq!(after.rows[i].top, before.rows[i].top);
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
    controller.add_frames(&[extra]);
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
    controller.add_frames(&[extra]);
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

#[test]
fn mutations_are_stacking_mode_only() {
    // `AppController::set_mode` refuses `Editor` since no editor mode
    // exists yet, so the mode is forced the way every other editor-gated
    // test in this crate does. `open_paths` does not check mode, so the
    // controller can hold a real project while forced into `Editor`.
    let root = tree("mutations_stacking_only_frames");
    for i in 0..3 {
        write_timed(&root, &format!("{i:02}.tif"), Some("2026:08:29 10:00:00"));
    }
    let mut controller = AppController::new_with_mode_for_test(ShellMode::Editor);
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        OpenOutcome::Opened(_)
    ));

    let before = controller.images_panel();
    controller.toggle_frame_excluded(0);
    controller.remove_frame(0);
    controller.move_frame(0, 1);
    controller.set_frame_sort(FrameSort::Name);
    controller.add_frames(&[PathBuf::from("/tmp/does-not-matter.tif")]);
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
    controller.add_frames(&[PathBuf::from("/tmp/does-not-matter.tif")]);
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

    let project_path = opened_project_path("sealed_tab_checkpoint_gate_frames", &controller);
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
    let root = tree("sort_and_drag_stacking_only_frames");
    for i in 0..3 {
        write_timed(&root, &format!("{i:02}.tif"), Some("2026:08:29 10:00:00"));
    }
    let mut controller = AppController::new_with_mode_for_test(ShellMode::Editor);
    assert!(matches!(
        common::open_and_settle(&mut controller, &[root]),
        OpenOutcome::Opened(_)
    ));
    controller.toggle_images_sort_menu();
    assert!(!controller.images_sort_menu_open());
    controller.frame_drag_pointer(0, PointerKind::Down, 0.0);
    assert_eq!(controller.dragging_row(), None);
}

// ---------------------------------------------------------------------
// The grip's real Slint-to-Rust wiring, through an actual window -- a
// controller-level test of `frame_drag_pointer` alone cannot see the row's
// own `TouchArea` disappear mid-gesture.
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
    let _installed = focusforge_gui::bridge::install(&app, controller.clone());
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

    // Hovering row 0 mounts its grip -- the same reveal a real pointer does.
    dispatch(WindowEvent::PointerMoved {
        position: row0_center,
    });
    let grip = ElementQuery::from_root(&app)
        .match_id("FFImageRow::grip-area")
        .find_first()
        .expect("a hovered row must mount its grip");
    let press_pos = element_center(&grip);

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
        "the grip's own TouchArea must survive the drag it started"
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
    let _installed = focusforge_gui::bridge::install(&app, controller.clone());
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

    dispatch(WindowEvent::PointerMoved {
        position: row0_center,
    });
    let grip = ElementQuery::from_root(&app)
        .match_id("FFImageRow::grip-area")
        .find_first()
        .expect("a hovered row must mount its grip");
    let press_pos = element_center(&grip);

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
    let _installed = focusforge_gui::bridge::install(&app, controller.clone());
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
    let row0_center = element_center(&rows[0]);
    app.window().dispatch_event(WindowEvent::PointerMoved {
        position: row0_center,
    });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
    assert!(
        ElementQuery::from_root(&app)
            .match_id("FFImageRow::grip-area")
            .find_first()
            .is_some(),
        "hovering a row mounts its grip before any job runs"
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

    app.window().dispatch_event(WindowEvent::PointerMoved {
        position: row0_center,
    });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
    assert!(
        ElementQuery::from_root(&app)
            .match_id("FFImageRow::grip-area")
            .find_first()
            .is_none(),
        "no row's grip may mount while this tab's own job is running, even hovered"
    );
    assert!(
        ElementQuery::from_root(&app)
            .match_id("FFImagesPanel::add-button")
            .find_first()
            .is_none(),
        "the add button must not show while this tab's own job is running"
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
    controller.add_frames(&[extra]);
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
    let _installed = focusforge_gui::bridge::install(&app, controller.clone());
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
