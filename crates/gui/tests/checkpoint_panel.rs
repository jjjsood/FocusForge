// SPDX-License-Identifier: GPL-3.0-or-later

//! The checkpoint dock panel: the view model in `checkpoints.rs` merged with
//! selection and materialization behind `AppController`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{Datelike, Days, Local, NaiveDate};
use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter};
use focusforge_core::params::ParamValues;
use focusforge_core::project::{
    HeldCheckpoint, Metrics, Op, Point, Project, ProjectError, SealRequest, ZMapStorage,
    append_ops_at, append_ops_onto, render_checkpoint, seal_at,
};
use focusforge_core::zmap::{ZMapInfo, ZMapReader, ZMapWriter};
use focusforge_gui::bridge::Canvas;
use focusforge_gui::canvas::CanvasFeed;
use focusforge_gui::input_map::PointerKind;
use focusforge_gui::{AppController, AppWindow, OpenOutcome, ShellMode, checkpoints, job};
use i_slint_backend_testing::ElementQuery;
use slint::ComponentHandle;

/// Writes a 47-checkpoint project spanning several local days to the repo's
/// own `.scratch/checkpoint-long-history/long.ffp`, for a manual scroll
/// check of the checkpoint list's four-row viewport. Not run by default:
/// `cargo test -p focusforge-gui --test checkpoint_panel
/// writes_a_long_history_project -- --ignored`.
#[test]
#[ignore]
fn writes_a_long_history_project() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(".scratch/checkpoint-long-history");
    fs::create_dir_all(&dir).expect("scratch dir must create");
    let project_path = dir.join("long.ffp");
    let _ = fs::remove_file(&project_path);
    write_history(&project_path, 47, 86_400 / 4);
}

/// A press on the drawn scrollbar thumb of a freshly installed window grabs
/// it instead of jumping the list: the list's fixed viewport height must
/// reach the controller at install, since a height that never changes never
/// fires a change handler.
#[test]
fn a_fresh_window_press_on_the_drawn_thumb_does_not_jump() {
    i_slint_backend_testing::init_no_event_loop();
    let app = AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));

    // Ten same-day rows: 400 px of content in the 160 px list, so the drawn
    // thumb spans [0, 64).
    let project_path = common::tree("checkpoint_panel_fresh_thumb").join("history.ffp");
    write_history(&project_path, 10, 60);
    let controller = Arc::new(Mutex::new(AppController::new()));
    common::open_and_settle(&mut controller.lock().unwrap(), &[project_path]);
    let _installed =
        focusforge_gui::bridge::install(&app, controller.clone(), common::empty_canvas());

    let mut c = controller.lock().unwrap();
    assert_eq!(c.checkpoints_panel().content_height, 400.0);
    c.checkpoints_scrollbar_pointer(PointerKind::Down, 40.0);
    assert_eq!(
        c.checkpoints_scroll_y(),
        0.0,
        "a press inside the drawn thumb must not move the list"
    );
    c.checkpoints_scrollbar_pointer(PointerKind::Up, 40.0);

    // The wheel stops with the last row at the list's bottom edge.
    c.scroll_checkpoints_panel(10_000.0);
    assert_eq!(c.checkpoints_scroll_y(), 240.0);
}

/// Seals a one-checkpoint project at `project_path` over a 4x4 staged
/// composite and appends `count - 1` more, `step` seconds apart.
fn write_history(project_path: &Path, count: i64, step: i64) {
    let start = 1_756_468_800; // an arbitrary local noon
    let times: Vec<i64> = (0..count).map(|i| start + i * step).collect();
    write_chain(project_path, &times);
}

/// Seals a sourceless project at `project_path` over a 4x4 staged composite
/// at `times[0]`, then appends one op-bearing child per later time, each onto
/// the previous head.
fn write_chain(project_path: &Path, times: &[i64]) {
    let dir = project_path.parent().expect("project path has a parent");
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
    .expect("composite must stage");
    writer
        .write_tile(full, &[0u16; 4 * 4 * 3])
        .expect("composite tile must write");
    writer.finish().expect("composite must finish");
    let mut map =
        ZMapWriter::create(&zmap, ZMapInfo::new(4, 4, 1).unwrap()).expect("zmap must stage");
    map.write_tile(full, &[0u16; 4 * 4])
        .expect("zmap tile must write");
    map.finish().expect("zmap must finish");

    seal_at(
        SealRequest {
            project_path,
            staged_composite: &composite,
            staged_zmap: &zmap,
            sources: &[],
            algorithm: "laplacian-pyramid",
            params: &ParamValues::new(),
            align: None,
            crop: None,
            composite_description: "staged".to_string(),
            label: None,
            metrics: Metrics::default(),
        },
        times[0],
    )
    .expect("root checkpoint must seal");

    for (i, &at) in times.iter().enumerate().skip(1) {
        let op = Op::BrushStroke {
            source_id: 0,
            path: vec![Point { x: 1, y: 1 }],
            radius: 0.5,
            softness: 0.0,
        };
        append_ops_at(project_path, vec![op], None, at, ZMapStorage::Bounded)
            .unwrap_or_else(|err| panic!("checkpoint {i} must append: {err}"));
    }
}

