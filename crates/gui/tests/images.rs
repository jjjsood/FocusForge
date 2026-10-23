// SPDX-License-Identifier: GPL-3.0-or-later

//! The `Images` dock panel, driven through `AppController` -- fast, no
//! window: the model over `fixtures/mantis-512` and small synthetic timed
//! frame sets, mirroring `tests/job.rs`'s own fixture-copy shape.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use focusforge_core::io::test_frames::write_frame_with_capture_time;
use focusforge_core::tile::HostBudget;
use focusforge_gui::appearance_settings::{AppearanceSettings, SourcePresentation};
use focusforge_gui::images::{DefaultButton, FrameSort, ImageRow, ImagesModel, ROW_HEIGHT};
use focusforge_gui::input_map::PointerKind;
use focusforge_gui::job;
use focusforge_gui::source_previews::{self, PreviewError};
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

fn controller_with_list_frames(name: &str, count: usize) -> AppController {
    let mut controller = controller_with_timed_frames(name, count);
    controller.install_appearance(AppearanceSettings {
        presentation: SourcePresentation::List,
    });
    controller
}

#[test]
fn source_preview_renders_original_rgb_without_crop_and_honours_limits() {
    let root = tree("source_preview_original_rgb");
    let path = write_timed(&root, "edge.tif", Some("2026:08:29 10:00:00"));
    let (preview, reservation) =
        source_previews::render(&path, HostBudget::explicit(32 * 1024 * 1024))
            .expect("a valid TIFF renders off the UI thread");
    assert_eq!((preview.width, preview.height), (4, 4));
    assert_eq!(preview.rgba.len(), 4 * 4 * 4);
    assert!(reservation.total <= 32 * 1024 * 1024);
    assert!(reservation.reduced_samples > 0);

    assert!(matches!(
        source_previews::render(&path, HostBudget::explicit(1)),
        Err(PreviewError::OverBudget)
    ));
    assert!(matches!(
        source_previews::render_cancellable(
            &path,
            HostBudget::explicit(32 * 1024 * 1024),
            &AtomicBool::new(true)
        ),
        Err(PreviewError::Cancelled)
    ));
}

#[test]
fn image_list_queues_only_the_visible_window_plus_two_rows() {
    let mut controller = controller_with_timed_frames("preview_visible_window", 8);
    controller.install_appearance(AppearanceSettings {
        presentation: SourcePresentation::ImageList,
    });
    controller.set_images_viewport_height(120.0);
    let snapshot = controller.source_preview_snapshot();
    assert_eq!(snapshot.scheduled, 3);
    assert!(controller.source_preview_row_visible(0));
    assert!(!controller.source_preview_row_visible(1));
    controller.images_viewport_scrolled(360.0);
    assert_eq!(controller.source_preview_snapshot().scheduled, 5);
    assert!(controller.source_preview_row_visible(3));
    assert!(!controller.source_preview_row_visible(4));
}

#[test]
fn image_list_keeps_a_fractional_thousand_row_viewport_to_the_two_row_margin() {
    let mut controller = controller_with_timed_frames("preview_fractional_thousand", 1000);
    controller.install_appearance(AppearanceSettings {
        presentation: SourcePresentation::ImageList,
    });

    controller.set_images_viewport_height(120.5);
    assert_eq!(
        controller.source_preview_snapshot().scheduled,
        4,
        "the two visible top rows retain two prefetched rows below"
    );
    assert!(controller.source_preview_row_visible(0));
    assert!(controller.source_preview_row_visible(1));
    assert!(!controller.source_preview_row_visible(2));

    controller.images_viewport_scrolled(500.0 * 120.0 + 0.25);
    assert_eq!(
        controller.source_preview_snapshot().scheduled,
        6,
        "an interior fractional viewport schedules its two visible rows plus two on each side"
    );
    assert!(controller.source_preview_row_visible(500));
    assert!(controller.source_preview_row_visible(501));
    assert!(!controller.source_preview_row_visible(499));
    assert!(!controller.source_preview_row_visible(502));
}

#[test]
fn every_directed_source_presentation_transition_preserves_identity_and_tab_state() {
    let mut controller = controller_with_timed_frames("presentation_transition_pairs", 5);
    controller.set_images_viewport_height(120.0);
    controller.select_frame(3);
    controller.toggle_frame_excluded(1);
    let expected = |controller: &AppController| {
        controller
            .images_panel()
            .rows
            .into_iter()
            .map(|row| (row.path, row.excluded, row.selected))
            .collect::<Vec<_>>()
    };
    let before = expected(&controller);
    let selected = before
        .iter()
        .find(|(_, _, selected)| *selected)
        .map(|(path, _, _)| path.clone())
        .expect("one selected source");

    let pairs = [
        (SourcePresentation::List, SourcePresentation::ImageList),
        (SourcePresentation::List, SourcePresentation::Large),
        (SourcePresentation::ImageList, SourcePresentation::List),
        (SourcePresentation::ImageList, SourcePresentation::Large),
        (SourcePresentation::Large, SourcePresentation::List),
        (SourcePresentation::Large, SourcePresentation::ImageList),
    ];
    for (from, to) in pairs {
        controller.set_source_presentation(from);
        controller.set_source_presentation(to);
        assert_eq!(
            expected(&controller),
            before,
            "{from:?} -> {to:?} keeps source identity state"
        );
        let selected_index = controller
            .images_panel()
            .rows
            .iter()
            .position(|row| row.path == selected)
            .expect("selected identity remains listed");
        assert!(
            controller.source_preview_row_visible(selected_index),
            "{from:?} -> {to:?} keeps the selected source visible"
        );
    }

    let other = tree("presentation_transition_second_tab");
    write_timed(&other, "other.tif", Some("2026:08:29 10:00:00"));
    assert!(matches!(
        common::open_and_settle(&mut controller, &[other]),
        OpenOutcome::Opened(_)
    ));
    controller.select_project(0);
    assert_eq!(
        expected(&controller),
        before,
        "a tab round trip keeps the first tab's source state"
    );
}

#[test]
fn source_preview_cap_change_replaces_an_unchanged_visible_set() {
    let mut controller = controller_with_timed_frames("preview_cap_change", 8);
    controller.install_appearance(AppearanceSettings {
        presentation: SourcePresentation::ImageList,
    });
    controller.set_images_viewport_height(120.0);
    let before = controller.source_preview_snapshot();
    assert_eq!(before.scheduled, 3);
    assert!(before.admission_bytes > 1);

    controller.set_host_memory_cap("1");
    let after = controller.source_preview_snapshot();
    assert_eq!(after.scheduled, 3, "the visible identities did not change");
    assert_eq!(after.configured_cap, 1);
    assert_eq!(
        after.admission_bytes, 1,
        "the worker must receive the new cap"
    );
}

#[test]
fn processing_refuses_when_retained_preview_bytes_consume_the_host_cap() {
    let mut controller = controller_with_timed_frames("preview_processing_cap", 2);
    controller.set_host_memory_cap("1");
    controller.set_published_preview_bytes(1);

    assert!(!controller.start_fuse());
    assert!(!controller.any_job_running());
    assert_eq!(
        controller.last_refusal(),
        Some(
            "Source previews use the available host memory. Close or change the source view before starting processing."
        )
    );
}

