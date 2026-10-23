// SPDX-License-Identifier: GPL-3.0-or-later

//! The checkpoint panel's `Export image` modal: enablement, the three
//! source kinds it names, its confirm/cancel flow, and its refusal.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use focusforge_core::align::Registration;
use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter};
use focusforge_core::params::ParamValues;
use focusforge_core::project::export::{
    ExportFormat, ExportOptions, MetadataOptions, export_checkpoint,
};
use focusforge_core::project::{
    Metrics, Op, Plane, Point, Project, SealRequest, append_ops_onto, materialize_plane, seal_at,
};
use focusforge_core::retouch::blend;
use focusforge_core::zmap::ZMapInfo;
use focusforge_gui::checkpoints::local_date_and_time;
use focusforge_gui::{AppController, OpenOutcome};

const SETTLE: Duration = Duration::from_secs(60);

fn scratch_controller(name: &str) -> AppController {
    let mut controller = AppController::new();
    controller.set_scratch_root_for_test(common::tree(&format!("{name}_scratch")));
    controller
}

fn settle_all(controller: &mut AppController) {
    while common::wait_for_settle(controller, SETTLE).is_some() {}
}

/// Opens the first `count` `fixtures/mantis-64` frames, copied under a fresh
/// directory, as a new active tab with no checkpoint.
fn open_mantis64_tab(controller: &mut AppController, name: &str, count: usize) -> PathBuf {
    let root = common::tree(&format!("{name}_frames"));
    for i in 0..count {
        fs::copy(
            common::fixture_frame("mantis-64", i),
            root.join(format!("{i:02}.tif")),
        )
        .expect("fixture frame must copy");
    }
    assert!(matches!(
        common::open_and_settle(controller, std::slice::from_ref(&root)),
        OpenOutcome::Opened(_)
    ));
    controller.set_align_algorithm("dummy-align");
    root
}

/// [`open_mantis64_tab`], then fuses it into one held checkpoint.
fn open_fused_tab(controller: &mut AppController, name: &str, count: usize) {
    open_mantis64_tab(controller, name, count);
    assert!(controller.start_fuse());
    settle_all(controller);
    assert_eq!(controller.checkpoint_count(), Some(1), "{name} must fuse");
}

/// Saves the active, never-saved tab to the path the save dialog proposes.
fn save_to_proposed(controller: &mut AppController) -> PathBuf {
    let mut target = None;
    assert!(common::save_and_settle(controller, |proposed| {
        target = Some(proposed.to_path_buf());
        target.clone()
    }));
    target.expect("a never-saved tab asks where to save")
}

fn wait_export(controller: &mut AppController) {
    let start = Instant::now();
    while !controller.settle_export() {
        assert!(start.elapsed() < SETTLE, "the export did not settle");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The only test in this binary that reaches the real save picker: the save
/// folder is process-wide and set once.
#[test]
fn save_folder_receives_the_export_under_its_proposed_name() {
    let folder = common::tree("save_folder_export_out");
    focusforge_gui::picker::set_save_folder(folder.clone());
    let mut controller = scratch_controller("save_folder_export");
    open_fused_tab(&mut controller, "save_folder_export", 3);
    controller.checkpoint_export_requested();
    controller.export_select_format("PNG");

    let mut proposed = None;
    controller.export_confirmed(|p| {
        proposed = Some(p.to_path_buf());
        focusforge_gui::picker::pick_save_image(p, ExportFormat::Png)
    });
    assert!(controller.take_export_started(), "the export must start");
    wait_export(&mut controller);

    let proposed = proposed.expect("the picker is asked for a destination");
    assert_eq!(proposed.extension().unwrap(), "png");
    let exported = folder.join(proposed.file_name().unwrap());
    assert!(exported.is_file(), "{} must be written", exported.display());
    assert!(
        !proposed.exists(),
        "nothing may be exported beside the frames"
    );
}

fn dab() -> Op {
    Op::BrushStroke {
        source_id: 0,
        path: vec![Point { x: 1, y: 1 }],
        radius: 0.5,
        softness: 0.0,
    }
}

const MW: u32 = 5;
const MH: u32 = 3;
const FRAMES: usize = 3;

fn full() -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: MW,
        height: MH,
    }
}

fn write_rgb(path: &Path, samples: &[u16]) {
    let samples_per_pixel = (samples.len() / (MW * MH) as usize) as u32;
    let mut writer = TiffWriter::create(
        path,
        TiffSpec {
            width: MW,
            height: MH,
            samples_per_pixel,
            description: Some("staged".to_string()),
        },
    )
    .unwrap();
    writer.write_tile(full(), samples).unwrap();
    writer.finish().unwrap();
}

fn frame_samples(seed: usize, samples_per_pixel: u32) -> Vec<u16> {
    (0..(MW * MH * samples_per_pixel) as usize)
        .map(|j| ((j * 131 + seed * 977) % 65_535) as u16)
        .collect()
}

/// A 5 x 3 project over three real source frames, no metadata of any kind:
/// a fused root and one op-bearing child, the head. Metadata-free sources
/// keep every export on the default TIFF's fast, un-timestamped path
/// (`plan_is_empty`), so a GUI export and a CLI export of the same
/// checkpoint are byte-identical regardless of which second each runs in.
struct Edited {
    project: PathBuf,
    sources: Vec<PathBuf>,
    root: String,
    edited: String,
}

fn build_edited(name: &str) -> Edited {
    build_edited_with_samples(name, 3)
}