// ---------------------------------------------------------------------
// Rows, order and count.
// ---------------------------------------------------------------------

/// The saved rows are `History::all()`'s ids in its order over a branched
/// history -- two children of the root and a grandchild, all three at one
/// `created_at`, so both the depth and the id tie-break decide -- the tab's
/// count equals the row count, and a held checkpoint merges by time, first
/// on a tie.
#[test]
fn rows_match_the_core_listing() {
    // `build_edited`'s root at t0 and its child at t0 + 60; a grandchild and
    // a second child of the root join at t0 + 60. Each paints a different
    // pixel: a child's id derives from its parent, map and time.
    let edited = build_edited("checkpoint_panel_rows");
    let project_path = edited.project.clone();
    let t0 = 1_756_468_800;
    for (parent, x) in [(&edited.edited, 4), (&edited.root, 0)] {
        let op = Op::BrushStroke {
            source_id: 2,
            path: vec![Point { x, y: 0 }],
            radius: 0.5,
            softness: 0.0,
        };
        append_ops_onto(
            &project_path,
            parent,
            vec![op],
            None,
            t0 + 60,
            ZMapStorage::Bounded,
        )
        .unwrap();
    }

    let mut controller = AppController::new();
    common::open_and_settle(&mut controller, std::slice::from_ref(&project_path));
    let panel = controller.checkpoints_panel();
    let project = Project::open(&project_path).unwrap();
    let saved = project.history().all();
    let listing: Vec<String> = saved.iter().map(|c| c.id.clone()).collect();
    assert_eq!(listing.len(), 4);
    assert_eq!(row_ids(&panel), listing, "rows must follow History::all()");
    assert_eq!(controller.checkpoint_count(), Some(panel.row_count));
    assert_eq!(panel.row_count, 4);

    // A held checkpoint at the newest saved time sorts first; one between
    // the root and its children sorts between them.
    let today = checkpoints::today();
    let tied = checkpoints::build_model(&saved, &[held_at(t0 + 60)], today, None, 0.0);
    assert_eq!(
        row_ids(&tied)[0],
        "held:0",
        "a held row goes first on a tie"
    );
    let between = checkpoints::build_model(&saved, &[held_at(t0 + 30)], today, None, 0.0);
    let ids = row_ids(&between);
    assert_eq!(ids[..3], listing[..3]);
    assert_eq!(ids[3], "held:0");
    assert_eq!(ids[4], listing[3]);
}

fn row_ids(model: &checkpoints::CheckpointsModel) -> Vec<String> {
    model
        .entries
        .iter()
        .filter_map(|entry| entry.id.clone())
        .collect()
}

fn held_at(created_at: i64) -> HeldCheckpoint {
    HeldCheckpoint {
        composite: PathBuf::from("composite.tif"),
        zmap: PathBuf::from("zmap.tif"),
        width: 4,
        height: 4,
        sources: Vec::new(),
        algorithm: "laplacian-pyramid".to_string(),
        params: ParamValues::new(),
        align: None,
        crop: None,
        composite_description: String::new(),
        metrics: Metrics::default(),
        created_at,
    }
}

fn dab() -> Op {
    Op::BrushStroke {
        source_id: 0,
        path: vec![Point { x: 1, y: 1 }],
        radius: 0.5,
        softness: 0.0,
    }
}

// ---------------------------------------------------------------------
// Selection and materialize.
// ---------------------------------------------------------------------

const MW: u32 = 5;
const MH: u32 = 3;
const FRAMES: usize = 3;

/// A 5 x 3 project over three real source frames: a fused root and one
/// op-bearing child, the head.
struct Edited {
    project: PathBuf,
    sources: Vec<PathBuf>,
    root: String,
    edited: String,
}

fn full() -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: MW,
        height: MH,
    }
}