#[test]
fn live_processing_reserves_cached_native_preview_growth_during_resize() {
    use slint::ComponentHandle;
    use std::sync::{Arc, Mutex};

    let mut controller = controller_with_timed_frames("preview_live_resize_reserve", 4);
    controller.install_appearance(AppearanceSettings {
        presentation: SourcePresentation::ImageList,
    });
    controller.set_host_memory_cap("1MiB");
    controller.set_images_viewport_height(120.0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while controller.source_preview_snapshot().cache_entries < 3 {
        controller.poll_source_previews();
        assert!(
            Instant::now() < deadline,
            "the controlled visible cache did not settle"
        );
        std::thread::yield_now();
    }

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
    focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());

    let before = controller.lock().unwrap().source_preview_snapshot();
    const PIXELS: u64 = 4 * 4 * 4;
    assert_eq!(before.cache_entries, 3);
    assert_eq!(before.resident_bytes, 3 * PIXELS);
    assert_eq!(before.conversion_bytes, 3 * PIXELS);
    assert_eq!(before.native_bytes, PIXELS);
    let running_allowance = before.host_remaining;
    assert!(controller.lock().unwrap().start_fuse());
    assert!(controller.lock().unwrap().job_running());

    controller.lock().unwrap().set_images_viewport_height(360.0);
    focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());
    let after = controller.lock().unwrap().source_preview_snapshot();
    assert_eq!(after.native_bytes, 3 * PIXELS);
    assert_eq!(
        after.host_remaining,
        after
            .configured_cap
            .saturating_sub(after.cache_bytes)
            .saturating_sub(after.native_bytes),
        "the snapshot reports current free space; the already-started worker retains its start allowance"
    );
    assert!(
        after
            .resident_bytes
            .saturating_add(after.native_bytes)
            .saturating_add(running_allowance)
            <= after.configured_cap,
        "the live worker and every actual retained source/native buffer fit the configured cap"
    );
}

