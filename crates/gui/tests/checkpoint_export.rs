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
    Metrics, Op, Point, Project, SealRequest, append_ops_onto, seal_at,
};
use focusforge_gui::checkpoints::local_date_and_time;
use focusforge_gui::{AppController, OpenOutcome, ShellMode};

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
    assert!(controller.save_active_project(|proposed| {
        target = Some(proposed.to_path_buf());
        target.clone()
    }));
    target.expect("a never-saved tab asks where to save")
}

fn wait_for_render(controller: &mut AppController) {
    let start = Instant::now();
    while !controller.settle_checkpoint_render() {
        assert!(
            start.elapsed() < SETTLE,
            "the checkpoint render did not settle"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_export(controller: &mut AppController) {
    let start = Instant::now();
    while !controller.settle_export() {
        assert!(start.elapsed() < SETTLE, "the export did not settle");
        std::thread::sleep(Duration::from_millis(5));
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
        focusforge_core::project::ZMapStorage::Bounded,
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
    assert!(controller.set_mode(ShellMode::Editor));
    wait_for_render(&mut controller);
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
    controller.export_confirmed(move |_proposed| Some(picked.clone()));
    wait_export(&mut controller);
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

    // A tab with unsealed edits exports the gather of its edited indices,
    // differing from its row's own export in exactly the edited pixels,
    // and changes neither the session's op count nor the `.ffp` on disk.
    let edited = build_edited("export_live_edits");
    let ffp_before = fs::read(&edited.project).unwrap();
    let mut controller = scratch_controller("export_live_edits");
    common::open_and_settle(&mut controller, std::slice::from_ref(&edited.project));
    let root_row = row_index(&controller, &edited.root);
    controller.select_checkpoint_row(root_row);
    assert!(controller.set_mode(ShellMode::Editor));
    wait_for_render(&mut controller);
    // A fresh session (0 ops) replays to exactly the row's own Z-Map, so
    // this is also what `baseline.tif` (the row's own, non-live export,
    // below) gathers.
    let indices_before = controller.edit_session().unwrap().indices().to_vec();

    let dir = common::tree("export_live_edits_out");
    controller.checkpoint_export_requested();
    let baseline = dir.join("baseline.tif");
    let picked = baseline.clone();
    controller.export_confirmed(move |_proposed| Some(picked.clone()));
    wait_export(&mut controller);

    // Pixel (1, 1)'s own index in `build_edited`'s root Z-Map is `(1 * MW +
    // 1) % FRAMES == 0`; painting a different source is what actually
    // changes the gathered sample below, unlike `dab()`'s own source 0.
    let repaint = Op::BrushStroke {
        source_id: 1,
        path: vec![Point { x: 1, y: 1 }],
        radius: 0.5,
        softness: 0.0,
    };
    controller
        .edit_session_mut()
        .unwrap()
        .append(repaint)
        .expect("the stroke inside the map applies");
    let indices_after = controller.edit_session().unwrap().indices().to_vec();
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
        "the dab must change at least one sample"
    );

    // The exact set of samples that may differ: every channel of every
    // pixel whose gathered source index actually changed, derived
    // structurally from the session's own Z-Map rather than assumed from
    // the brush's radius.
    const SPP: usize = 3;
    let expected: std::collections::HashSet<usize> = indices_before
        .iter()
        .zip(indices_after.iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .flat_map(|(pixel, _)| (pixel * SPP)..(pixel * SPP + SPP))
        .collect();
    assert!(
        !expected.is_empty(),
        "the stroke must change at least one Z-Map index"
    );
    let actual: std::collections::HashSet<usize> = base_samples
        .iter()
        .zip(edited_samples.iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        actual, expected,
        "exactly the edited pixels' samples must differ"
    );
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
fn held_row_missing_source_is_refused_per_frame() {
    // A held row has no stored fingerprint (plain paths, `seal.rs`'s
    // `HeldCheckpoint::sources`), so it can only ever report `missing`, the
    // same per-frame line format a saved row's `RenderSourcesUnavailable`
    // produces -- via `ExportSourcesUnavailable`'s own real path.
    let mut controller = scratch_controller("export_refused_held");
    let root = open_mantis64_tab(&mut controller, "export_refused_held", 2);
    assert!(controller.start_fuse());
    settle_all(&mut controller);
    assert_eq!(controller.checkpoint_count(), Some(1));

    fs::remove_file(root.join("00.tif")).unwrap();

    controller.checkpoint_export_requested();
    let (_, what) = controller.export_copy().expect("the modal must still open");
    assert_eq!(what, "Exporting the unsaved fuse result held in this tab.");
    let dir = common::tree("export_refused_held_out");
    let out = dir.join("held-refused.tif");
    let picked = out.clone();
    controller.export_confirmed(move |_proposed| Some(picked.clone()));
    wait_export(&mut controller);

    assert_eq!(controller.export_stage(), "refused");
    assert_eq!(
        controller.export_refusal_lines(),
        &["00.tif — missing".to_string()]
    );
    assert!(!out.exists());
    assert!(!dir.join("held-refused.tif.partial").exists());
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