fn write_rgb(path: &Path, samples: &[u16]) {
    let mut writer = TiffWriter::create(
        path,
        TiffSpec {
            width: MW,
            height: MH,
            samples_per_pixel: 3,
            description: Some("staged".to_string()),
        },
    )
    .unwrap();
    writer.write_tile(full(), samples).unwrap();
    writer.finish().unwrap();
}

fn frame_samples(seed: usize) -> Vec<u16> {
    (0..(MW * MH * 3) as usize)
        .map(|j| ((j * 131 + seed * 977) % 65_535) as u16)
        .collect()
}

fn build_edited(name: &str) -> Edited {
    let dir = common::tree(name);
    let sources: Vec<PathBuf> = (0..FRAMES)
        .map(|i| {
            let path = dir.join(format!("frame-{i:02}.tif"));
            write_rgb(&path, &frame_samples(i));
            path
        })
        .collect();
    let composite = dir.join("composite.tif");
    write_rgb(&composite, &vec![9000; (MW * MH * 3) as usize]);
    let zmap = dir.join("zmap.tif");
    let indices: Vec<u16> = (0..(MW * MH) as usize)
        .map(|i| (i % FRAMES) as u16)
        .collect();
    let mut map = ZMapWriter::create(&zmap, ZMapInfo::new(MW, MH, FRAMES as u32).unwrap()).unwrap();
    map.write_tile(full(), &indices).unwrap();
    map.finish().unwrap();

    let project = dir.join("edited.ffp");
    let t0 = 1_756_468_800;
    let root = seal_at(
        SealRequest {
            project_path: &project,
            staged_composite: &composite,
            staged_zmap: &zmap,
            sources: &sources,
            algorithm: "laplacian-pyramid",
            params: &ParamValues::new(),
            align: None,
            crop: None,
            composite_description: "staged".to_string(),
            label: None,
            metrics: Metrics::default(),
        },
        t0,
    )
    .unwrap()
    .checkpoint;
    let edited = append_ops_onto(
        &project,
        &root,
        vec![Op::BrushStroke {
            source_id: 1,
            path: vec![Point { x: 2, y: 1 }],
            radius: 1.5,
            softness: 0.0,
        }],
        None,
        t0 + 60,
        ZMapStorage::Bounded,
    )
    .unwrap()
    .checkpoint;
    Edited {
        project,
        sources,
        root,
        edited,
    }
}

/// `render_checkpoint`'s own output for `id`, as samples.
fn rendered(project: &Path, id: &str, out: &Path) -> Vec<u16> {
    let mut project = Project::open(project).unwrap();
    render_checkpoint(&mut project, id, out).unwrap();
    samples(out)
}

fn samples(path: &Path) -> Vec<u16> {
    let stack = TiffStack::open_files(&[path.to_path_buf()]).unwrap();
    let (width, height) = stack.dimensions();
    stack
        .read_tile(
            0,
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
        )
        .unwrap()
        .samples
}

fn read_zmap(path: &Path) -> Vec<u16> {
    let reader = ZMapReader::open(path).unwrap();
    let info = reader.info();
    let whole = Rect {
        x: 0,
        y: 0,
        width: info.width(),
        height: info.height(),
    };
    let mut indices = vec![0u16; info.tile_len(whole)];
    reader.read_tile_into(whole, &mut indices).unwrap();
    indices
}

fn row_index(controller: &AppController, id: &str) -> usize {
    controller
        .checkpoints_panel()
        .entries
        .iter()
        .position(|entry| entry.id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("no row for {id}"))
}

fn selected_rows(controller: &AppController) -> Vec<usize> {
    controller
        .checkpoints_panel()
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.selected)
        .map(|(i, _)| i)
        .collect()
}

fn loaded_composite(feed: CanvasFeed) -> PathBuf {
    match feed {
        CanvasFeed::Load { composite, .. } => composite,
        CanvasFeed::Keep => panic!("expected planes to load, got Keep"),
        CanvasFeed::Clear => panic!("expected planes to load, got Clear"),
    }
}

fn wait_for_render(controller: &mut AppController) {
    let start = Instant::now();
    while !controller.settle_checkpoint_render() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the checkpoint render did not settle"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn scratch_controller(name: &str) -> AppController {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree(&format!("{name}_scratch")));
    controller
}