#[test]
fn an_idle_tab_cannot_schedule_source_previews_while_another_tab_processes() {
    let mut controller = controller_with_timed_frames("preview_global_job_guard", 4);
    controller.install_appearance(AppearanceSettings {
        presentation: SourcePresentation::ImageList,
    });
    controller.set_images_viewport_height(120.0);

    let other = tree("preview_global_job_guard_second_tab");
    for name in [
        "other-00.tif",
        "other-01.tif",
        "other-02.tif",
        "other-03.tif",
    ] {
        write_timed(&other, name, Some("2026:08:29 10:00:00"));
    }
    assert!(matches!(
        common::open_and_settle(&mut controller, &[other]),
        OpenOutcome::Opened(_)
    ));
    controller.select_project(0);
    assert!(controller.start_fuse());
    assert!(controller.any_job_running());

    controller.select_project(1);
    controller.set_images_viewport_height(360.5);
    controller.images_viewport_scrolled(120.25);
    let snapshot = controller.source_preview_snapshot();
    assert_eq!(snapshot.scheduled, 0);
    assert_eq!(snapshot.admission_bytes, 0);
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
    assert!(common::save_and_settle(controller, |proposed| {
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
        OpenOutcome::Mismatch(_) => panic!("opening a folder must never raise a mismatch"),
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
    } = panel;
}

#[test]
fn rows_retain_the_recorded_capture_stamp_for_image_metadata() {
    let root = tree("capture_stamp");
    write_timed(&root, "00.tif", Some("2026:08:29 10:00:00"));
    write_timed(&root, "01.tif", None);
    let mut controller = AppController::new();
    match common::open_and_settle(&mut controller, &[root]) {
        OpenOutcome::Opened(_) => {}
        OpenOutcome::Question(_, _) => {
            common::answer_and_settle(&mut controller, OpenAs::OneProject);
        }
        OpenOutcome::Mismatch(_) => panic!("opening a folder must never raise a mismatch"),
        OpenOutcome::Refused(_) => panic!("opening two tiny synthetic frames must not be refused"),
    }

    let panel = controller.images_panel();
    assert_eq!(panel.rows[0].trailing_text, "10:00:00");
    assert_eq!(panel.rows[0].capture_stamp, "2026:08:29 10:00:00");
    assert_eq!(panel.rows[1].capture_stamp, "");
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
            path: _,
            chip: _,
            name: _,
            trailing_text: _,
            capture_stamp: _,
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

#[test]
fn confirming_a_reorder_keeps_the_selected_source_visible() {
    let mut controller = controller_with_frames("confirmed_reorder_selection_visibility", 3);
    controller.set_align_algorithm("dummy-align");
    assert!(controller.start_fuse());
    wait_for_settle(&mut controller, Duration::from_secs(120));
    controller.set_images_viewport_height(10.0);
    controller.select_frame(0);
    controller.images_viewport_scrolled(0.0);

    controller.move_frame(0, 2);
    assert!(
        controller.pending_frame_change().is_some(),
        "a checkpoint still requires confirmation before reordering"
    );
    assert!(
        controller.confirm_frame_change(),
        "confirmation applies the deferred reorder and starts the replacement run"
    );
    assert_eq!(controller.frame_pane_label(), "00.tif");
    assert!(
        controller.source_preview_row_visible(2),
        "confirmation keeps the moved selected source visible"
    );
    wait_for_settle(&mut controller, Duration::from_secs(120));
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
    controller.install_appearance(AppearanceSettings {
        presentation: SourcePresentation::List,
    });
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
    // A relink is verified by content hash, so the moved frames are the
    // sealed ones, byte for byte.
    let new_dir = tree("images_relink_new_sources");
    for source in &sources {
        std::fs::copy(source, new_dir.join(source.file_name().unwrap())).unwrap();
    }
    std::fs::remove_dir_all(&old_dir).unwrap();

    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[project_path]),
        OpenOutcome::Opened(_)
    ));
    assert!(controller.images_panel().rows.iter().all(|r| r.unresolved));

    // The open tab itself carries the `Sources missing` failure.
    let tab_id = controller.tab_id_at(0).expect("the opened tab");
    common::locate_and_settle(
        &mut controller,
        focusforge_gui::entry::LocateTarget::Tab(tab_id),
        new_dir,
    );
    let rows = controller.images_panel().rows;
    assert!(rows.iter().all(|r| !r.unresolved), "{rows:?}");
    assert_eq!(
        rows.iter()
            .map(|r| r.trailing_text.as_str())
            .collect::<Vec<_>>(),
        vec!["10:00:00", "10:00:01"]
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
// The sort menu's own open/closed state and a source-row drag.
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
    let mut controller = controller_with_list_frames("source_drag_live_offset", 4);
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
    let mut controller = controller_with_list_frames("source_drag_live_target", 4);
    assert_eq!(controller.drag_target_row(), None, "no drag yet");

    controller.frame_drag_pointer(0, PointerKind::Down, 10.0);
    assert_eq!(
        controller.drag_target_row(),
        Some(0),
        "releasing now would land it back where it started"
    );

    // The same halfway-point rounding `a_source_drag_moves_the_row_by_whole_row_steps`
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
fn presentation_switches_drag_and_scroll_geometry_without_reusing_list_height() {
    let mut controller = controller_with_timed_frames("presentation_drag_geometry", 4);
    controller.set_images_viewport_height(240.0);

    controller.install_appearance(AppearanceSettings {
        presentation: SourcePresentation::ImageList,
    });
    controller.frame_drag_pointer(0, PointerKind::Down, 10.0);
    controller.frame_drag_pointer(0, PointerKind::Move, 190.0);
    assert_eq!(controller.drag_target_row(), Some(2));
    assert_eq!(controller.drag_offset(), 180.0);

    controller.set_source_presentation(SourcePresentation::Large);
    assert_eq!(
        controller.dragging_row(),
        None,
        "a geometry change cancels a live drag"
    );
    assert!(
        controller.images_viewport_scrolled(f32::MAX),
        "the large layout has scrollable content"
    );
    assert_eq!(controller.images_scroll_y(), 560.0);

    controller.frame_drag_pointer(0, PointerKind::Down, 10.0);
    controller.frame_drag_pointer(0, PointerKind::Move, 310.0);
    assert_eq!(controller.drag_target_row(), Some(2));
    assert_eq!(controller.drag_offset(), 300.0);

    controller.frame_drag_pointer(0, PointerKind::Up, 310.0);
    controller.select_frame(3);
    controller.set_source_presentation(SourcePresentation::List);
    assert_eq!(
        controller.images_scroll_y(),
        0.0,
        "the selected final List row remains visible after shrinking geometry"
    );
}

#[test]
fn a_source_drag_moves_the_row_by_whole_row_steps() {
    let mut controller = controller_with_list_frames("source_drag_moves_row", 4);
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
fn releasing_a_source_drag_on_the_same_row_moves_nothing() {
    let mut controller = controller_with_list_frames("source_drag_no_move", 3);
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
// The source row's real Slint-to-Rust wiring, through an actual window -- a
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

/// A point inside the first row's source-name region at window height `y`.
fn source_drag_point(app: &focusforge_gui::AppWindow, y: f32) -> slint::LogicalPosition {
    let area = i_slint_backend_testing::ElementQuery::from_root(app)
        .match_id("FFImageRow::area")
        .find_first()
        .expect("a source row is mounted");
    slint::LogicalPosition::new(area.absolute_position().x + 40.0, y)
}

#[test]
fn row_hover_survives_image_name_timestamp_and_action_surfaces_in_every_presentation() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    use slint::platform::WindowEvent;

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    app.set_list_preview_active(true);
    app.set_list_preview_mode("stacking".into());
    app.set_list_preview_header_text("1 image".into());
    app.set_list_preview_dragging_row(-1);
    app.set_list_preview_drag_target_row(-1);
    app.set_list_preview_drag_offset(0.0);

    let row = focusforge_gui::ImageRowView {
        chip: slint::Color::from_rgb_u8(220, 201, 55),
        name: "IMG_9819-long-filename.tif".into(),
        time_text: "16:42:09".into(),
        capture_stamp: "2026:08:29 16:42:09".into(),
        excluded: false,
        unresolved: false,
        selected: false,
        hovered: false,
        focused: false,
        job_disabled: false,
        preview: slint::Image::default(),
        preview_state: "ready".into(),
    };

    let dispatch = |position| {
        app.window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
    };
    let mounted = |id| {
        ElementQuery::from_root(&app)
            .match_id(id)
            .find_first()
            .expect("the requested row surface is mounted")
    };

    for (presentation, name_slot, exclude_button, remove_button) in [
        (
            "list",
            "FFImageRow::list-name-slot",
            "FFImageRow::list-exclude-button",
            "FFImageRow::list-remove-button",
        ),
        (
            "image-list",
            "FFImageRow::image-list-name-slot",
            "FFImageRow::image-list-exclude-button",
            "FFImageRow::image-list-remove-button",
        ),
        (
            "large",
            "FFImageRow::large-name-slot",
            "FFImageRow::large-exclude-button",
            "FFImageRow::large-remove-button",
        ),
    ] {
        app.set_list_preview_presentation(presentation.into());
        app.set_list_preview_rows(
            std::rc::Rc::new(slint::VecModel::from(vec![row.clone()])).into(),
        );
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));

        let area = mounted("FFImageRow::area");
        let row_center = element_center(&area);
        let image = slint::LogicalPosition::new(area.absolute_position().x + 16.0, row_center.y);
        let name_handle = mounted(name_slot);
        let name = element_center(&name_handle);
        let name_before = (name_handle.absolute_position(), name_handle.size());

        for position in [image, name] {
            dispatch(position);
            assert!(
                ElementQuery::from_root(&app)
                    .match_id(exclude_button)
                    .find_first()
                    .is_some(),
                "{presentation}: row actions remain visible over image and filename"
            );
        }

        let name_after = mounted(name_slot);
        assert_eq!(
            (name_after.absolute_position(), name_after.size()),
            name_before,
            "{presentation}: filename width stays fixed while actions replace the timestamp"
        );

        let exclude = element_center(&mounted(exclude_button));
        let remove = element_center(&mounted(remove_button));
        let timestamp_lane = slint::LogicalPosition::new(exclude.x, exclude.y);
        let gap = slint::LogicalPosition::new((exclude.x + remove.x) / 2.0, exclude.y);
        for position in [timestamp_lane, exclude, gap, remove] {
            dispatch(position);
            assert!(
                ElementQuery::from_root(&app)
                    .match_id(exclude_button)
                    .find_first()
                    .is_some(),
                "{presentation}: action hover keeps the row controls visible"
            );
        }

        dispatch(slint::LogicalPosition::new(0.0, 0.0));
        assert!(
            ElementQuery::from_root(&app)
                .match_id(exclude_button)
                .find_first()
                .is_none(),
            "{presentation}: leaving the row hides its controls"
        );
    }
}

#[test]
fn source_rows_take_keyboard_focus_and_select_the_next_resolved_source() {
    use focusforge_core::timing::{enable_trace_accounting, timing_snapshot};
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    use std::sync::{Arc, Mutex};

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    let controller = Arc::new(Mutex::new(controller_with_list_frames(
        "images_keyboard_selection",
        100,
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
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    };
    settle();

    let press = |key: slint::SharedString| {
        app.window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.clone() });
        app.window()
            .dispatch_event(WindowEvent::KeyReleased { text: key });
    };
    let mut reached_sources = false;
    for _ in 0..40 {
        press(Key::Tab.into());
        press(Key::DownArrow.into());
        settle();
        if controller.lock().unwrap().frame_pane_label() == "01.tif" {
            reached_sources = true;
            break;
        }
    }
    assert!(
        reached_sources,
        "Tab navigation reaches the source keyboard scope before a pointer click"
    );
    press(Key::UpArrow.into());
    settle();
    assert_eq!(controller.lock().unwrap().frame_pane_label(), "00.tif");

    let row = ElementQuery::from_root(&app)
        .match_id("FFImageRow::area")
        .find_first()
        .expect("the first source row is mounted");
    let position = element_center(&row);
    app.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    app.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
    settle();
    assert_eq!(
        controller.lock().unwrap().frame_pane_label(),
        "00.tif",
        "the clicked source is the keyboard-selection anchor"
    );

    press(Key::DownArrow.into());
    settle();
    assert_eq!(controller.lock().unwrap().frame_pane_label(), "01.tif");

    let metadata_reads_before_keys = {
        enable_trace_accounting();
        timing_snapshot()
            .expect("trace accounting is enabled")
            .metadata_reads
    };
    for presentation in [
        SourcePresentation::List,
        SourcePresentation::ImageList,
        SourcePresentation::Large,
    ] {
        {
            let mut controller = controller.lock().unwrap();
            controller.set_source_presentation(presentation);
            controller.select_frame(0);
            controller.images_viewport_scrolled(0.0);
        }
        settle();
        for _ in 0..50 {
            press(Key::DownArrow.into());
            settle();
        }
        assert_eq!(
            controller.lock().unwrap().frame_pane_label(),
            "50.tif",
            "{presentation:?}: Down selects beyond the initial viewport"
        );
        assert!(
            controller.lock().unwrap().images_scroll_y() > 0.0,
            "{presentation:?}: Down keeps the selected row visible"
        );
        let name_slot = match presentation {
            SourcePresentation::List => "FFImageRow::list-name-slot",
            SourcePresentation::ImageList => "FFImageRow::image-list-name-slot",
            SourcePresentation::Large => "FFImageRow::large-name-slot",
        };
        assert!(
            ElementQuery::from_root(&app)
                .match_id(name_slot)
                .match_descendants()
                .match_predicate(|element| {
                    element
                        .accessible_label()
                        .is_some_and(|label| label == "50.tif")
                })
                .find_first()
                .is_some(),
            "{presentation:?}: the selected row's name is mounted inside the source list"
        );

        for _ in 0..50 {
            press(Key::UpArrow.into());
            settle();
        }
        assert_eq!(
            controller.lock().unwrap().frame_pane_label(),
            "00.tif",
            "{presentation:?}: Up returns to the first source"
        );
        assert_eq!(
            controller.lock().unwrap().images_scroll_y(),
            0.0,
            "{presentation:?}: Up restores the first row"
        );
    }
    assert_eq!(
        timing_snapshot()
            .expect("trace accounting is enabled")
            .metadata_reads,
        metadata_reads_before_keys,
        "keyboard visibility changes do not read metadata"
    );
}

#[test]
fn keyboard_selection_skips_missing_reanchors_at_edges_and_stays_read_only_in_editor() {
    let mut controller = controller_with_timed_frames("keyboard_visible_selection", 3);
    controller.set_images_viewport_height(10.0);
    std::fs::remove_file(
        PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("keyboard_visible_selection_frames")
            .join("01.tif"),
    )
    .expect("the middle source exists");
    assert!(!controller.start_fuse(), "the missing source is refused");
    assert!(controller.images_panel().rows[1].unresolved);

    controller.select_frame(0);
    controller.images_viewport_scrolled(0.0);
    controller.select_adjacent_resolved_frame(1);
    assert_eq!(
        controller.frame_pane_label(),
        "02.tif",
        "keyboard selection skips an unresolved source"
    );
    assert!(
        controller.source_preview_row_visible(2),
        "the resolved target is visible after the keyboard step"
    );

    controller.images_viewport_scrolled(0.0);
    controller.select_adjacent_resolved_frame(1);
    assert!(
        controller.source_preview_row_visible(2),
        "a last-row key press reanchors a manually displaced viewport"
    );
    controller.select_frame(0);
    controller.images_viewport_scrolled(f32::MAX);
    controller.select_adjacent_resolved_frame(-1);
    assert!(
        controller.source_preview_row_visible(0),
        "a first-row key press reanchors a manually displaced viewport"
    );

    let mut editor = controller_in_editor_mode("keyboard_visible_selection_editor");
    editor.install_appearance(AppearanceSettings {
        presentation: SourcePresentation::ImageList,
    });
    editor.set_images_viewport_height(10.0);
    editor.select_frame(0);
    editor.select_adjacent_resolved_frame(1);
    assert_eq!(editor.mode(), ShellMode::Editor);
    assert_eq!(editor.source_presentation(), SourcePresentation::ImageList);
    assert!(
        editor.source_preview_row_visible(1),
        "the read-only editor list follows keyboard selection in the Appearance presentation"
    );
}

#[test]
fn editor_source_list_follows_appearance_presentation() {
    let mut editor = controller_in_editor_mode("editor_follows_presentation");
    assert_eq!(editor.mode(), ShellMode::Editor);
    for (presentation, row_height, queues_previews) in [
        (SourcePresentation::List, 26.0, false),
        (SourcePresentation::ImageList, 120.0, true),
        (SourcePresentation::Large, 200.0, true),
    ] {
        editor.install_appearance(AppearanceSettings { presentation });
        editor.set_images_viewport_height(10.0);
        assert_eq!(editor.source_presentation(), presentation);

        editor.select_frame(0);
        editor.images_viewport_scrolled(0.0);
        editor.select_adjacent_resolved_frame(1);
        editor.select_adjacent_resolved_frame(1);
        assert!(
            editor.source_preview_row_visible(2),
            "{presentation:?}: keyboard selection keeps the third row visible"
        );
        assert!(
            !editor.source_preview_row_visible(0),
            "{presentation:?}: the first row has scrolled out of the viewport"
        );
        assert!(
            editor.images_scroll_y() > row_height,
            "{presentation:?}: scroll moved by whole {row_height}px rows"
        );
        assert_eq!(
            editor.source_preview_snapshot().scheduled > 0,
            queues_previews,
            "{presentation:?}: preview requests"
        );
        for (index, row) in editor.images_panel().rows.iter().enumerate() {
            assert_eq!(
                row.trailing_text,
                (index + 1).to_string(),
                "{presentation:?}: legend rows carry the source number"
            );
        }
    }
}

#[test]
fn editor_legend_rows_stay_read_only_in_every_presentation() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    use slint::platform::{PointerEventButton, WindowEvent};
    use std::sync::{Arc, Mutex};

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    let controller = Arc::new(Mutex::new(controller_in_editor_mode(
        "editor_legend_read_only",
    )));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    app.set_project_loaded(true);
    app.set_mode("editor".into());
    app.set_dock_tab("images".into());
    let settle = || {
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    };
    let selected_row = || {
        controller
            .lock()
            .unwrap()
            .images_panel()
            .rows
            .iter()
            .position(|row| row.selected)
    };
    let names = || {
        controller
            .lock()
            .unwrap()
            .images_panel()
            .rows
            .iter()
            .map(|row| row.name.clone())
            .collect::<Vec<_>>()
    };

    for (presentation, name_slot) in [
        (SourcePresentation::List, "FFImageRow::list-name-slot"),
        (
            SourcePresentation::ImageList,
            "FFImageRow::image-list-name-slot",
        ),
        (SourcePresentation::Large, "FFImageRow::large-name-slot"),
    ] {
        {
            let mut controller = controller.lock().unwrap();
            controller.set_source_presentation(presentation);
            controller.select_frame(0);
        }
        settle();
        assert!(
            ElementQuery::from_root(&app)
                .match_id(name_slot)
                .find_first()
                .is_some(),
            "{presentation:?}: the editor rows use the chosen layout"
        );
        let rows = ElementQuery::from_root(&app)
            .match_id("FFImageRow::area")
            .find_all();
        let position = element_center(&rows[1]);
        app.window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        for control in [
            "FFImageRow::list-exclude-button",
            "FFImageRow::list-remove-button",
            "FFImageRow::image-list-exclude-button",
            "FFImageRow::image-list-remove-button",
            "FFImageRow::large-exclude-button",
            "FFImageRow::large-remove-button",
        ] {
            assert!(
                ElementQuery::from_root(&app)
                    .match_id(control)
                    .find_first()
                    .is_none(),
                "{presentation:?}: a hovered editor row shows no {control}"
            );
        }

        let before = names();
        let dragged_to = slint::LogicalPosition::new(position.x, position.y + 150.0);
        app.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        app.window().dispatch_event(WindowEvent::PointerMoved {
            position: dragged_to,
        });
        app.window().dispatch_event(WindowEvent::PointerReleased {
            position: dragged_to,
            button: PointerEventButton::Left,
        });
        settle();
        assert_eq!(names(), before, "{presentation:?}: a drag reorders nothing");
        assert!(
            controller.lock().unwrap().pending_frame_change().is_none(),
            "{presentation:?}: a drag starts no frame change"
        );

        controller.lock().unwrap().select_frame(0);
        settle();
        for event in [
            WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            },
            WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            },
        ] {
            app.window().dispatch_event(event);
        }
        settle();
        assert_eq!(
            selected_row(),
            Some(1),
            "{presentation:?}: a click selects the frame"
        );
    }
}

