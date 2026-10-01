// SPDX-License-Identifier: GPL-3.0-or-later

//! The checkpoint dock panel: the view model in `checkpoints.rs` merged with
//! selection and materialization behind `AppController`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{Datelike, Days, Local, NaiveDate};
use focusforge_core::align::Registration;
use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter};
use focusforge_core::params::ParamValues;
use focusforge_core::project::{
    HeldCheckpoint, InMemoryCheckpoint, InMemoryKind, Metrics, Op, Point, Project, ProjectError,
    SealRequest, ZMapStorage, append_ops_at, append_ops_onto, render_checkpoint, seal_at,
};
use focusforge_core::zmap::{ZMapInfo, ZMapReader, ZMapWriter};
use focusforge_gui::bridge::Canvas;
use focusforge_gui::canvas::CanvasFeed;
use focusforge_gui::input_map::PointerKind;
use focusforge_gui::{
    AppController, AppWindow, OpenOutcome, ShellEvent, ShellMode, checkpoints, job,
};
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
    let _installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );

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
            registration: Registration::default(),
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

fn held_at(created_at: i64) -> InMemoryCheckpoint {
    let held = HeldCheckpoint {
        composite: PathBuf::from("composite.tif"),
        zmap: PathBuf::from("zmap.tif"),
        width: 4,
        height: 4,
        sources: Vec::new(),
        algorithm: "laplacian-pyramid".to_string(),
        params: ParamValues::new(),
        align: None,
        registration: Registration::default(),
        composite_description: String::new(),
        metrics: Metrics::default(),
        created_at,
    };
    InMemoryCheckpoint {
        kind: InMemoryKind::Fused(held),
        parent: None,
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
            registration: Registration::default(),
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

// ---------------------------------------------------------------------
// Creating and restoring checkpoints.
// ---------------------------------------------------------------------

/// A stroke painting `source_id` at `(x, y)`; the map is 5 x 3 with source
/// `(y * 5 + x) % 3` at each pixel.
fn stroke(x: i32, y: i32, source_id: u16) -> Op {
    Op::BrushStroke {
        source_id,
        path: vec![Point { x, y }],
        radius: 0.5,
        softness: 0.0,
    }
}

fn paint(controller: &mut AppController, op: Op) {
    controller
        .edit_session_mut()
        .expect("the tab has an edit session")
        .append(op)
        .expect("a stroke inside the map applies");
}

/// `set_mode(Editor)`, waiting out the render an op-bearing row needs first.
fn enter_editor(controller: &mut AppController) {
    let start = Instant::now();
    while !controller.set_mode(ShellMode::Editor) {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the editor did not open"
        );
        controller.settle_checkpoint_render();
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn live_indices(controller: &AppController) -> Vec<u16> {
    controller.edit_session().unwrap().indices().to_vec()
}

fn live_ops(controller: &AppController) -> usize {
    controller.edit_session().map_or(0, |s| s.op_count())
}

fn current_row(controller: &AppController) -> Option<String> {
    controller
        .checkpoints_panel()
        .entries
        .iter()
        .find(|entry| entry.selected)
        .and_then(|entry| entry.id.clone())
}

fn click(controller: &mut AppController, id: &str) {
    let row = row_index(controller, id);
    controller.select_checkpoint_row(row);
}

fn saved_ids(project: &Path) -> Vec<String> {
    Project::open(project)
        .unwrap()
        .history()
        .all()
        .iter()
        .map(|checkpoint| checkpoint.id.clone())
        .collect()
}

fn parent_of(project: &Path, id: &str) -> Option<String> {
    Project::open(project)
        .unwrap()
        .history()
        .get(id)
        .unwrap()
        .parent
        .clone()
}

/// The one saved id of `project` that is not in `known`.
fn added_id(project: &Path, known: &[&str]) -> String {
    let added: Vec<String> = saved_ids(project)
        .into_iter()
        .filter(|id| !known.contains(&id.as_str()))
        .collect();
    assert_eq!(added.len(), 1, "exactly one checkpoint is new");
    added.into_iter().next().unwrap()
}

/// `build_edited`'s project opened with its head current and the editor on
/// it, the canvas already loaded.
fn editor_on_head(name: &str) -> (Edited, AppController) {
    let project = build_edited(name);
    let mut controller = scratch_controller(name);
    common::open_and_settle(&mut controller, std::slice::from_ref(&project.project));
    enter_editor(&mut controller);
    loaded_composite(controller.canvas_feed(None));
    (project, controller)
}

fn record_events(controller: &mut AppController) -> Arc<Mutex<Vec<ShellEvent>>> {
    let events: Arc<Mutex<Vec<ShellEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    controller.set_sink(Arc::new(move |event| sink.lock().unwrap().push(event)));
    events
}

/// Saving a checkpoint keeps live's image, makes the new row the origin and
/// parents it on the origin, whichever kind of row that is; the `.ffp` is
/// untouched until the project is saved.
#[test]
fn manual_checkpoint_is_a_child_of_the_origin() {
    let mut empty = AppController::new();
    assert!(!empty.brush_toolbar_view().can_save_checkpoint);
    assert!(!empty.save_checkpoint(), "no tab to snapshot");

    // A tab that has not entered the editor has no session to keep.
    let stacking_project = build_edited("checkpoint_panel_manual_stacking");
    let mut stacking = scratch_controller("checkpoint_panel_manual_stacking");
    common::open_and_settle(&mut stacking, &[stacking_project.project]);
    assert!(stacking.edit_session().is_none());
    assert!(!stacking.brush_toolbar_view().can_save_checkpoint);
    assert!(!stacking.save_checkpoint());

    let (project, mut controller) = editor_on_head("checkpoint_panel_manual");
    assert!(!controller.brush_toolbar_view().can_save_checkpoint);
    let rows = controller.checkpoints_panel();
    assert!(!controller.save_checkpoint(), "no ops, nothing to keep");
    assert_eq!(controller.checkpoints_panel(), rows);

    paint(&mut controller, stroke(1, 1, 0));
    paint(&mut controller, stroke(3, 1, 1));
    assert!(controller.brush_toolbar_view().can_save_checkpoint);
    controller.undo();
    controller.undo();
    assert_eq!(live_ops(&controller), 0);
    assert!(
        !controller.brush_toolbar_view().can_save_checkpoint,
        "undone back to zero ops, nothing to keep"
    );
    assert!(!controller.save_checkpoint());
    controller.redo();
    assert!(controller.brush_toolbar_view().can_save_checkpoint);
    controller.redo();
    assert_eq!(live_ops(&controller), 2);
    let live = live_indices(&controller);
    let bytes = fs::read(&project.project).unwrap();
    let before = row_ids(&controller.checkpoints_panel());

    assert!(controller.save_checkpoint());
    let panel = controller.checkpoints_panel();
    let top = panel.entries.iter().position(|e| e.id.is_some()).unwrap();
    assert_eq!(panel.entries[top].id.as_deref(), Some("held:0"));
    assert_eq!(selected_rows(&controller), vec![top]);
    assert_eq!(
        row_ids(&panel)[1..],
        before,
        "every earlier row keeps its place"
    );
    assert_eq!(live_ops(&controller), 0);
    assert!(!controller.edit_session().unwrap().can_undo());
    assert_eq!(live_indices(&controller), live);
    assert!(!controller.brush_toolbar_view().can_save_checkpoint);
    assert!(
        controller.save_enabled(),
        "an in-memory checkpoint is unsaved"
    );
    assert!(matches!(controller.canvas_feed(Some(0)), CanvasFeed::Keep));
    assert_eq!(
        fs::read(&project.project).unwrap(),
        bytes,
        "nothing written"
    );

    // A second one parents on the first, which is only in memory.
    paint(&mut controller, stroke(4, 2, 1));
    assert!(controller.save_checkpoint());
    assert_eq!(current_row(&controller).as_deref(), Some("held:1"));

    assert!(controller.save_active_project(|_| None));
    let ids = saved_ids(&project.project);
    assert_eq!(ids.len(), 4);
    let first = ids
        .iter()
        .find(|id| parent_of(&project.project, id).as_deref() == Some(project.edited.as_str()))
        .expect("one checkpoint under the head")
        .clone();
    let second = ids
        .iter()
        .find(|id| parent_of(&project.project, id).as_deref() == Some(first.as_str()))
        .expect("one checkpoint under the first")
        .clone();
    assert_eq!(current_row(&controller), Some(second.clone()));

    // From an older row the parent is that row.
    click(&mut controller, &project.root);
    paint(&mut controller, stroke(2, 0, 1));
    assert!(controller.save_checkpoint());
    assert!(controller.save_active_project(|_| None));
    let third = added_id(
        &project.project,
        &[&project.root, &project.edited, &first, &second],
    );
    assert_eq!(
        parent_of(&project.project, &third).as_deref(),
        Some(project.root.as_str())
    );
}

#[test]
fn restoring_an_older_checkpoint_then_creating_one_branches_from_it() {
    let (project, mut controller) = editor_on_head("checkpoint_panel_branch");
    click(&mut controller, &project.root);
    assert_eq!(live_ops(&controller), 0);
    paint(&mut controller, stroke(2, 0, 1));
    assert!(controller.save_checkpoint());
    assert!(controller.save_active_project(|_| None));

    let reopened = Project::open(&project.project).unwrap();
    let branch = added_id(&project.project, &[&project.root, &project.edited]);
    assert_eq!(
        reopened.history().get(&branch).unwrap().parent.as_deref(),
        Some(project.root.as_str())
    );
    let ids = saved_ids(&project.project);
    assert!(ids.contains(&project.root) && ids.contains(&project.edited));
    assert_eq!(
        reopened
            .history()
            .get(&project.edited)
            .unwrap()
            .parent
            .as_deref(),
        Some(project.root.as_str()),
        "the head keeps its own parent"
    );

    let mut again = scratch_controller("checkpoint_panel_branch_reopened");
    common::open_and_settle(&mut again, std::slice::from_ref(&project.project));
    let panel = again.checkpoints_panel();
    assert_eq!(row_ids(&panel), ids, "time-ordered rows, no branch drawn");
    assert!(
        panel
            .entries
            .iter()
            .all(|entry| entry.separator_label.is_some() || entry.id.is_some())
    );
}

#[test]
fn restore_asks_only_when_live_is_modified() {
    let (project, mut controller) = editor_on_head("checkpoint_panel_restore_asks");

    // Nothing to lose: the click restores at once.
    click(&mut controller, &project.root);
    assert!(controller.pending_restore_copy().is_none());
    assert_eq!(
        current_row(&controller).as_deref(),
        Some(project.root.as_str())
    );
    assert_eq!(live_ops(&controller), 0);

    // One op: the singular body, nothing dropped until it is answered.
    paint(&mut controller, stroke(1, 1, 0));
    click(&mut controller, &project.edited);
    let copy = controller.pending_restore_copy().expect("the dialog is up");
    let target = controller
        .checkpoints_panel()
        .entries
        .into_iter()
        .find(|entry| entry.id.as_deref() == Some(project.edited.as_str()))
        .unwrap();
    assert_eq!(copy.title, "Save this state first?");
    assert_eq!(
        copy.context,
        format!("Restoring {} · {}", target.algorithm, target.time_text)
    );
    assert_eq!(
        copy.body,
        "1 brush stroke has happened since the last checkpoint. It belongs to no saved \
         state.\n\nSave it as a checkpoint first, then restore — or restore and discard your \
         changes, which loses it."
    );
    assert_eq!(
        copy.info,
        "A saved state lands at the top of the list, like any other."
    );
    assert_eq!(copy.secondary_label, "Save first, then restore");
    assert_eq!(copy.confirm_label, "Discard and restore");
    assert_eq!(
        current_row(&controller).as_deref(),
        Some(project.root.as_str())
    );
    assert_eq!(live_ops(&controller), 1);

    // A second click while it is pending changes nothing.
    click(&mut controller, &project.root);
    click(&mut controller, &project.edited);
    assert_eq!(
        controller.pending_restore_copy().unwrap().context,
        copy.context
    );
    assert_eq!(live_ops(&controller), 1);

    // Cancel keeps row, origin and ops.
    controller.close_guard_cancelled();
    assert!(controller.pending_restore_copy().is_none());
    assert_eq!(
        current_row(&controller).as_deref(),
        Some(project.root.as_str())
    );
    assert_eq!(live_ops(&controller), 1);

    // Three ops: the plural body with its digit.
    paint(&mut controller, stroke(2, 1, 0));
    paint(&mut controller, stroke(3, 1, 1));
    click(&mut controller, &project.edited);
    let copy = controller.pending_restore_copy().unwrap();
    assert_eq!(
        copy.body,
        "3 brush strokes have happened since the last checkpoint. They belong to no saved \
         state.\n\nSave them as a checkpoint first, then restore — or restore and discard \
         your changes, which loses them."
    );

    // Discard and restore.
    controller.close_guard_confirmed();
    assert!(controller.pending_restore_copy().is_none());
    assert_eq!(
        current_row(&controller).as_deref(),
        Some(project.edited.as_str())
    );
    wait_for_render(&mut controller);
    assert_eq!(live_ops(&controller), 0);
    assert_eq!(saved_ids(&project.project).len(), 2, "nothing was kept");

    // Save first, then restore: one checkpoint under the old origin.
    paint(&mut controller, stroke(1, 1, 0));
    click(&mut controller, &project.root);
    assert!(!controller.close_guard_secondary_pressed(|_| None));
    assert!(controller.take_settled_failure().is_none(), "no toast");
    assert_eq!(controller.checkpoints_panel().row_count, 3);
    assert_eq!(
        current_row(&controller).as_deref(),
        Some(project.root.as_str())
    );
    assert_eq!(live_ops(&controller), 0);
    assert!(controller.save_active_project(|_| None));
    let kept = added_id(&project.project, &[&project.root, &project.edited]);
    assert_eq!(
        parent_of(&project.project, &kept).as_deref(),
        Some(project.edited.as_str())
    );
}

#[test]
fn a_row_click_in_stacking_mode_asks_with_unsaved_ops() {
    let (project, mut controller) = editor_on_head("checkpoint_panel_stacking_click");
    paint(&mut controller, stroke(1, 1, 0));
    paint(&mut controller, stroke(3, 1, 1));
    assert!(controller.set_mode(ShellMode::Stacking));

    click(&mut controller, &project.root);
    let copy = controller
        .pending_restore_copy()
        .expect("asks in Stacking mode too");
    assert!(copy.body.starts_with("2 brush strokes have happened"));
    assert_eq!(
        current_row(&controller).as_deref(),
        Some(project.edited.as_str())
    );
    assert_eq!(live_ops(&controller), 2);

    controller.close_guard_confirmed();
    assert_eq!(
        current_row(&controller).as_deref(),
        Some(project.root.as_str())
    );
    assert_eq!(live_ops(&controller), 0);

    // The filled answer, from the head again.
    click(&mut controller, &project.edited);
    enter_editor(&mut controller);
    paint(&mut controller, stroke(1, 1, 0));
    paint(&mut controller, stroke(3, 1, 1));
    assert!(controller.set_mode(ShellMode::Stacking));
    click(&mut controller, &project.root);
    assert!(controller.pending_restore_copy().is_some());
    assert!(!controller.close_guard_secondary_pressed(|_| None));
    assert_eq!(
        current_row(&controller).as_deref(),
        Some(project.root.as_str())
    );
    assert!(controller.save_active_project(|_| None));
    let kept = added_id(&project.project, &[&project.root, &project.edited]);
    assert_eq!(
        parent_of(&project.project, &kept).as_deref(),
        Some(project.edited.as_str())
    );
}

/// A restored in-memory stroke checkpoint shows exactly the image it had when
/// it was created: its base's planes with its strokes laid over them and
/// nothing left to undo, in either mode and through two levels of them.
#[test]
fn restoring_an_unsaved_stroke_checkpoint_shows_its_image() {
    let (project, mut controller) = editor_on_head("checkpoint_panel_restore_strokes");
    let out = common::tree("checkpoint_panel_restore_strokes_expected");
    let head_image = rendered(&project.project, &project.edited, &out.join("head.tif"));

    paint(&mut controller, stroke(1, 1, 0));
    paint(&mut controller, stroke(3, 1, 1));
    let x_indices = live_indices(&controller);
    assert!(controller.save_checkpoint());
    paint(&mut controller, stroke(4, 2, 1));
    let y_indices = live_indices(&controller);
    assert_ne!(x_indices, y_indices);
    assert!(controller.save_checkpoint());
    let (x, y) = ("held:0", "held:1");

    // Stacking mode: restore the head first (silent), then X.
    click(&mut controller, &project.edited);
    wait_for_render(&mut controller);
    assert_ne!(live_indices(&controller), x_indices);
    assert!(controller.set_mode(ShellMode::Stacking));
    click(&mut controller, x);
    wait_for_render(&mut controller);
    let session = controller
        .edit_session()
        .expect("an op row opens its session");
    assert_eq!(session.indices(), x_indices.as_slice());
    assert_eq!(session.op_count(), 0);
    assert!(!session.can_undo() && !session.can_redo());
    let composite = loaded_composite(controller.canvas_feed(None));
    assert_eq!(samples(&composite), head_image, "the base's planes load");
    assert_eq!(current_row(&controller).as_deref(), Some(x));

    // Entering the editor keeps the restored indices.
    enter_editor(&mut controller);
    assert_eq!(live_indices(&controller), x_indices);
    assert_eq!(live_ops(&controller), 0);

    // Two levels: Y over X over the head.
    click(&mut controller, y);
    wait_for_render(&mut controller);
    assert_eq!(live_indices(&controller), y_indices);
    assert_eq!(live_ops(&controller), 0);
    assert!(!controller.edit_session().unwrap().can_undo());
}

/// Restores `target` from Stacking mode, then enters the editor, and checks
/// live holds exactly `expected` with nothing to undo in both.
fn assert_restores_to(controller: &mut AppController, target: &str, expected: &[u16]) {
    assert!(controller.set_mode(ShellMode::Stacking));
    click(controller, target);
    wait_for_render(controller);
    let session = controller
        .edit_session()
        .expect("an op row opens its session");
    assert_eq!(session.indices(), expected);
    assert_eq!(session.op_count(), 0);
    assert!(!session.can_undo() && !session.can_redo());
    enter_editor(controller);
    assert_eq!(live_indices(controller), expected);
    assert_eq!(live_ops(controller), 0);
}

/// A stroke checkpoint taken over a saved fused root, which carries no ops of
/// its own, restores to that root's map with its strokes laid over it.
#[test]
fn restoring_an_unsaved_stroke_checkpoint_over_a_saved_fused_base() {
    let (project, mut controller) = editor_on_head("checkpoint_panel_restore_over_root");
    click(&mut controller, &project.root);
    assert_eq!(live_ops(&controller), 0);
    let root_indices = live_indices(&controller);
    paint(&mut controller, stroke(1, 1, 0));
    paint(&mut controller, stroke(3, 1, 1));
    let expected = live_indices(&controller);
    assert_ne!(expected, root_indices);
    assert!(controller.save_checkpoint());

    click(&mut controller, &project.edited);
    wait_for_render(&mut controller);
    assert_ne!(live_indices(&controller), expected);
    assert_restores_to(&mut controller, "held:0", &expected);
}

/// The same over a fused checkpoint that exists only in memory: a fuse run on
/// the fixture frames, two stroke levels above it.
#[test]
fn restoring_an_unsaved_stroke_checkpoint_over_an_in_memory_fused_base() {
    let name = "checkpoint_panel_restore_over_fused";
    let frames = common::tree(&format!("{name}_frames"));
    for i in 0..3 {
        fs::copy(
            common::fixture_frame("mantis-64", i),
            frames.join(format!("{i:02}.tif")),
        )
        .expect("fixture frame must copy");
    }
    let mut controller = scratch_controller(name);
    common::open_and_settle(&mut controller, &[frames]);
    controller.set_align_algorithm("dummy-align");
    assert!(controller.start_fuse());
    while common::wait_for_settle(&mut controller, Duration::from_secs(60)).is_some() {}
    assert_eq!(controller.checkpoint_count(), Some(1), "the fuse commits");
    enter_editor(&mut controller);
    let base = live_indices(&controller);

    paint(&mut controller, stroke(10, 10, 1));
    paint(&mut controller, stroke(20, 12, 2));
    let x_indices = live_indices(&controller);
    assert_ne!(x_indices, base);
    assert!(controller.save_checkpoint());
    paint(&mut controller, stroke(30, 30, 1));
    let y_indices = live_indices(&controller);
    assert_ne!(y_indices, x_indices);
    assert!(controller.save_checkpoint());

    click(&mut controller, "held:0");
    assert_eq!(live_indices(&controller), base);
    assert_restores_to(&mut controller, "held:1", &x_indices);
    assert_restores_to(&mut controller, "held:2", &y_indices);
}

/// The canvas and the session follow the row live stands on, not the
/// highlighted one: a create moves the highlight to a row with no planes of
/// its own, and a stroke row's image is its base's.
#[test]
fn live_keeps_its_own_planes_when_the_highlight_moves() {
    let (project, mut controller) = editor_on_head("checkpoint_panel_live_planes");
    let second = build_edited("checkpoint_panel_live_planes_other");
    common::open_and_settle(&mut controller, std::slice::from_ref(&second.project));
    controller.select_project(0);
    let out = common::tree("checkpoint_panel_live_planes_expected");
    let head_image = rendered(&project.project, &project.edited, &out.join("head.tif"));
    let root_image = rendered(&project.project, &project.root, &out.join("root.tif"));
    assert_ne!(head_image, root_image);
    loaded_composite(controller.canvas_feed(None));

    paint(&mut controller, stroke(1, 1, 0));
    paint(&mut controller, stroke(3, 1, 1));
    let live = live_indices(&controller);
    assert!(controller.save_checkpoint());
    assert_eq!(current_row(&controller).as_deref(), Some("held:0"));
    let composite = loaded_composite(controller.canvas_feed(None));
    assert_eq!(samples(&composite), head_image, "live's planes, not Clear");
    assert_eq!(live_indices(&controller), live);

    controller.select_project(1);
    controller.select_project(0);
    let composite = loaded_composite(controller.canvas_feed(None));
    assert_eq!(samples(&composite), head_image, "after a tab round trip");
    assert_eq!(live_indices(&controller), live);
    enter_editor(&mut controller);

    // Over the root instead of the head.
    click(&mut controller, &project.root);
    paint(&mut controller, stroke(2, 0, 1));
    assert!(controller.save_checkpoint());
    let x = current_row(&controller).unwrap();
    click(&mut controller, &project.edited);
    wait_for_render(&mut controller);
    assert!(controller.set_mode(ShellMode::Stacking));
    click(&mut controller, &x);
    wait_for_render(&mut controller);
    let composite = loaded_composite(controller.canvas_feed(None));
    assert_eq!(samples(&composite), root_image);
    assert_eq!(current_row(&controller).as_deref(), Some(x.as_str()));
}

#[test]
fn clicking_the_current_row_with_ops_asks_to_restore_it() {
    let project = build_edited("checkpoint_panel_current_row");
    let mut controller = scratch_controller("checkpoint_panel_current_row");
    common::open_and_settle(&mut controller, std::slice::from_ref(&project.project));
    click(&mut controller, &project.root);
    enter_editor(&mut controller);
    let (_, zmap) = controller.materialize_active_tab_planes().unwrap();
    let root_map = read_zmap(&zmap);
    loaded_composite(controller.canvas_feed(None));

    // Zero ops: nothing happens, nothing is emitted.
    let events = record_events(&mut controller);
    click(&mut controller, &project.root);
    assert!(controller.pending_restore_copy().is_none());
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(live_ops(&controller), 0);

    // Two ops on the root: the dialog names the root; discard goes back to it.
    paint(&mut controller, stroke(1, 1, 0));
    paint(&mut controller, stroke(3, 1, 1));
    click(&mut controller, &project.root);
    let copy = controller.pending_restore_copy().expect("the dialog is up");
    let target = controller
        .checkpoints_panel()
        .entries
        .into_iter()
        .find(|entry| entry.id.as_deref() == Some(project.root.as_str()))
        .unwrap();
    assert_eq!(
        copy.context,
        format!("Restoring {} · {}", target.algorithm, target.time_text)
    );
    controller.close_guard_confirmed();
    assert_eq!(live_ops(&controller), 0);
    assert_eq!(live_indices(&controller), root_map);
    assert_eq!(
        current_row(&controller).as_deref(),
        Some(project.root.as_str())
    );

    // The filled answer: one checkpoint under the root, the root current again.
    paint(&mut controller, stroke(1, 1, 0));
    click(&mut controller, &project.root);
    assert!(!controller.close_guard_secondary_pressed(|_| None));
    assert_eq!(controller.checkpoints_panel().row_count, 3);
    assert_eq!(
        current_row(&controller).as_deref(),
        Some(project.root.as_str())
    );
    assert_eq!(live_indices(&controller), root_map);
    assert!(controller.save_active_project(|_| None));
    let kept = added_id(&project.project, &[&project.root, &project.edited]);
    assert_eq!(
        parent_of(&project.project, &kept).as_deref(),
        Some(project.root.as_str())
    );
}

/// While the restore question is open the window and the tab cannot be
/// closed, and the shell reports a modal.
#[test]
fn an_open_restore_question_blocks_closing() {
    let project = build_edited("checkpoint_panel_restore_blocks_close");
    let mut controller = scratch_controller("checkpoint_panel_restore_blocks_close");
    common::open_and_settle(&mut controller, std::slice::from_ref(&project.project));
    click(&mut controller, &project.root);
    enter_editor(&mut controller);
    paint(&mut controller, stroke(1, 1, 0));
    assert!(!controller.modal_open());
    click(&mut controller, &project.root);
    assert!(controller.pending_restore_copy().is_some());
    assert!(controller.modal_open());

    let events = record_events(&mut controller);
    let tabs = controller.project_tabs().len();
    controller.window_close();
    controller.request_close_at(0, Instant::now() + Duration::from_secs(10));
    assert!(events.lock().unwrap().is_empty());
    assert!(controller.pending_quit_copy().is_none());
    assert!(controller.pending_close().is_none());
    assert_eq!(controller.project_tabs().len(), tabs);
    assert!(controller.pending_restore_copy().is_some());
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
    canvas.set_frame(Some(&path), None);
    assert_eq!(canvas.frame_refusal(), missing.as_slice());

    fs::copy(common::fixture_frame("mantis-64", 0), &path).unwrap();
    canvas.set_frame(Some(&path), None);
    assert_eq!(
        canvas.frame_refusal(),
        missing.as_slice(),
        "a repaint of the same selection must not reopen the frame"
    );
    canvas.set_frame(None, None);
    canvas.set_frame(Some(&path), None);
    assert!(canvas.frame_refusal().is_empty());

    let junk = dir.join("01.tif");
    fs::write(&junk, b"not a tiff").unwrap();
    canvas.set_frame(Some(&junk), None);
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
    let installed = focusforge_gui::bridge::install(
        &app,
        controller.clone(),
        common::empty_canvas(),
        std::rc::Rc::new(std::cell::RefCell::new(None)),
    );
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
    assert!(
        controller.export_enabled(),
        "a saved row's current position must be exportable"
    );
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