#[test]
fn selecting_a_row_materializes_and_marks_it() {
    let project = build_edited("checkpoint_panel_select");
    let out = common::tree("checkpoint_panel_select_expected");
    let expected_root = rendered(&project.project, &project.root, &out.join("root.tif"));
    let expected_edited = rendered(&project.project, &project.edited, &out.join("edited.tif"));
    assert_ne!(expected_root, expected_edited);

    let mut controller = scratch_controller("checkpoint_panel_select");
    common::open_and_settle(&mut controller, std::slice::from_ref(&project.project));
    let root_row = row_index(&controller, &project.root);
    let edited_row = row_index(&controller, &project.edited);

    // A fused row loads at once.
    controller.select_checkpoint_row(root_row);
    assert_eq!(selected_rows(&controller), vec![root_row]);
    let composite = loaded_composite(controller.canvas_feed(None));
    assert_eq!(samples(&composite), expected_root);

    // An op-bearing row answers before its render settles, keeping the
    // previous image, then loads the render.
    controller.select_checkpoint_row(edited_row);
    assert_eq!(selected_rows(&controller), vec![edited_row]);
    assert!(matches!(controller.canvas_feed(Some(0)), CanvasFeed::Keep));
    wait_for_render(&mut controller);
    let composite = loaded_composite(controller.canvas_feed(Some(0)));
    assert_eq!(samples(&composite), expected_edited);

    // A changed source: the op-bearing row refuses, naming that frame.
    write_rgb(&project.sources[1], &frame_samples(7));
    controller.select_checkpoint_row(root_row);
    loaded_composite(controller.canvas_feed(Some(0)));
    controller.select_checkpoint_row(edited_row);
    assert!(matches!(controller.canvas_feed(Some(0)), CanvasFeed::Keep));
    wait_for_render(&mut controller);
    assert!(matches!(controller.canvas_feed(Some(0)), CanvasFeed::Clear));
    assert!(controller.materialize_active_tab_planes().is_none());
    let lines = controller.composite_message_lines();
    assert_eq!(
        lines,
        vec![
            "Cannot show this checkpoint — these frames are missing or changed:".to_string(),
            "changed: frame-01.tif".to_string(),
        ]
    );
    assert!(lines.iter().all(|line| !line.contains(&project.edited)));
    assert_eq!(selected_rows(&controller), vec![edited_row]);

    // A fused row renders with every source gone.
    for source in &project.sources {
        fs::remove_file(source).unwrap();
    }
    controller.select_checkpoint_row(root_row);
    let composite = loaded_composite(controller.canvas_feed(Some(0)));
    assert_eq!(samples(&composite), expected_root);
    assert!(controller.composite_message_lines().is_empty());
}

/// In editor mode a row click with edits in the session changes nothing;
/// without edits it selects the row and the session reopens over that row's
/// Z-Map once its render settles.
#[test]
fn a_row_click_with_unsaved_edits_is_a_no_op() {
    let project = build_edited("checkpoint_panel_click_edits");
    let mut controller = scratch_controller("checkpoint_panel_click_edits");
    common::open_and_settle(&mut controller, std::slice::from_ref(&project.project));
    let root_row = row_index(&controller, &project.root);
    let edited_row = row_index(&controller, &project.edited);
    controller.select_checkpoint_row(root_row);
    assert!(controller.set_mode(ShellMode::Editor));

    controller.select_checkpoint_row(edited_row);
    assert_eq!(selected_rows(&controller), vec![edited_row]);
    wait_for_render(&mut controller);
    let (_, zmap) = controller.materialize_active_tab_planes().unwrap();
    assert_eq!(
        controller.edit_session().unwrap().indices(),
        read_zmap(&zmap).as_slice()
    );

    controller
        .edit_session_mut()
        .unwrap()
        .append(dab())
        .expect("a dab inside the map applies");
    controller.select_checkpoint_row(root_row);
    assert_eq!(selected_rows(&controller), vec![edited_row]);
    assert_eq!(controller.edit_session().unwrap().op_count(), 1);
}

/// Switching, in editor mode, to a tab whose current row still renders keeps
/// editor mode; the session opens once the render settles.
#[test]
fn switching_to_a_rendering_tab_keeps_editor_mode() {
    let first = build_edited("checkpoint_panel_switch_a");
    let second = build_edited("checkpoint_panel_switch_b");
    let mut controller = scratch_controller("checkpoint_panel_switch");
    common::open_and_settle(&mut controller, std::slice::from_ref(&first.project));
    common::open_and_settle(&mut controller, std::slice::from_ref(&second.project));
    controller.select_project(0);
    controller.select_checkpoint_row(row_index(&controller, &first.root));
    assert!(controller.set_mode(ShellMode::Editor));

    controller.select_project(1);
    assert_eq!(controller.mode(), ShellMode::Editor);
    wait_for_render(&mut controller);
    assert_eq!(controller.mode(), ShellMode::Editor);
    assert!(controller.edit_session().is_some());
}