#[test]
fn selecting_a_hovered_source_keeps_its_hover_actions_mounted() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    use slint::platform::{PointerEventButton, WindowEvent};
    use std::sync::{Arc, Mutex};

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    let controller = Arc::new(Mutex::new(controller_with_list_frames(
        "images_hover_selection_reuse",
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
    let settle = || {
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    };
    settle();

    let row = ElementQuery::from_root(&app)
        .match_id("FFImageRow::area")
        .find_all()
        .into_iter()
        .nth(1)
        .expect("a second resolved source row is mounted");
    let position = element_center(&row);
    app.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    assert!(
        ElementQuery::from_root(&app)
            .match_id("FFImageRow::list-exclude-button")
            .find_first()
            .is_some(),
        "hover exposes the second source actions"
    );

    for event in [
        WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        },
        WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        },
    ] {
        app.window().dispatch_event(event);
    }
    settle();

    assert_eq!(
        controller
            .lock()
            .unwrap()
            .images_panel()
            .rows
            .iter()
            .position(|row| row.selected),
        Some(1)
    );
    assert!(
        ElementQuery::from_root(&app)
            .match_id("FFImageRow::list-exclude-button")
            .find_first()
            .is_some(),
        "selection updates the marker without replacing the hovered row's input tree"
    );

    let toggle = || {
        let button = ElementQuery::from_root(&app)
            .match_id("FFImageRow::list-exclude-button")
            .find_first()
            .expect("the hovered source action remains mounted");
        let position = element_center(&button);
        for event in [
            WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            },
            WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            },
        ] {
            app.window().dispatch_event(event);
        }
        settle();
    };
    toggle();
    assert!(controller.lock().unwrap().images_panel().rows[1].excluded);
    assert!(
        ElementQuery::from_root(&app)
            .match_id("FFImageRow::list-exclude-button")
            .find_first()
            .is_some(),
        "exclude keeps the stationary-pointer action mounted"
    );
    toggle();
    assert!(!controller.lock().unwrap().images_panel().rows[1].excluded);
}