fn build_edited_with_samples(name: &str, samples_per_pixel: u32) -> Edited {
    let dir = common::tree(name);
    let sources: Vec<PathBuf> = (0..FRAMES)
        .map(|i| {
            let path = dir.join(format!("frame-{i:02}.tif"));
            write_rgb(&path, &frame_samples(i, samples_per_pixel));
            path
        })
        .collect();
    let composite = dir.join("composite.tif");
    write_rgb(
        &composite,
        &vec![9000; (MW * MH * samples_per_pixel) as usize],
    );
    let zmap = dir.join("zmap.tif");
    let indices: Vec<u16> = (0..(MW * MH) as usize)
        .map(|i| (i % FRAMES) as u16)
        .collect();
    let mut map = focusforge_core::zmap::ZMapWriter::create(
        &zmap,
        focusforge_core::zmap::ZMapInfo::new(MW, MH, FRAMES as u32).unwrap(),
    )
    .unwrap();
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

fn row_index(controller: &AppController, id: &str) -> usize {
    controller
        .checkpoints_panel()
        .entries
        .iter()
        .position(|entry| entry.id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("no row for {id}"))
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

#[test]
fn export_button_opens_the_modal_for_the_current_position() {
    // No project, no row: disabled.
    let controller = scratch_controller("export_modal_none");
    assert!(!controller.export_enabled());

    // A project open, but no checkpoint yet (an un-fused tab): still no
    // current row to export.
    let mut controller = scratch_controller("export_modal_no_row");
    open_mantis64_tab(&mut controller, "export_modal_no_row", 2);
    assert!(
        !controller.export_enabled(),
        "an un-fused tab has no current position to export"
    );

    // A saved row.
    let edited = build_edited("export_modal_saved");
    let mut controller = scratch_controller("export_modal_saved");
    common::open_and_settle(&mut controller, std::slice::from_ref(&edited.project));
    let root_row = row_index(&controller, &edited.root);
    controller.select_checkpoint_row(root_row);
    assert!(controller.export_enabled());
    controller.checkpoint_export_requested();
    let (context, what) = controller
        .export_copy()
        .expect("the modal must open for a saved row");
    assert!(
        context.starts_with("edited.ffp"),
        "the context line must name the project file: {context}"
    );
    assert!(context.contains(" · "), "{context}");
    assert_eq!(controller.export_stage(), "ready");
    assert_eq!(what, "Exporting the saved checkpoint you selected.");

    // The context line names the row's own algorithm and local time.
    let project = Project::open(&edited.project).unwrap();
    let root_checkpoint = project.history().get(&edited.root).unwrap();
    assert!(
        context.contains(&root_checkpoint.algorithm),
        "the context must name the row's algorithm: {context}"
    );
    assert!(
        context.contains(&local_date_and_time(root_checkpoint.created_at)),
        "the context must name the row's local time: {context}"
    );
    drop(project);

    // The modal always exports the default TIFF; its format selector is
    // drawn but inert.
    assert_eq!(ExportOptions::default().format(), ExportFormat::Tiff);

    controller.cancel_export();
    assert!(
        controller.export_copy().is_none(),
        "cancel closes the modal"
    );

    // A held row, in a never-saved tab.
    let mut controller = scratch_controller("export_modal_held");
    open_fused_tab(&mut controller, "export_modal_held", 2);
    assert!(controller.export_enabled());
    controller.checkpoint_export_requested();
    let (context, what) = controller
        .export_copy()
        .expect("the modal must open for a held row");
    assert!(
        context.ends_with("unsaved fuse result, held in this tab"),
        "{context}"
    );
    assert_eq!(what, "Exporting the unsaved fuse result held in this tab.");

    // A tab with unsealed edits.
    let edited = build_edited("export_modal_live");
    let mut controller = scratch_controller("export_modal_live");
    common::open_and_settle(&mut controller, std::slice::from_ref(&edited.project));
    let root_row = row_index(&controller, &edited.root);
    controller.select_checkpoint_row(root_row);
    common::enter_editor(&mut controller);
    controller
        .edit_session_mut()
        .expect("editor mode opens a session")
        .append(dab())
        .expect("a dab inside the map applies");
    assert!(controller.export_enabled());
    controller.checkpoint_export_requested();
    let (context, what) = controller
        .export_copy()
        .expect("the modal must open for unsealed edits");
    assert!(context.contains("unsaved edits on"), "{context}");
    assert_eq!(what, "Exporting your current unsaved retouch edits.");
}

/// One dab, then **Save checkpoint**: the tab's selected row is a held
/// retouch entry with no unsealed edits after it. Checks the dialog's copy,
/// then that a further dab brings the live-edits copy back.
fn assert_held_retouch_export_copy(controller: &mut AppController) {
    controller
        .edit_session_mut()
        .expect("editor mode opens a session")
        .append(dab())
        .expect("a dab inside the map applies");
    assert!(controller.save_checkpoint(), "the edit seals a checkpoint");
    assert!(controller.export_enabled());
    controller.checkpoint_export_requested();
    let (context, what) = controller
        .export_copy()
        .expect("the modal must open for a held retouch checkpoint");
    assert!(context.contains(" · retouch checkpoint on "), "{context}");
    assert_eq!(
        what,
        "Exporting the unsaved retouch checkpoint held in this tab."
    );
    controller.cancel_export();

    controller
        .edit_session_mut()
        .expect("the session stays open")
        .append(dab())
        .expect("a second dab applies");
    controller.checkpoint_export_requested();
    let (context, what) = controller
        .export_copy()
        .expect("the modal must open for unsealed edits");
    assert!(context.contains("unsaved edits on"), "{context}");
    assert_eq!(what, "Exporting your current unsaved retouch edits.");
}

#[test]
fn held_retouch_checkpoint_export_line() {
    // A never-saved tab.
    let mut controller = scratch_controller("export_held_retouch_unsaved");
    open_fused_tab(&mut controller, "export_held_retouch_unsaved", 2);
    common::enter_editor(&mut controller);
    assert_held_retouch_export_copy(&mut controller);

    // A tab opened from a project file.
    let edited = build_edited("export_held_retouch_opened");
    let mut controller = scratch_controller("export_held_retouch_opened");
    common::open_and_settle(&mut controller, std::slice::from_ref(&edited.project));
    let root_row = row_index(&controller, &edited.root);
    controller.select_checkpoint_row(root_row);
    common::enter_editor(&mut controller);
    assert_held_retouch_export_copy(&mut controller);
}

#[test]
fn gui_export_equals_cli_export_and_cancel_writes_nothing() {
    let edited = build_edited("export_matches_cli");
    let mut controller = scratch_controller("export_matches_cli");
    common::open_and_settle(&mut controller, std::slice::from_ref(&edited.project));
    let row = row_index(&controller, &edited.edited);
    controller.select_checkpoint_row(row);
    controller.checkpoint_export_requested();

    // `export_confirmed` takes whatever path `pick` returns as-is; the
    // bare-name-gets-`.tif` behaviour is `picker::pick_save_image`'s own
    // job, covered directly in `crates/gui/src/picker.rs`'s unit tests.
    let dir = common::tree("export_matches_cli_out");
    let out_path = dir.join("gui-export.tif");
    let picked = out_path.clone();
    let ffp_before = fs::read(&edited.project).unwrap();
    controller.export_confirmed(move |_proposed| Some(picked.clone()));
    wait_export(&mut controller);
    assert_eq!(
        fs::read(&edited.project).unwrap(),
        ffp_before,
        "an export must not touch the project file"
    );
    assert!(
        controller.export_copy().is_none(),
        "the modal closes once the write succeeds"
    );
    assert!(out_path.exists());

    let mut project = Project::open(&edited.project).unwrap();
    let cli_out = dir.join("cli-export.tif");
    export_checkpoint(
        &mut project,
        &edited.edited,
        &cli_out,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .expect("the CLI export must succeed");

    assert_eq!(
        fs::read(&out_path).unwrap(),
        fs::read(&cli_out).unwrap(),
        "the GUI export must be byte-identical to the CLI export"
    );

    // Drains the real export's own `started` flag, so the check below
    // proves the *cancelled* dialog started nothing, not a stale flag this
    // earlier, successful export already set.
    assert!(
        controller.take_export_started(),
        "the real export above must have started a worker"
    );

    // Cancelling the save dialog never starts a write, proven against the
    // real destination directory's contents rather than one unused name:
    // an entry that appears here could only be this export's own file.
    let before: std::collections::BTreeSet<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    controller.select_checkpoint_row(row);
    controller.checkpoint_export_requested();
    let mut proposed_to_cancel = None;
    controller.export_confirmed(|proposed| {
        proposed_to_cancel = Some(proposed.to_path_buf());
        None
    });
    assert!(
        proposed_to_cancel.is_some(),
        "the picker must still be asked for a destination"
    );
    assert_eq!(
        controller.export_stage(),
        "ready",
        "a cancelled save dialog must not start a write"
    );
    assert!(
        !controller.take_export_started(),
        "a cancelled save dialog must never start a worker"
    );
    let after: std::collections::BTreeSet<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(before, after, "a cancelled save dialog must write nothing");

    // Cancelling the modal itself, before ever choosing a destination,
    // writes nothing either.
    controller.cancel_export();
    assert!(controller.export_copy().is_none());
    assert!(
        !controller.take_export_started(),
        "cancelling the modal must never start a worker"
    );
    let after_modal_cancel: std::collections::BTreeSet<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(
        before, after_modal_cancel,
        "cancelling the modal must write nothing"
    );
}

#[test]
fn live_export_writes_the_unsaved_state_and_changes_nothing() {
    // A held row's export equals `export_checkpoint` of that checkpoint
    // once saved, and touches neither the held count nor the write itself.
    let mut controller = scratch_controller("export_live_held");
    open_fused_tab(&mut controller, "export_live_held", 2);
    controller.checkpoint_export_requested();
    let dir = common::tree("export_live_held_out");
    let held_out = dir.join("held.tif");
    let picked = held_out.clone();
    controller.export_confirmed(move |_proposed| Some(picked.clone()));
    wait_export(&mut controller);
    assert!(held_out.exists());
    assert_eq!(
        controller.held_checkpoints().len(),
        1,
        "exporting a held row must not seal or drop it"
    );

    let saved_path = save_to_proposed(&mut controller);
    let mut project = Project::open(&saved_path).unwrap();
    let id = project.history().all()[0].id.clone();
    let cli_out = dir.join("saved.tif");
    export_checkpoint(
        &mut project,
        &id,
        &cli_out,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .unwrap();
    assert_eq!(fs::read(&held_out).unwrap(), fs::read(&cli_out).unwrap());

    // A tab with unsaved strokes exports its state through the one render:
    // the second stroke changes exactly the samples its footprint covers, the
    // session and the `.ffp` on disk stay as they were, and the image is the
    // export of the same strokes once saved.
    let edited = build_edited("export_live_edits");
    let ffp_before = fs::read(&edited.project).unwrap();
    let mut controller = scratch_controller("export_live_edits");
    common::open_and_settle(&mut controller, std::slice::from_ref(&edited.project));
    let root_row = row_index(&controller, &edited.root);
    controller.select_checkpoint_row(root_row);
    common::enter_editor(&mut controller);
    let first = Op::BrushStroke {
        source_id: 1,
        path: vec![Point { x: 1, y: 1 }],
        radius: 0.5,
        softness: 0.0,
    };
    controller
        .edit_session_mut()
        .unwrap()
        .append(first.clone())
        .expect("the stroke inside the map applies");

    let dir = common::tree("export_live_edits_out");
    controller.checkpoint_export_requested();
    let baseline = dir.join("baseline.tif");
    let picked = baseline.clone();
    controller.export_confirmed(move |_proposed| Some(picked.clone()));
    wait_export(&mut controller);

    let repaint = Op::BrushStroke {
        source_id: 2,
        path: vec![Point { x: 1, y: 1 }],
        radius: 0.5,
        softness: 0.0,
    };
    controller
        .edit_session_mut()
        .unwrap()
        .append(repaint.clone())
        .expect("the stroke inside the map applies");
    let op_count_before = controller.edit_session().unwrap().op_count();
    let held_before = controller.held_checkpoints().len();

    controller.checkpoint_export_requested();
    let (_, what) = controller.export_copy().unwrap();
    assert_eq!(what, "Exporting your current unsaved retouch edits.");
    let edited_out = dir.join("edited.tif");
    let picked = edited_out.clone();
    controller.export_confirmed(move |_proposed| Some(picked.clone()));
    wait_export(&mut controller);

    assert_eq!(
        controller.edit_session().unwrap().op_count(),
        op_count_before,
        "export must not touch the edit session"
    );
    assert_eq!(
        controller.held_checkpoints().len(),
        held_before,
        "export must not add a held row"
    );
    assert_eq!(
        fs::read(&edited.project).unwrap(),
        ffp_before,
        "export must not touch the .ffp on disk"
    );

    let base_samples = samples(&baseline);
    let edited_samples = samples(&edited_out);
    assert_ne!(
        base_samples, edited_samples,
        "the repaint must change at least one sample"
    );
    const SPP: usize = 3;
    let footprint = blend::footprint(&repaint, ZMapInfo::new(MW, MH, FRAMES as u32).unwrap())
        .unwrap()
        .expect("the stroke meets the map");
    let inside: std::collections::HashSet<usize> = (footprint.y..footprint.y + footprint.height)
        .flat_map(|y| {
            (footprint.x..footprint.x + footprint.width).flat_map(move |x| {
                let pixel = (y * MW + x) as usize;
                (pixel * SPP)..(pixel * SPP + SPP)
            })
        })
        .collect();
    let changed: std::collections::HashSet<usize> = base_samples
        .iter()
        .zip(edited_samples.iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, _)| i)
        .collect();
    assert!(
        changed.is_subset(&inside),
        "only samples inside the repaint's footprint may differ: {changed:?} vs {inside:?}"
    );

    // The same strokes appended to a copy of the project and exported as that
    // checkpoint give the same file.
    let copy = edited.project.with_file_name("appended.ffp");
    fs::copy(&edited.project, &copy).unwrap();
    let appended = append_ops_onto(
        &copy,
        &edited.root,
        vec![first, repaint],
        None,
        1_756_468_980,
    )
    .unwrap()
    .checkpoint;
    let mut copy_project = Project::open(&copy).unwrap();
    let appended_out = dir.join("appended.tif");
    export_checkpoint(
        &mut copy_project,
        &appended,
        &appended_out,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .unwrap();
    assert_eq!(
        fs::read(&edited_out).unwrap(),
        fs::read(&appended_out).unwrap(),
        "the pending export is the saved export of the same strokes"
    );

    // A never-saved tab exports its pending strokes too, writing no project;
    // after Save the same state exports the same samples.
    let mut controller = scratch_controller("export_live_unsaved");
    let frames = open_mantis64_tab(&mut controller, "export_live_unsaved", 2);
    assert!(controller.start_fuse());
    settle_all(&mut controller);
    common::enter_editor(&mut controller);
    controller
        .edit_session_mut()
        .unwrap()
        .append(Op::BrushStroke {
            source_id: 1,
            path: vec![Point { x: 30, y: 30 }],
            radius: 8.0,
            softness: 0.5,
        })
        .expect("the stroke inside the map applies");
    let before: std::collections::BTreeSet<PathBuf> = fs::read_dir(&frames)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    controller.checkpoint_export_requested();
    let unsaved_out = dir.join("unsaved.tif");
    let picked = unsaved_out.clone();
    controller.export_confirmed(move |_proposed| Some(picked.clone()));
    wait_export(&mut controller);
    assert!(
        unsaved_out.exists(),
        "a never-saved tab exports its strokes"
    );
    let after: std::collections::BTreeSet<PathBuf> = fs::read_dir(&frames)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(before, after, "the export writes no project");
    assert_ne!(
        samples(&unsaved_out),
        samples(&held_out),
        "the stroke must change the held result"
    );

    let saved_path = save_to_proposed(&mut controller);
    let mut project = Project::open(&saved_path).unwrap();
    let stroked = project
        .history()
        .all()
        .iter()
        .find(|checkpoint| !checkpoint.ops.is_empty())
        .expect("Save appends the pending stroke")
        .id
        .clone();
    let saved_out = dir.join("unsaved-saved.tif");
    export_checkpoint(
        &mut project,
        &stroked,
        &saved_out,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .unwrap();
    assert_eq!(samples(&unsaved_out), samples(&saved_out));
}

#[test]
fn refused_export_is_shown_in_the_modal() {
    let edited = build_edited("export_refused");
    let mut controller = scratch_controller("export_refused");
    common::open_and_settle(&mut controller, std::slice::from_ref(&edited.project));
    let row = row_index(&controller, &edited.edited);
    controller.select_checkpoint_row(row);

    fs::remove_file(&edited.sources[0]).unwrap();

    controller.checkpoint_export_requested();
    let dir = common::tree("export_refused_out");
    let out = dir.join("refused.tif");
    let picked = out.clone();
    controller.export_confirmed(move |_proposed| Some(picked.clone()));
    wait_export(&mut controller);

    assert_eq!(controller.export_stage(), "refused");
    assert!(
        controller.export_can_locate(),
        "a missing source offers Locate frames…"
    );
    let lines = controller.export_refusal_lines();
    // One bare `"<name> — missing"` line per source, no heading of its own --
    // `dialogs.slint`'s static heading is the only heading. `frame-00.tif`
    // is `edited.sources[0]`, removed above.
    assert_eq!(
        lines,
        &["frame-00.tif — missing".to_string()],
        "no heading, exactly one line per missing source"
    );
    assert!(!out.exists(), "a refused export leaves no file");
    assert!(
        !dir.join("refused.tif.partial").exists(),
        "a refused export leaves no .partial"
    );

    // The shell stays usable: the modal closes and the row is still
    // exportable in principle.
    controller.cancel_export();
    assert!(controller.export_copy().is_none());
    assert!(controller.export_enabled());
}

#[test]
fn held_row_exports_from_a_never_saved_tab_without_its_sources() {
    let mut controller = scratch_controller("export_held_no_sources");
    let root = open_mantis64_tab(&mut controller, "export_held_no_sources", 2);
    assert!(controller.start_fuse());
    settle_all(&mut controller);
    assert_eq!(controller.checkpoint_count(), Some(1));

    // The frames go away; the tab has never been saved, so no `.ffp` exists.
    let away = root.with_file_name("export_held_no_sources_away");
    fs::rename(&root, &away).unwrap();

    controller.checkpoint_export_requested();
    let (_, what) = controller.export_copy().expect("the modal must still open");
    assert_eq!(what, "Exporting the unsaved fuse result held in this tab.");
    let dir = common::tree("export_held_no_sources_out");
    let held_out = dir.join("held.tif");
    let picked = held_out.clone();
    controller.export_confirmed(move |_proposed| Some(picked.clone()));
    wait_export(&mut controller);
    assert!(
        controller.export_copy().is_none(),
        "the modal closes once the write succeeds"
    );
    assert!(held_out.exists());
    assert!(!dir.join("held.tif.partial").exists());
    assert_eq!(controller.held_checkpoints().len(), 1);

    // The same result after Save, exported through the CLI's entry point.
    fs::rename(&away, &root).unwrap();
    let saved_path = save_to_proposed(&mut controller);
    let mut project = Project::open(&saved_path).unwrap();
    let id = project.history().all()[0].id.clone();
    let cli_out = dir.join("saved.tif");
    export_checkpoint(
        &mut project,
        &id,
        &cli_out,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .unwrap();
    assert_eq!(samples(&held_out), samples(&cli_out));
}

#[test]
fn saved_unedited_row_exports_its_stored_composite_without_sources() {
    let edited = build_edited("export_saved_no_sources");
    let mut controller = scratch_controller("export_saved_no_sources");
    common::open_and_settle(&mut controller, std::slice::from_ref(&edited.project));
    let ffp_before = fs::read(&edited.project).unwrap();
    for path in &edited.sources {
        fs::remove_file(path).unwrap();
    }
    let row = row_index(&controller, &edited.root);
    controller.select_checkpoint_row(row);
    controller.checkpoint_export_requested();
    let dir = common::tree("export_saved_no_sources_out");
    let out = dir.join("root.tif");
    export_to(&mut controller, &out);
    assert!(
        controller.export_copy().is_none(),
        "the modal closes once the write succeeds"
    );

    let mut project = Project::open(&edited.project).unwrap();
    let stored = dir.join("stored.tif");
    materialize_plane(&mut project, Plane::Composite, &stored).unwrap();
    assert_eq!(samples(&out), samples(&stored));
    assert_eq!(fs::read(&edited.project).unwrap(), ffp_before);
}

#[test]
fn unwritable_destination_names_the_cause_in_the_modal() {
    let edited = build_edited("export_refused_unwritable");
    let mut controller = scratch_controller("export_refused_unwritable");
    common::open_and_settle(&mut controller, std::slice::from_ref(&edited.project));
    let row = row_index(&controller, &edited.root);
    controller.select_checkpoint_row(row);

    controller.checkpoint_export_requested();
    let dir = common::tree("export_refused_unwritable_out");
    let out = dir.join("no-such-directory").join("out.tif");
    let picked = out.clone();
    controller.export_confirmed(move |_proposed| Some(picked.clone()));
    wait_export(&mut controller);

    assert_eq!(controller.export_stage(), "refused");
    let lines = controller.export_refusal_lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_ne!(
        lines[0], "Cannot export this position.",
        "an unwritable destination must name its real cause, not the generic fallback: {lines:?}"
    );
    assert!(
        lines[0].contains("write"),
        "the line must say what went wrong: {lines:?}"
    );
    assert!(!out.exists());
}

// ---- format, settings and refusal tests ------------------------------------

use focusforge_core::params::ParamKind;
use focusforge_core::project::export::{
    JpegSubsampling, PngBits, PngCompression, TiffBits, TiffCompression, params_for,
};
use focusforge_gui::export_settings::{self, ExportChoice};

/// A fresh controller with the edited checkpoint's row selected and the
/// export modal open.
fn open_edited_modal(name: &str) -> (AppController, Edited) {
    let edited = build_edited(name);
    let mut controller = scratch_controller(name);
    common::open_and_settle(&mut controller, std::slice::from_ref(&edited.project));
    let row = row_index(&controller, &edited.edited);
    controller.select_checkpoint_row(row);
    controller.checkpoint_export_requested();
    (controller, edited)
}

/// Confirms the open modal at `path` and waits for the write to settle.
fn export_to(controller: &mut AppController, path: &Path) {
    let picked = path.to_path_buf();
    controller.export_confirmed(move |_| Some(picked));
    wait_export(controller);
}

fn quality_fraction(quality: i64) -> f32 {
    (quality - 1) as f32 / 99.0
}

fn metadata_off() -> MetadataOptions {
    MetadataOptions {
        exif: false,
        icc: false,
        xmp: false,
        iptc: false,
    }
}

#[test]
fn format_switch_shows_only_that_formats_settings() {
    let (mut controller, _edited) = open_edited_modal("export_format_switch");
    for format in export_settings::FORMATS {
        let label = format.id().to_uppercase();
        controller.export_select_format(&label);
        let view = controller.export_view().expect("the modal is open");
        assert_eq!(view.segments, ["TIFF", "PNG", "JPEG"]);
        assert_eq!(view.selected, label);
        assert_eq!(view.heading, format!("{label} SETTINGS"));
        assert!(view.metadata, "Metadata is on at first open");
        assert_eq!(view.slider_group, format == ExportFormat::Jpeg);
        let schema = params_for(format);
        assert_eq!(view.rows.len(), schema.len(), "exactly {label}'s rows");
        for (row, desc) in view.rows.iter().zip(schema) {
            assert_eq!(row.id, desc.id);
            assert_eq!(row.label, desc.label);
            if let ParamKind::Enum { variants } = &desc.kind {
                let ids: Vec<&str> = variants.iter().map(|v| v.id.as_str()).collect();
                let labels: Vec<&str> = variants.iter().map(|v| v.label.as_str()).collect();
                assert_eq!(row.choice_ids, ids);
                assert_eq!(row.choices, labels);
            }
        }
    }

    // The quality slider's fraction maps back to the same integer.
    controller.export_select_format("JPEG");
    for quality in [1, 50, 85, 95, 100] {
        controller.export_set_quality_fraction(quality_fraction(quality));
        let view = controller.export_view().unwrap();
        assert_eq!(view.rows[0].value_text, quality.to_string());
        controller.export_set_quality_fraction(view.rows[0].fraction);
        assert_eq!(
            controller.export_view().unwrap().rows[0].value_text,
            quality.to_string(),
            "fraction {} maps back",
            view.rows[0].fraction
        );
    }
}

#[test]
fn modal_export_equals_cli_export_for_every_format() {
    let edited = build_edited("export_every_format");
    let mut controller = scratch_controller("export_every_format");
    common::open_and_settle(&mut controller, std::slice::from_ref(&edited.project));
    let row = row_index(&controller, &edited.edited);
    controller.select_checkpoint_row(row);
    let mut project = Project::open(&edited.project).unwrap();
    let dir = common::tree("export_every_format_out");

    struct Case {
        format: &'static str,
        sets: Vec<(&'static str, &'static str)>,
        quality: Option<i64>,
        metadata: bool,
        literal: ExportOptions,
    }
    let tiff = |bits: &'static str, compression: &'static str, b, c| Case {
        format: "TIFF",
        sets: vec![("tiff-bits", bits), ("tiff-compression", compression)],
        quality: None,
        metadata: true,
        literal: ExportOptions::Tiff {
            bits: b,
            compression: c,
        },
    };
    let png = |bits: &'static str, compression: &'static str, b, c| Case {
        format: "PNG",
        sets: vec![("png-bits", bits), ("png-compression", compression)],
        quality: None,
        metadata: true,
        literal: ExportOptions::Png {
            bits: b,
            compression: c,
        },
    };
    let jpeg = |quality: i64, subsampling: &'static str, s| Case {
        format: "JPEG",
        sets: vec![("jpeg-subsampling", subsampling)],
        quality: Some(quality),
        metadata: true,
        literal: ExportOptions::Jpeg {
            quality,
            subsampling: s,
        },
    };
    let mut cases = vec![
        tiff("8", "none", TiffBits::Eight, TiffCompression::None),
        tiff("8", "lzw", TiffBits::Eight, TiffCompression::Lzw),
        tiff("8", "deflate", TiffBits::Eight, TiffCompression::Deflate),
        tiff("16", "none", TiffBits::Sixteen, TiffCompression::None),
        tiff("16", "lzw", TiffBits::Sixteen, TiffCompression::Lzw),
        tiff("16", "deflate", TiffBits::Sixteen, TiffCompression::Deflate),
        png("8", "fast", PngBits::Eight, PngCompression::Fast),
        png("8", "default", PngBits::Eight, PngCompression::Default),
        png("8", "best", PngBits::Eight, PngCompression::Best),
        png("16", "fast", PngBits::Sixteen, PngCompression::Fast),
        png("16", "default", PngBits::Sixteen, PngCompression::Default),
        png("16", "best", PngBits::Sixteen, PngCompression::Best),
    ];
    for quality in [1, 85, 100] {
        cases.push(jpeg(quality, "444", JpegSubsampling::S444));
        cases.push(jpeg(quality, "420", JpegSubsampling::S420));
    }
    let mut no_metadata = tiff("16", "none", TiffBits::Sixteen, TiffCompression::None);
    no_metadata.metadata = false;
    cases.push(no_metadata);

    for (i, case) in cases.iter().enumerate() {
        controller.checkpoint_export_requested();
        controller.export_select_format(case.format);
        for (id, variant) in &case.sets {
            controller.export_choose(id, variant);
        }
        if let Some(quality) = case.quality {
            controller.export_set_quality_fraction(quality_fraction(quality));
        }
        if controller.export_view().unwrap().metadata != case.metadata {
            controller.export_toggle_metadata();
        }
        let extension = case.literal.format().id();
        let extension = if extension == "jpeg" {
            "jpg"
        } else {
            extension
        };
        let gui_out = dir.join(format!("gui-{i}.{extension}"));
        export_to(&mut controller, &gui_out);
        assert!(
            controller.export_copy().is_none(),
            "case {i} closes the modal"
        );

        let core_out = dir.join(format!("core-{i}.{extension}"));
        let metadata = if case.metadata {
            MetadataOptions::default()
        } else {
            metadata_off()
        };
        export_checkpoint(
            &mut project,
            &edited.edited,
            &core_out,
            &case.literal,
            &metadata,
        )
        .unwrap_or_else(|e| panic!("case {i} {:?}: {e}", case.literal));
        assert_eq!(
            fs::read(&gui_out).unwrap(),
            fs::read(&core_out).unwrap(),
            "case {i}: {:?}",
            case.literal
        );
    }

    // The opening defaults are the CLI's defaults, 4:2:0 JPEG included.
    controller.checkpoint_export_requested();
    controller.export_select_format("JPEG");
    let fresh = ExportChoice::default();
    assert_eq!(
        fresh.jpeg,
        ExportOptions::Jpeg {
            quality: 95,
            subsampling: JpegSubsampling::S420,
        }
    );
    controller.cancel_export();
}

#[test]
fn destination_extension_follows_the_format() {
    let (mut controller, _edited) = open_edited_modal("export_extension");
    for (label, extension) in [("TIFF", "tif"), ("PNG", "png"), ("JPEG", "jpg")] {
        controller.export_select_format(label);
        let mut proposed = None;
        controller.export_confirmed(|p| {
            proposed = Some(p.to_path_buf());
            None
        });
        let proposed = proposed.expect("the picker is asked for a destination");
        assert_eq!(
            proposed.extension().and_then(|e| e.to_str()),
            Some(extension),
            "{label}: {proposed:?}"
        );
    }
}

fn settings_text(dir: &Path) -> String {
    fs::read_to_string(dir.join("settings.toml")).unwrap_or_default()
}

#[test]
fn last_choice_is_restored_and_cancel_stores_nothing() {
    let (mut controller, edited) = open_edited_modal("export_last_choice");
    let cfg = common::tree("export_last_choice_cfg");
    let original = "# hand note\n[input]\nzoom = 1\n";
    fs::write(cfg.join("settings.toml"), original).unwrap();
    controller.install_recent(Vec::new(), cfg.clone());

    // Cancelling the modal stores nothing and the next one opens as before.
    controller.export_select_format("PNG");
    controller.cancel_export();
    assert_eq!(settings_text(&cfg), original);
    controller.checkpoint_export_requested();
    assert_eq!(controller.export_view().unwrap().selected, "TIFF");

    // A successful export stores the whole choice.
    controller.export_select_format("JPEG");
    controller.export_set_quality_fraction(quality_fraction(90));
    controller.export_choose("jpeg-subsampling", "444");
    controller.export_toggle_metadata();
    let out = common::tree("export_last_choice_out").join("a.jpg");
    export_to(&mut controller, &out);
    let stored = settings_text(&cfg);
    assert!(stored.starts_with(original), "{stored}");
    assert!(stored.contains("[export]"), "{stored}");

    let expect_restored = |controller: &AppController| {
        let view = controller.export_view().expect("the modal is open");
        assert_eq!(view.selected, "JPEG");
        assert_eq!(view.rows[0].value_text, "90");
        assert_eq!(view.rows[1].value_text, "4:4:4");
        assert!(!view.metadata);
    };
    controller.checkpoint_export_requested();
    expect_restored(&controller);
    controller.cancel_export();

    // After a restart: a new controller reads the file from disk.
    let mut restarted = scratch_controller("export_last_choice_restart");
    common::open_and_settle(&mut restarted, std::slice::from_ref(&edited.project));
    restarted.install_export_choice(export_settings::load(&cfg));
    let row = row_index(&restarted, &edited.edited);
    restarted.select_checkpoint_row(row);
    restarted.checkpoint_export_requested();
    expect_restored(&restarted);
}

#[test]
fn retry_after_locate_exports_the_dialogs_choice_and_a_refusal_stores_nothing() {
    let (mut controller, edited) = open_edited_modal("export_retry_choice");
    let cfg = common::tree("export_retry_choice_cfg");
    controller.install_recent(Vec::new(), cfg.clone());

    controller.export_select_format("JPEG");
    controller.export_set_quality_fraction(quality_fraction(90));
    let original = fs::read(&edited.sources[0]).unwrap();
    fs::remove_file(&edited.sources[0]).unwrap();
    let out = common::tree("export_retry_choice_out").join("retry.jpg");
    export_to(&mut controller, &out);
    assert_eq!(controller.export_stage(), "refused");
    assert!(controller.export_can_locate());
    assert!(
        !cfg.join("settings.toml").exists(),
        "a refused export stores nothing"
    );

    fs::write(&edited.sources[0], original).unwrap();
    let folder = edited.sources[0].parent().unwrap().to_path_buf();
    controller
        .export_locate_frames(folder)
        .expect("a verification starts")
        .join()
        .expect("the locate worker did not panic");
    controller.settle_locate();
    wait_export(&mut controller);
    assert!(controller.export_copy().is_none(), "the retry succeeded");

    let mut project = Project::open(&edited.project).unwrap();
    let core_out = out.with_file_name("core.jpg");
    export_checkpoint(
        &mut project,
        &edited.edited,
        &core_out,
        &ExportOptions::Jpeg {
            quality: 90,
            subsampling: JpegSubsampling::S420,
        },
        &MetadataOptions::default(),
    )
    .unwrap();
    assert_eq!(
        fs::read(&out).unwrap(),
        fs::read(&core_out).unwrap(),
        "the retry wrote the dialog's JPEG choice, not the defaults"
    );
    assert!(
        settings_text(&cfg).contains("jpeg"),
        "the success stores it"
    );
}

#[test]
fn broken_settings_file_falls_back_to_defaults() {
    // An out-of-range quality and an unknown format drop alone.
    let cfg = common::tree("export_broken_cfg");
    fs::write(
        cfg.join("settings.toml"),
        "[export]\nformat = \"webp\"\n[export.jpeg]\njpeg-quality = 0\njpeg-subsampling = \"444\"\n",
    )
    .unwrap();
    let (mut controller, _edited) = open_edited_modal("export_broken");
    controller.cancel_export();
    controller.install_export_choice(export_settings::load(&cfg));
    controller.checkpoint_export_requested();
    assert_eq!(controller.export_view().unwrap().selected, "TIFF");
    controller.export_select_format("JPEG");
    let view = controller.export_view().unwrap();
    assert_eq!(view.rows[0].value_text, "95", "quality falls back alone");
    assert_eq!(view.rows[1].value_text, "4:4:4", "the valid chroma stays");
    controller.cancel_export();

    // A settings file that does not parse stays byte-unchanged and the
    // export still succeeds.
    let broken_cfg = common::tree("export_broken_cfg2");
    let broken = "[export\nformat = ";
    fs::write(broken_cfg.join("settings.toml"), broken).unwrap();
    controller.install_recent(Vec::new(), broken_cfg.clone());
    controller.checkpoint_export_requested();
    let out = common::tree("export_broken_out").join("a.tif");
    export_to(&mut controller, &out);
    assert!(out.exists(), "the export succeeds");
    assert_eq!(settings_text(&broken_cfg), broken);

    // An unwritable config directory leaves the export successful.
    let blocker = common::tree("export_broken_blocker").join("file");
    fs::write(&blocker, "x").unwrap();
    controller.install_recent(Vec::new(), blocker.join("sub"));
    controller.checkpoint_export_requested();
    let out = out.with_file_name("b.tif");
    export_to(&mut controller, &out);
    assert!(out.exists(), "the choice is simply unremembered");
}

#[test]
fn a_format_refusal_names_its_cause_and_offers_close_only() {
    let edited = build_edited_with_samples("export_four_samples", 4);
    let mut controller = scratch_controller("export_four_samples");
    common::open_and_settle(&mut controller, std::slice::from_ref(&edited.project));
    let row = row_index(&controller, &edited.edited);
    controller.select_checkpoint_row(row);
    controller.checkpoint_export_requested();
    controller.export_select_format("JPEG");
    let out = common::tree("export_four_samples_out").join("a.jpg");
    export_to(&mut controller, &out);

    assert_eq!(controller.export_stage(), "refused");
    assert_eq!(
        controller.export_refusal_lines(),
        &[
            "PNG and JPEG can only save grey or colour (RGB) images; this one has extra channels."
                .to_string()
        ]
    );
    assert!(
        !controller.export_can_locate(),
        "a format refusal offers Close only"
    );
    assert!(!out.exists());
    controller.cancel_export();
}

#[test]
fn save_and_export_dialogs_run_unlocked_for_export() {
    use std::sync::{Arc, Mutex};

    let mut controller = scratch_controller("export_dialog_unlocked");
    open_fused_tab(&mut controller, "export_dialog_unlocked", 3);
    controller.checkpoint_export_requested();
    controller.export_select_format("TIFF");
    let controller = Arc::new(Mutex::new(controller));

    let (tab_id, format, proposed) = controller
        .lock()
        .unwrap()
        .export_destination_request()
        .expect("a ready modal proposes a destination");
    assert_eq!(format, ExportFormat::Tiff);
    assert!(
        controller.try_lock().is_ok(),
        "the save dialog runs with the controller free"
    );

    let chosen = common::tree("export_dialog_unlocked_elsewhere").join("renamed.tif");
    assert_ne!(chosen.file_name(), proposed.file_name());
    let mut controller = controller.lock().unwrap();
    controller.export_destination_chosen(tab_id, format, chosen.clone());
    assert!(controller.take_export_started());
    wait_export(&mut controller);
    assert!(chosen.is_file(), "{} must be written", chosen.display());
    assert!(!proposed.exists(), "nothing is written at the proposal");
}

#[test]
fn stale_dialog_answer_is_dropped_for_export() {
    let mut controller = scratch_controller("export_dialog_stale");
    open_fused_tab(&mut controller, "export_dialog_stale", 3);
    let out = common::tree("export_dialog_stale_out");
    let target = |name: &str| out.join(name);

    // The modal was cancelled while the dialog was up.
    controller.checkpoint_export_requested();
    controller.export_select_format("TIFF");
    let (tab_id, format, _) = controller.export_destination_request().unwrap();
    controller.cancel_export();
    controller.export_destination_chosen(tab_id, format, target("cancelled.tif"));
    assert!(!controller.take_export_started());
    assert!(!target("cancelled.tif").exists());

    // The format changed while the dialog was up: the path was chosen for
    // another format, and the modal stays as the user left it.
    controller.checkpoint_export_requested();
    controller.export_select_format("TIFF");
    let (tab_id, format, _) = controller.export_destination_request().unwrap();
    controller.export_select_format("PNG");
    controller.export_destination_chosen(tab_id, format, target("switched.tif"));
    assert!(!controller.take_export_started());
    assert!(!target("switched.tif").exists());
    assert_eq!(controller.export_format(), ExportFormat::Png);
    assert_eq!(controller.export_stage(), "ready");

    // The modal already moved to writing: a second answer starts nothing.
    controller.export_select_format("TIFF");
    let (tab_id, format, _) = controller.export_destination_request().unwrap();
    controller.export_destination_chosen(tab_id, format, target("first.tif"));
    assert!(controller.take_export_started());
    controller.export_destination_chosen(tab_id, format, target("second.tif"));
    assert!(!controller.take_export_started());
    wait_export(&mut controller);
    assert!(target("first.tif").is_file());
    assert!(!target("second.tif").exists());

    // A cancelled dialog leaves the modal open on `Ready`.
    controller.checkpoint_export_requested();
    controller.export_confirmed(|_| None);
    assert_eq!(controller.export_stage(), "ready");
    assert!(!controller.take_export_started());
    assert!(controller.export_destination_request().is_some());
}