// ---------------------------------------------------------------------
// Refusals.
// ---------------------------------------------------------------------

/// A render that fails for any reason but its frames, or whose worker
/// panics, shows the generic line.
#[test]
fn other_render_failures_show_the_generic_line() {
    let generic = vec!["Cannot show this checkpoint.".to_string()];
    let err = job::PlaneCacheError::Project(ProjectError::FuseStateUnknown {
        path: PathBuf::from("p.ffp"),
        checkpoint: "abc".to_string(),
    });
    assert_eq!(job::render_refusal_lines(&err), generic);
    assert_eq!(
        job::render_outcome(Err(Box::new("the worker panicked"))),
        Err(generic)
    );
}

/// The left pane names a frame it cannot open as missing or changed, and
/// does not reopen a failed frame until the frame selection changes.
#[test]
fn a_failed_frame_is_named_and_not_reopened_per_repaint() {
    let dir = common::tree("checkpoint_panel_frame_refusal");
    let path = dir.join("00.tif");
    let mut canvas = Canvas::default();
    let missing = vec![
        "Cannot show this frame:".to_string(),
        "missing: 00.tif".to_string(),
    ];
    canvas.set_frame(Some(&path));
    assert_eq!(canvas.frame_refusal(), missing.as_slice());

    fs::copy(common::fixture_frame("mantis-64", 0), &path).unwrap();
    canvas.set_frame(Some(&path));
    assert_eq!(
        canvas.frame_refusal(),
        missing.as_slice(),
        "a repaint of the same selection must not reopen the frame"
    );
    canvas.set_frame(None);
    canvas.set_frame(Some(&path));
    assert!(canvas.frame_refusal().is_empty());

    let junk = dir.join("01.tif");
    fs::write(&junk, b"not a tiff").unwrap();
    canvas.set_frame(Some(&junk));
    assert_eq!(
        canvas.frame_refusal(),
        [
            "Cannot show this frame:".to_string(),
            "changed: 01.tif".to_string()
        ]
        .as_slice()
    );
}

// ---------------------------------------------------------------------
// Day separators, empty states, the four-row list.
// ---------------------------------------------------------------------

/// Local noon `days_ago` days before today, so no time zone moves a row
/// across midnight.
fn local_noon(days_ago: u64) -> (NaiveDate, i64) {
    let date = checkpoints::today()
        .checked_sub_days(Days::new(days_ago))
        .unwrap();
    let at = date
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_local_timezone(Local)
        .earliest()
        .unwrap()
        .timestamp();
    (date, at)
}

fn separator_labels(controller: &AppController) -> Vec<String> {
    controller
        .checkpoints_panel()
        .entries
        .iter()
        .filter_map(|entry| entry.separator_label.clone())
        .collect()
}

#[test]
fn day_separator_appears_only_across_days() {
    let dir = common::tree("checkpoint_panel_days");
    let (_, today) = local_noon(0);
    let one_day = dir.join("one-day.ffp");
    write_chain(&one_day, &[today - 60, today]);
    let mut controller = AppController::new();
    common::open_and_settle(&mut controller, &[one_day]);
    assert!(separator_labels(&controller).is_empty());

    let (_, yesterday) = local_noon(1);
    let (older_date, older) = local_noon(3);
    let three_days = dir.join("three-days.ffp");
    write_chain(&three_days, &[older, yesterday, today]);
    let mut controller = AppController::new();
    common::open_and_settle(&mut controller, &[three_days]);
    const MONTHS: [&str; 12] = [
        "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
    ];
    let absolute = format!(
        "{:02} {} {}",
        older_date.day(),
        MONTHS[older_date.month0() as usize],
        older_date.year()
    );
    assert_eq!(
        separator_labels(&controller),
        vec!["TODAY".to_string(), "YESTERDAY".to_string(), absolute]
    );
}