#[test]
fn elided_filename_tooltip_uses_available_window_width_before_wrapping() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    use slint::platform::WindowEvent;

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(720, 480));
    app.set_list_preview_active(true);
    app.set_list_preview_mode("stacking".into());
    app.set_list_preview_presentation("list".into());
    app.set_list_preview_header_text("1 image".into());
    app.set_list_preview_rows(
        std::rc::Rc::new(slint::VecModel::from(vec![focusforge_gui::ImageRowView {
            chip: slint::Color::from_rgb_u8(220, 201, 55),
            name: "IMG_9819_composite-exposure-calibrated-master-source-frame.tif".into(),
            time_text: "16:42:09".into(),
            capture_stamp: "2026:08:29 16:42:09".into(),
            excluded: false,
            unresolved: false,
            selected: false,
            hovered: false,
            focused: false,
            job_disabled: false,
            preview: slint::Image::default(),
            preview_state: "ready".into(),
        }]))
        .into(),
    );

    let name = ElementQuery::from_root(&app)
        .match_id("FFImageRow::list-name-slot")
        .find_first()
        .expect("the clipped filename is mounted");
    app.window().dispatch_event(WindowEvent::PointerMoved {
        position: element_center(&name),
    });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(600));

    let tooltip = ElementQuery::from_root(&app)
        .match_predicate(|element| element.type_name().as_deref() == Some("FFTooltip"))
        .find_first()
        .expect("the built-in Tooltip exposes its custom content after the hover delay");
    assert!(
        tooltip.size().width > 248.0,
        "a filename that fits the app viewport must not inherit the old 248px row cap"
    );
    assert!(
        tooltip.size().height < 40.0,
        "a filename that fits the app viewport remains one readable line"
    );
    assert!(
        tooltip.size().width <= app.window().size().width as f32,
        "the native tooltip popup fits inside the actual app viewport"
    );
    let tooltip_position = tooltip.absolute_position();
    assert!(
        tooltip_position.x >= 0.0 && tooltip_position.y >= 0.0,
        "custom tooltip content must stay within its popup host instead of overflowing at a negative local offset: {tooltip_position:?}"
    );
}

#[test]
fn constrained_filename_tooltip_balances_wrapped_lines_within_the_viewport() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    use slint::platform::WindowEvent;

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(360, 480));
    app.set_list_preview_active(true);
    app.set_list_preview_mode("stacking".into());
    app.set_list_preview_presentation("list".into());
    app.set_list_preview_header_text("1 image".into());
    app.set_list_preview_rows(
        std::rc::Rc::new(slint::VecModel::from(vec![focusforge_gui::ImageRowView {
            chip: slint::Color::from_rgb_u8(220, 201, 55),
            name: "IMG_9819_calibrated-composite-exposure-source-frame-with-reference-metadata.tif"
                .into(),
            time_text: "16:42:09".into(),
            capture_stamp: "2026:08:29 16:42:09".into(),
            excluded: false,
            unresolved: false,
            selected: false,
            hovered: false,
            focused: false,
            job_disabled: false,
            preview: slint::Image::default(),
            preview_state: "ready".into(),
        }]))
        .into(),
    );

    let name = ElementQuery::from_root(&app)
        .match_id("FFImageRow::list-name-slot")
        .find_first()
        .expect("the clipped filename is mounted");
    app.window().dispatch_event(WindowEvent::PointerMoved {
        position: element_center(&name),
    });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(600));

    let tooltip = ElementQuery::from_root(&app)
        .match_predicate(|element| element.type_name().as_deref() == Some("FFTooltip"))
        .find_first()
        .expect("the built-in Tooltip exposes its custom content after the hover delay");
    let viewport_width = app.window().size().width as f32;
    assert!(
        tooltip.size().width <= viewport_width,
        "the constrained tooltip stays inside the usable app width"
    );
    assert!(
        tooltip.size().width > viewport_width / 2.0,
        "wrapped tooltip lines use the available width instead of leaving a tiny final fragment"
    );
    assert!(
        tooltip.size().height >= 40.0,
        "a filename larger than the viewport wraps rather than clipping"
    );
}

#[test]
fn dragging_a_row_by_its_source_reorders_it_through_the_real_window() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    use slint::platform::{PointerEventButton, WindowEvent};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));

    let controller = Arc::new(Mutex::new(controller_with_list_frames(
        "images_source_drag_wire",
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

    let press_pos = source_drag_point(&app, row0_center.y);

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
fn installed_drag_callback_publishes_capture_before_the_next_motion() {
    use slint::ComponentHandle;
    use std::sync::{Arc, Mutex};

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    let controller = Arc::new(Mutex::new(controller_with_list_frames(
        "installed_drag_callback_publication",
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
    focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());

    app.invoke_images_frame_drag_pointer(0, 0, 10.0);
    assert_eq!(controller.lock().unwrap().dragging_row(), Some(0));
    assert!(
        !app.get_images_drag_capture_active(),
        "a stationary press does not capture the whole list"
    );

    app.invoke_images_frame_drag_pointer(0, 2, 10.0 + ROW_HEIGHT);
    assert!(controller.lock().unwrap().drag_capture_active());
    assert!(
        app.get_images_drag_capture_active(),
        "the installed callback publishes capture before another event-loop turn"
    );

    app.invoke_images_frame_drag_pointer(0, 1, 10.0 + ROW_HEIGHT);
    assert_eq!(controller.lock().unwrap().dragging_row(), None);
    assert!(
        !app.get_images_drag_capture_active(),
        "release clears capture before another event-loop turn"
    );
}

#[test]
fn dragging_a_row_by_its_source_floats_a_copy_that_follows_the_cursor() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    use slint::platform::{PointerEventButton, WindowEvent};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));

    let controller = Arc::new(Mutex::new(controller_with_list_frames(
        "images_source_drag_ghost",
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
    let find_capture = || {
        ElementQuery::from_root(&app)
            .match_id("FFImagesPanel::drag-capture")
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
    let row0_top = rows[0].absolute_position().y;
    let last_row_top = rows[4].absolute_position().y;

    let press_pos = source_drag_point(&app, row0_center.y);

    dispatch(WindowEvent::PointerMoved {
        position: press_pos,
    });
    dispatch(WindowEvent::PointerPressed {
        position: press_pos,
        button: PointerEventButton::Left,
    });
    assert!(
        find_ghost().is_none(),
        "a stationary press keeps the row input surface mounted"
    );
    assert!(
        find_capture().is_none(),
        "a stationary press must not install the full-panel capture overlay"
    );
    assert!(
        find_landing_line().is_none(),
        "releasing right after the press would move nothing, so no line is drawn yet"
    );

    let target = slint::LogicalPosition::new(press_pos.x, press_pos.y + 3.0 * ROW_HEIGHT);
    dispatch(WindowEvent::PointerMoved { position: target });
    assert!(
        find_capture().is_some(),
        "moving the pointer installs capture for a drag that leaves its row"
    );
    let at_target = find_ghost()
        .expect("the floating copy stays mounted while the drag continues")
        .absolute_position()
        .y;
    assert!(
        (at_target - row0_top - 3.0 * ROW_HEIGHT).abs() < 1.0,
        "the floating copy must have moved down by the same pixels the cursor did: \
         from {row0_top} to {at_target}"
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

    dispatch(WindowEvent::PointerMoved {
        position: press_pos,
    });
    assert!(
        find_ghost().is_none(),
        "returning to the press position removes the floating copy"
    );
    assert!(
        find_capture().is_some(),
        "returning to the press position keeps capture for the active gesture"
    );
    assert!(
        find_landing_line().is_none(),
        "returning to the press position clears the landing line"
    );

    dispatch(WindowEvent::PointerMoved { position: target });
    assert!(
        find_ghost().is_some(),
        "the same press resumes its floating copy after returning to origin"
    );
    assert!(
        find_capture().is_some(),
        "the same press resumes capture after returning to origin"
    );
    assert_eq!(controller.lock().unwrap().dragging_row(), Some(0));

    dispatch(WindowEvent::PointerReleased {
        position: target,
        button: PointerEventButton::Left,
    });
    assert!(
        find_ghost().is_none(),
        "the floating copy stays absent after release"
    );
    assert!(
        find_landing_line().is_none(),
        "the landing line stays absent after release"
    );
    assert!(
        find_capture().is_none(),
        "release removes the active drag capture"
    );
    assert_eq!(controller.lock().unwrap().dragging_row(), None);
    let names: Vec<String> = controller
        .lock()
        .unwrap()
        .images_panel()
        .rows
        .into_iter()
        .map(|row| row.name)
        .collect();
    assert_eq!(
        names,
        vec!["01.tif", "02.tif", "03.tif", "00.tif", "04.tif"],
        "a resumed drag reorders on its release without needing a second press"
    );
}

#[test]
fn dragging_at_images_viewport_edges_auto_scrolls_and_keeps_the_selected_source_visible() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    use slint::platform::{PointerEventButton, WindowEvent};
    use std::sync::{Arc, Mutex};

    init_window_backend();
    for (presentation, at_bottom) in [
        (SourcePresentation::List, false),
        (SourcePresentation::List, true),
        (SourcePresentation::ImageList, false),
        (SourcePresentation::ImageList, true),
        (SourcePresentation::Large, false),
        (SourcePresentation::Large, true),
    ] {
        let app = focusforge_gui::AppWindow::new().unwrap();
        app.window().set_size(slint::PhysicalSize::new(1440, 900));
        let mut loaded = controller_with_list_frames(
            &format!("images_drag_edge_{presentation:?}_{at_bottom}"),
            40,
        );
        loaded.set_source_presentation(presentation);
        let row_height = match presentation {
            SourcePresentation::List => ROW_HEIGHT,
            SourcePresentation::ImageList => 120.0,
            SourcePresentation::Large => 200.0,
        };
        let controller = Arc::new(Mutex::new(loaded));
        let _installed = focusforge_gui::bridge::install(
            &app,
            controller.clone(),
            common::empty_canvas(),
            std::rc::Rc::new(std::cell::RefCell::new(None)),
        );
        app.set_project_loaded(true);
        app.set_dock_tab("images".into());
        let settle = || {
            i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
            focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());
            i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        };
        let dispatch = |event: WindowEvent| {
            app.window().dispatch_event(event);
            settle();
        };
        settle();
        if !at_bottom {
            let mut controller = controller.lock().unwrap();
            controller.select_frame(20);
            controller.images_viewport_scrolled(20.0 * row_height);
            drop(controller);
            settle();
        }

        let list = mounted(&app, "FFImagesPanel::list");
        let expected_dragged_name = {
            let controller = controller.lock().unwrap();
            let first_visible = (controller.images_scroll_y() / row_height).floor() as usize;
            if !at_bottom {
                assert!(
                    controller.images_scroll_y() > 0.0,
                    "{presentation:?}: the top-edge case starts away from the content bound"
                );
            }
            controller.images_panel().rows[first_visible].name.clone()
        };
        let selected_name_before = controller
            .lock()
            .unwrap()
            .images_panel()
            .rows
            .into_iter()
            .find(|row| row.selected)
            .expect("one selected source before drag")
            .name;
        let edge_y = if at_bottom {
            list.absolute_position().y + list.size().height - 1.0
        } else {
            list.absolute_position().y + 1.0
        };
        let source = source_drag_point(
            &app,
            list.absolute_position().y + (row_height / 2.0).min(list.size().height / 2.0),
        );
        dispatch(WindowEvent::PointerMoved { position: source });
        dispatch(WindowEvent::PointerPressed {
            position: source,
            button: PointerEventButton::Left,
        });
        let (dragged_name, order_before) = {
            let controller = controller.lock().unwrap();
            let dragged = controller
                .dragging_row()
                .expect("the source press starts a drag");
            (
                controller.images_panel().rows[dragged].name.clone(),
                controller
                    .images_panel()
                    .rows
                    .into_iter()
                    .map(|row| row.name)
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            dragged_name, expected_dragged_name,
            "{presentation:?}: the visible source receives the drag press"
        );
        let edge = slint::LogicalPosition::new(source.x, edge_y);
        dispatch(WindowEvent::PointerMoved { position: edge });
        assert!(
            ElementQuery::from_root(&app)
                .match_id("FFImagesPanel::drag-capture")
                .find_first()
                .is_some(),
            "{presentation:?}: a moved source drag owns capture at the viewport edge"
        );
        let before = controller.lock().unwrap().images_scroll_y();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(90));
        let after = controller.lock().unwrap().images_scroll_y();
        assert!(
            if at_bottom {
                after > before
            } else {
                after < before
            },
            "{presentation:?}: a held edge drag scrolls without another pointer move"
        );
        for _ in 0..200 {
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(30));
        }
        let bounded = controller.lock().unwrap().images_scroll_y();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(90));
        assert_eq!(
            controller.lock().unwrap().images_scroll_y(),
            bounded,
            "{presentation:?}: edge scrolling stops at the content bound"
        );
        let target = controller
            .lock()
            .unwrap()
            .drag_target_row()
            .expect("the held drag retains a live target");

        dispatch(WindowEvent::PointerReleased {
            position: edge,
            button: PointerEventButton::Left,
        });
        let guard = controller.lock().unwrap();
        assert_eq!(
            guard.dragging_row(),
            None,
            "{presentation:?}: release clears drag"
        );
        let selected = guard
            .images_panel()
            .rows
            .iter()
            .position(|row| row.selected)
            .expect("one selected source");
        let rows = guard.images_panel().rows;
        assert_ne!(
            rows.iter().map(|row| &row.name).collect::<Vec<_>>(),
            order_before.iter().collect::<Vec<_>>(),
            "{presentation:?}: edge release reorders the dragged source"
        );
        assert_eq!(
            rows[target].name, dragged_name,
            "{presentation:?}: release places the dragged source at its live target"
        );
        assert_eq!(
            rows[selected].name, selected_name_before,
            "{presentation:?}: reordering preserves the previous selection identity"
        );
        let list = mounted(&app, "FFImagesPanel::list");
        let list_top = list.absolute_position().y;
        let list_bottom = list_top + list.size().height;
        let moved_top = list_top + target as f32 * row_height + app.get_images_list_viewport_y();
        let moved_bottom = moved_top + row_height;
        if list.size().height >= row_height {
            assert!(
                moved_top >= list_top - 1.0 && moved_bottom <= list_bottom + 1.0,
                "{presentation:?}: release fully keeps the moved source in view"
            );
        } else {
            assert!(
                moved_top < list_bottom && moved_bottom > list_top,
                "{presentation:?}: a short viewport keeps the moved source at its bounded best fit"
            );
        }
        let scroll_after_release = guard.images_scroll_y();
        let order_after_release: Vec<_> = rows.into_iter().map(|row| row.name).collect();
        drop(guard);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(90));
        let controller = controller.lock().unwrap();
        assert_eq!(
            controller.images_scroll_y(),
            scroll_after_release,
            "{presentation:?}: release stops edge auto-scroll"
        );
        assert_eq!(
            controller
                .images_panel()
                .rows
                .into_iter()
                .map(|row| row.name)
                .collect::<Vec<_>>(),
            order_after_release,
            "{presentation:?}: release leaves order stable while the timer continues"
        );
    }
}

#[test]
fn a_stationary_source_press_near_an_edge_does_not_auto_scroll_or_reorder() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    use slint::platform::{PointerEventButton, WindowEvent};
    use std::sync::{Arc, Mutex};

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    let loaded = controller_with_list_frames("images_stationary_edge_press", 40);
    let controller = Arc::new(Mutex::new(loaded));
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
    app.set_project_loaded(true);
    app.set_dock_tab("images".into());
    let settle = || {
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        focusforge_gui::bridge::push_images_panel(&app, &controller.lock().unwrap());
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    };
    settle();
    {
        let mut controller = controller.lock().unwrap();
        controller.select_frame(10);
        controller.images_viewport_scrolled(10.0 * ROW_HEIGHT);
    }
    settle();
    let expected_clicked_name = {
        let controller = controller.lock().unwrap();
        assert!(
            controller.images_scroll_y() > 0.0,
            "the stationary press starts away from the content bound"
        );
        let first_visible = (controller.images_scroll_y() / ROW_HEIGHT).floor() as usize;
        controller.images_panel().rows[first_visible].name.clone()
    };
    let names_before: Vec<_> = controller
        .lock()
        .unwrap()
        .images_panel()
        .rows
        .into_iter()
        .map(|row| row.name)
        .collect();
    let list = mounted(&app, "FFImagesPanel::list");
    let press = source_drag_point(&app, list.absolute_position().y + 1.0);
    app.window()
        .dispatch_event(WindowEvent::PointerMoved { position: press });
    app.window().dispatch_event(WindowEvent::PointerPressed {
        position: press,
        button: PointerEventButton::Left,
    });
    settle();
    let scroll_before = controller.lock().unwrap().images_scroll_y();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(90));
    assert_eq!(
        controller.lock().unwrap().images_scroll_y(),
        scroll_before,
        "a stationary press does not auto-scroll"
    );
    assert!(
        ElementQuery::from_root(&app)
            .match_id("FFImagesPanel::drag-ghost")
            .find_first()
            .is_none(),
        "a stationary press does not mount a drag ghost"
    );
    app.window().dispatch_event(WindowEvent::PointerReleased {
        position: press,
        button: PointerEventButton::Left,
    });
    settle();
    let controller = controller.lock().unwrap();
    assert_eq!(controller.dragging_row(), None);
    assert_eq!(
        controller
            .images_panel()
            .rows
            .into_iter()
            .map(|row| row.name)
            .collect::<Vec<_>>(),
        names_before,
        "a stationary press cannot reorder sources"
    );
    assert_eq!(
        controller.frame_pane_label(),
        expected_clicked_name,
        "a stationary press still selects the visible source it clicks"
    );
}