/// A 1440 x 900 app window installed over `controller`.
fn installed_window(
    controller: AppController,
) -> (
    AppWindow,
    Arc<Mutex<AppController>>,
    focusforge_gui::bridge::Installed,
) {
    // The testing platform installs once per thread.
    thread_local! {
        static INIT: std::sync::Once = const { std::sync::Once::new() };
    }
    INIT.with(|init| init.call_once(i_slint_backend_testing::init_no_event_loop));
    let app = AppWindow::new().unwrap();
    app.window().set_size(slint::PhysicalSize::new(1440, 900));
    let controller = Arc::new(Mutex::new(controller));
    let installed =
        focusforge_gui::bridge::install(&app, controller.clone(), common::empty_canvas());
    focusforge_gui::bridge::push_projects(&app, &controller);
    // The dock's tab row shows only once a project is loaded, which the
    // canvas repaint reports.
    app.set_project_loaded(true);
    (app, controller, installed)
}

fn has_element(app: &AppWindow, id: &str) -> bool {
    ElementQuery::from_root(app)
        .match_id(id)
        .find_first()
        .is_some()
}

#[test]
fn single_checkpoint_and_empty_states() {
    let dir = common::tree("checkpoint_panel_single");
    let project_path = dir.join("single.ffp");
    write_chain(&project_path, &[1_756_468_800]);
    let mut controller = AppController::new();
    common::open_and_settle(&mut controller, &[project_path]);
    let panel = controller.checkpoints_panel();
    assert_eq!(panel.entries.len(), 1, "one row and no separator");
    assert!(panel.entries[0].selected);
    let (app, _controller, _installed) = installed_window(controller);
    assert!(!has_element(&app, "FFCheckpointPanel::empty-note"));

    let frames = common::tree("checkpoint_panel_unsaved");
    for i in 0..2 {
        fs::copy(
            common::fixture_frame("mantis-64", i),
            frames.join(format!("{i:02}.tif")),
        )
        .unwrap();
    }
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[frames]),
        OpenOutcome::Opened(_)
    ));
    assert!(controller.checkpoints_panel().entries.is_empty());
    assert_eq!(
        controller.checkpoint_panel_line(),
        "None yet — fuse a stack"
    );
    assert!(!controller.export_enabled());
    assert!(!controller.editor_enabled());
    let (app, _controller, _installed) = installed_window(controller);
    assert!(has_element(&app, "FFCheckpointPanel::empty-note"));
}

fn dock_tabs_y(app: &AppWindow) -> f32 {
    ElementQuery::from_root(app)
        .match_id("FFDock::dock-tabs")
        .find_first()
        .expect("the dock tabs are drawn")
        .absolute_position()
        .y
}

/// Forty-seven checkpoints over twelve days scroll inside a 160 px list,
/// clamped at both ends, with exactly one sticky day header at any offset,
/// and the dock tabs sit where they sit for a one-row project.
#[test]
fn long_history_scrolls_in_a_four_row_list() {
    let dir = common::tree("checkpoint_panel_long");
    let one_row = dir.join("one.ffp");
    write_chain(&one_row, &[1_756_468_800]);
    let long = dir.join("long.ffp");
    write_history(&long, 47, 86_400 / 4);

    let mut controller = AppController::new();
    common::open_and_settle(&mut controller, &[one_row]);
    let (app, _controller, _installed) = installed_window(controller);
    let one_row_tabs_y = dock_tabs_y(&app);

    let mut controller = AppController::new();
    common::open_and_settle(&mut controller, &[long]);
    let (app, controller, _installed) = installed_window(controller);
    assert_eq!(app.get_checkpoint_viewport_height(), 160.0);
    assert_eq!(dock_tabs_y(&app), one_row_tabs_y);

    let mut c = controller.lock().unwrap();
    let panel = c.checkpoints_panel();
    assert_eq!(panel.row_count, 47);
    assert!(panel.content_height > 160.0);
    c.scroll_checkpoints_panel(-10_000.0);
    assert_eq!(c.checkpoints_scroll_y(), 0.0);
    c.scroll_checkpoints_panel(100_000.0);
    let bottom = panel.content_height - 160.0;
    assert_eq!(c.checkpoints_scroll_y(), bottom);

    // The sticky header: at every offset exactly one day's separator is
    // current -- at or above the list's top, its successor still below.
    let separators: Vec<_> = panel
        .entries
        .iter()
        .filter(|entry| entry.separator_label.is_some())
        .collect();
    assert!(separators.len() > 1);
    let mut offset = 0.0;
    while offset <= bottom {
        let current = separators
            .iter()
            .filter(|entry| {
                entry.top <= offset && offset < entry.next_separator_top.unwrap_or(f32::INFINITY)
            })
            .count();
        assert_eq!(current, 1, "one sticky header at offset {offset}");
        offset += 7.0;
    }
}