#[test]
fn closing_a_dragged_tab_retires_its_edge_scroll_state() {
    let mut controller = controller_with_list_frames("images_dragged_tab_close", 40);
    controller.set_images_viewport_height(2.0 * ROW_HEIGHT);

    let other = tree("images_dragged_tab_close_other");
    write_timed(&other, "other.tif", Some("2026:08:29 10:00:00"));
    assert!(matches!(
        common::open_and_settle(&mut controller, &[other]),
        OpenOutcome::Opened(_)
    ));
    controller.select_project(0);
    controller.images_viewport_scrolled(10.0 * ROW_HEIGHT);
    let edge = 12.0 * ROW_HEIGHT - 1.0;
    controller.frame_drag_pointer(10, PointerKind::Down, 10.0 * ROW_HEIGHT + 1.0);
    controller.frame_drag_pointer(10, PointerKind::Move, edge);
    assert!(controller.drag_capture_active());
    assert!(controller.advance_frame_drag_autoscroll());

    controller.close_project_at(0, Instant::now());

    assert_eq!(
        controller.dragging_row(),
        None,
        "closing a dragged tab clears its source gesture"
    );
    assert!(
        !controller.advance_frame_drag_autoscroll(),
        "the timer cannot scroll a replacement tab after the dragged one closed"
    );
}

#[test]
fn source_drag_retirement_paths_leave_no_stale_edge_scroll() {
    let begin_drag = |controller: &mut AppController| {
        controller.frame_drag_pointer(0, PointerKind::Down, 1.0);
        controller.frame_drag_pointer(0, PointerKind::Move, ROW_HEIGHT + 1.0);
        assert!(controller.drag_capture_active());
    };
    let assert_retired = |controller: &mut AppController, reason: &str| {
        assert_eq!(controller.dragging_row(), None, "{reason}");
        assert!(
            !controller.advance_frame_drag_autoscroll(),
            "{reason}: a retired drag cannot scroll"
        );
    };

    let mut released = controller_with_list_frames("images_drag_release", 3);
    begin_drag(&mut released);
    released.frame_drag_pointer(0, PointerKind::Up, ROW_HEIGHT + 1.0);
    assert_retired(&mut released, "release retires the drag");

    let mut removed = controller_with_list_frames("images_drag_source_retirement", 3);
    begin_drag(&mut removed);
    removed.remove_frame(0);
    assert_retired(&mut removed, "removing a source retires the drag");

    let mut switched = controller_with_list_frames("images_drag_tab_switch", 3);
    let other = tree("images_drag_tab_switch_other");
    write_timed(&other, "other.tif", Some("2026:08:29 10:00:00"));
    assert!(matches!(
        common::open_and_settle(&mut switched, &[other]),
        OpenOutcome::Opened(_)
    ));
    switched.select_project(0);
    begin_drag(&mut switched);
    switched.select_project(1);
    assert_retired(&mut switched, "switching tabs retires the drag");

    let mut mode_changed = controller_in_editor_mode("images_drag_mode_change");
    assert!(mode_changed.set_mode(ShellMode::Stacking));
    begin_drag(&mut mode_changed);
    assert!(mode_changed.set_mode(ShellMode::Editor));
    assert_retired(&mut mode_changed, "changing mode retires the drag");

    let mut running = controller_with_list_frames("images_drag_job_start", 3);
    begin_drag(&mut running);
    running.set_align_algorithm("dummy-align");
    assert!(running.start_fuse());
    assert_retired(&mut running, "starting a job retires the drag");
    wait_for_settle(&mut running, Duration::from_secs(120));
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

    let mut controller = controller_with_frames("images_panel_disable_is_whole_panel", 4);
    controller.install_appearance(AppearanceSettings {
        presentation: SourcePresentation::List,
    });
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
        "every row inherits the panel's dim: {:?}",
        row_opacities()
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

    let mut loaded = controller_with_frames("job_disables_images_and_settings", 3);
    loaded.install_appearance(AppearanceSettings {
        presentation: SourcePresentation::List,
    });
    let controller = Arc::new(Mutex::new(loaded));
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
    let gutter = source_drag_point(&app, element_center(&rows[0]).y);
    app.window()
        .dispatch_event(WindowEvent::PointerMoved { position: gutter });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(0));
    assert!(
        ElementQuery::from_root(&app)
            .match_id("FFImageRow::list-exclude-button")
            .find_first()
            .is_some(),
        "hovering a row's source-name region shows its controls before any job runs"
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
            .match_id("FFImageRow::list-exclude-button")
            .find_first()
            .is_none(),
        "no row controls may show while this tab's own job is running, even hovered"
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
    controller
        .lock()
        .unwrap()
        .install_appearance(AppearanceSettings {
            presentation: SourcePresentation::List,
        });
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

    let selected = || {
        controller
            .lock()
            .unwrap()
            .images_panel()
            .rows
            .iter()
            .position(|r| r.selected)
    };
    let metadata_reads_before_wheel = counters().0;
    for presentation in [
        SourcePresentation::List,
        SourcePresentation::ImageList,
        SourcePresentation::Large,
    ] {
        {
            let mut controller = controller.lock().unwrap();
            controller.set_source_presentation(presentation);
            controller.select_frame(0);
            controller.images_viewport_scrolled(0.0);
        }
        settle();
        let list_center = element_center(&mounted(&app, "FFImagesPanel::list"));
        dispatch(WindowEvent::PointerScrolled {
            position: list_center,
            delta_x: 0.0,
            delta_y: -100_000.0,
        });
        assert_eq!(
            selected(),
            Some(99),
            "{presentation:?}: wheel selects the last row"
        );
        assert!(
            controller.lock().unwrap().images_scroll_y() > 0.0,
            "{presentation:?}: wheel keeps its selected row visible"
        );
        let name_slot = match presentation {
            SourcePresentation::List => "FFImageRow::list-name-slot",
            SourcePresentation::ImageList => "FFImageRow::image-list-name-slot",
            SourcePresentation::Large => "FFImageRow::large-name-slot",
        };
        assert!(
            i_slint_backend_testing::ElementQuery::from_root(&app)
                .match_id(name_slot)
                .match_descendants()
                .match_predicate(|element| {
                    element
                        .accessible_label()
                        .is_some_and(|label| label == "99.tif")
                })
                .find_first()
                .is_some(),
            "{presentation:?}: the selected row's name is mounted inside the source list"
        );

        dispatch(WindowEvent::PointerScrolled {
            position: element_center(&mounted(&app, "FFImagesPanel::list")),
            delta_x: 0.0,
            delta_y: 100_000.0,
        });
        assert_eq!(
            selected(),
            Some(0),
            "{presentation:?}: wheel selects the first row"
        );
        assert_eq!(
            controller.lock().unwrap().images_scroll_y(),
            0.0,
            "{presentation:?}: returning selection restores the first row"
        );
    }
    assert_eq!(
        counters().0,
        metadata_reads_before_wheel,
        "wheel selection visibility does not read metadata"
    );
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

    assert_eq!(counters().0, before.0, "scrolling reads no metadata");
    assert!(
        counters().1 >= before.1,
        "scrolling republishes the visible model so cached previews become native images"
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
    let old_panel = controller.lock().unwrap().images_panel();
    let removed = row_updates(&|c| c.remove_frame(98));
    let new = shown();
    let new_panel = controller.lock().unwrap().images_panel();
    assert_eq!(new.len(), old.len() - 1);
    let changed = new_panel
        .rows
        .iter()
        .zip(&old_panel.rows)
        .filter(|(new, old)| new != old)
        .count();
    assert_eq!(
        removed,
        (changed + 1) as u64,
        "the bridge updates each changed source row and trims the removed tail"
    );
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
fn a_source_drag_survives_leaving_the_viewport_and_row_recycling() {
    use i_slint_backend_testing::ElementQuery;
    use slint::ComponentHandle;
    use slint::platform::{PointerEventButton, WindowEvent};
    use std::sync::{Arc, Mutex};

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    let controller = Arc::new(Mutex::new(controller_with_list_frames(
        "images_source_drag_recycling",
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
    let at = |y: f32| source_drag_point(&app, y);
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
    assert_eq!(
        labelled(&app, "01.tif"),
        1,
        "returning to origin removes the floating copy"
    );
    assert!(
        ElementQuery::from_root(&app)
            .match_id("FFImagesPanel::drag-capture")
            .find_first()
            .is_some(),
        "a drag that already moved retains capture through a zero offset"
    );

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
            .join("images_source_drag_recycling_frames")
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
        .match_id("FFModalPanel::panel")
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

/// Confirming a frame-set change in the window starts a re-fuse that settles
/// through the window's own job timer.
#[test]
fn confirming_a_frame_change_in_the_window_settles_its_refuse() {
    use std::sync::{Arc, Mutex};

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    let mut controller = controller_with_frames("frame_change_refuse_window", 3);
    controller.set_align_algorithm("dummy-align");
    assert!(controller.start_fuse());
    common::wait_for_settle(&mut controller, Duration::from_secs(120)).expect("the fuse settles");
    controller.set_mode(ShellMode::Stacking);
    controller.remove_frame(1);
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
    assert!(app.get_frame_warning_open());

    app.invoke_frame_warning_confirmed();
    assert!(controller.lock().unwrap().any_job_running());
    let start = Instant::now();
    while controller.lock().unwrap().any_job_running() {
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "the window's job timer never settled the re-fuse"
        );
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(controller.lock().unwrap().checkpoint_count(), Some(2));
}

#[test]
fn source_row_actions_and_panel_header_buttons_carry_their_names() {
    use i_slint_backend_testing::{AccessibleRole, ElementQuery};
    use slint::ComponentHandle;

    init_window_backend();
    let app = focusforge_gui::AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    app.set_list_preview_active(true);
    app.set_list_preview_mode("stacking".into());
    app.set_list_preview_dragging_row(-1);
    app.set_list_preview_drag_target_row(-1);
    let named = |id: &str| {
        let handle = ElementQuery::from_root(&app)
            .match_id(id)
            .find_first()
            .unwrap_or_else(|| panic!("{id} is mounted"));
        assert_eq!(
            handle.accessible_role(),
            Some(AccessibleRole::Button),
            "{id}"
        );
        handle.accessible_label().unwrap_or_default().to_string()
    };

    for presentation in ["list", "image-list", "large"] {
        app.set_list_preview_presentation(presentation.into());
        for (excluded, exclude_label) in [(false, "Exclude"), (true, "Include")] {
            let row = focusforge_gui::ImageRowView {
                chip: slint::Color::from_rgb_u8(220, 201, 55),
                name: "IMG_9819.tif".into(),
                time_text: "16:42:09".into(),
                capture_stamp: "2026:08:29 16:42:09".into(),
                excluded,
                unresolved: false,
                selected: false,
                hovered: true,
                focused: false,
                job_disabled: false,
                preview: slint::Image::default(),
                preview_state: "ready".into(),
            };
            app.set_list_preview_rows(std::rc::Rc::new(slint::VecModel::from(vec![row])).into());
            i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
            assert_eq!(
                named(&format!("FFImageRow::{presentation}-exclude-button")),
                exclude_label,
                "{presentation}, excluded {excluded}"
            );
            assert_eq!(
                named(&format!("FFImageRow::{presentation}-remove-button")),
                "Remove"
            );
        }
    }
    assert_eq!(named("FFImagesPanel::add-button"), "Add images");
    assert_eq!(named("FFImagesPanel::sort-button"), "Sort");
}
