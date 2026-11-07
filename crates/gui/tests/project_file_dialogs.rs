// SPDX-License-Identifier: GPL-3.0-or-later

//! Opening a `.ffp` renders three severities -- refusal, blocking choice,
//! warning -- at the shell, and `Locate frames…` verifies a folder
//! by content hash rather than by name.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use focusforge_core::align::Registration;
use focusforge_core::io::{Rect, TiffSpec, TiffWriter};
use focusforge_core::params::ParamValues;
use focusforge_core::project::container::{ContainerWriter, Index};
use focusforge_core::project::{self, Metrics, SealRequest};
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};
use focusforge_gui::entry::LocateTarget;
use focusforge_gui::{AppController, OpenOutcome, RefusalAction};

const SPP: u32 = 3;
const W: u32 = 8;
const H: u32 = 8;

fn full() -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: W,
        height: H,
    }
}

fn write_frame(path: &Path, seed: u16) {
    let mut writer = TiffWriter::create(
        path,
        TiffSpec {
            width: W,
            height: H,
            samples_per_pixel: SPP,
            description: None,
        },
    )
    .unwrap();
    let samples: Vec<u16> = (0..(W * H * SPP) as usize)
        .map(|i| (i as u16).wrapping_add(seed))
        .collect();
    writer.write_tile(full(), &samples).unwrap();
    writer.finish().unwrap();
}

/// A sealed project, `count` `Relative` source frames written directly
/// inside its own directory -- moving or damaging them on disk is exactly
/// what a user could do to a copy.
fn sealed(dir_name: &str, count: usize) -> (PathBuf, PathBuf) {
    let dir = common::tree(dir_name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let composite = staging.join("composite.tif");
    write_frame(&composite, 0);
    let zmap_path = staging.join("zmap.tif");
    let mut map =
        ZMapWriter::create(&zmap_path, ZMapInfo::new(W, H, count as u32).unwrap()).unwrap();
    map.write_tile(full(), &vec![0u16; (W * H) as usize])
        .unwrap();
    map.finish().unwrap();

    let sources: Vec<PathBuf> = (0..count)
        .map(|i| {
            let p = dir.join(format!("frame-{i:02}.tif"));
            write_frame(&p, (i as u16) * 97 + 1);
            p
        })
        .collect();

    let project_path = dir.join("demo.ffp");
    project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap_path,
        sources: &sources,
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        registration: Registration::default(),
        composite_description: "staged".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");
    (dir, project_path)
}

fn read_entry_bytes(path: &Path, name: &str) -> Vec<u8> {
    Index::read(path).unwrap().read_entry(path, name).unwrap()
}

/// Copies `src` to `dst`, replacing the named entries in `replace` -- a raw
/// rewrite of a real container, so a test can inject a malformed entry the
/// production writer would never produce. `drop` names entries left out of
/// the copy entirely.
fn mutate_container(src: &Path, dst: &Path, replace: &[(String, Vec<u8>)], drop: &[&str]) {
    let index = Index::read(src).unwrap();
    let mut writer = ContainerWriter::create(dst).unwrap();
    for name in index.names() {
        if drop.contains(&name) {
            continue;
        }
        let bytes = match replace.iter().find(|entry| entry.0 == name) {
            Some(entry) => entry.1.clone(),
            None => index.read_entry(src, name).unwrap(),
        };
        writer.put_bytes(name, &bytes).unwrap();
    }
    writer.finish().unwrap();
}

/// Decompresses `original`, flips one byte and recompresses -- a blob whose
/// bytes no longer hash to their entry name, but which still decodes to the
/// declared length: what a bit-rotted or maliciously edited blob actually
/// looks like, never a truncated zstd frame failing for the wrong reason.
fn corrupt_blob_bytes(original: &[u8]) -> Vec<u8> {
    use std::io::{Read, Write};
    let mut decoder = zstd::stream::read::Decoder::new(original).unwrap();
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded).unwrap();
    decoded[0] ^= 0xFF;
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
    encoder.write_all(&decoded).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn import_is_opening() {
    let (_dir, project_path) = sealed("project_file_dialogs_import_is_opening", 2);
    let mut controller = AppController::new();
    let outcome = common::open_and_settle(&mut controller, &[project_path]);
    assert!(
        matches!(outcome, OpenOutcome::Opened(_)),
        "opening a .ffp is exactly Open project…'s own path"
    );
    assert_eq!(controller.project_tabs().len(), 1);
}

#[test]
fn fingerprint_mismatch_blocks_and_does_not_default_to_proceed() {
    let (dir, project_path) = sealed("project_file_dialogs_mismatch", 3);
    let manifest = project::Project::open(&project_path)
        .unwrap()
        .manifest()
        .clone();
    let changed_path = dir.join(&manifest.sources[0].path);
    common::rewrite_changed(&changed_path);

    let mut controller = AppController::new();
    let outcome = common::open_and_settle(&mut controller, std::slice::from_ref(&project_path));
    assert!(matches!(outcome, OpenOutcome::Mismatch(_)));
    assert_eq!(
        controller.project_tabs().len(),
        0,
        "no tab exists while a mismatch is pending"
    );
    let (title, body, _file_name) = controller
        .mismatch_copy()
        .expect("a mismatch must be pending");
    assert_eq!(title, "The source frames are not the same");
    assert!(
        body.contains("3 frames"),
        "names the recorded frame count: {body}"
    );
    assert!(
        body.contains("Z-Map"),
        "names the consequence in Z-Map terms: {body}"
    );

    // Dismiss opens nothing.
    controller.cancel_mismatch();
    assert!(controller.pending_mismatch().is_none());
    assert_eq!(
        controller.project_tabs().len(),
        0,
        "dismissing the mismatch dialog opens nothing"
    );

    // Re-trigger the same mismatch and accept it: exactly one tab.
    let outcome = common::open_and_settle(&mut controller, &[project_path]);
    assert!(matches!(outcome, OpenOutcome::Mismatch(_)));
    controller.open_anyway();
    assert_eq!(
        controller.project_tabs().len(),
        1,
        "Open anyway opens exactly one tab"
    );
    assert!(controller.pending_mismatch().is_none());
}

#[test]
fn unresolvable_sources_warn_and_open() {
    let (_dir, project_path) = sealed("project_file_dialogs_missing", 3);
    let copy_dir = common::tree("project_file_dialogs_missing_copy");
    let copied = copy_dir.join(project_path.file_name().unwrap());
    std::fs::copy(&project_path, &copied).unwrap();
    // The frames are deliberately not copied alongside.

    let mut controller = AppController::new();
    let outcome = common::open_and_settle(&mut controller, &[copied]);
    assert!(matches!(outcome, OpenOutcome::Opened(_)));
    assert_eq!(
        controller.project_tabs().len(),
        1,
        "the project opens despite missing sources"
    );
    assert!(
        !controller.checkpoints_panel().entries.is_empty(),
        "history is listable"
    );
    let (_title, body, _file_name) = controller
        .sources_notice()
        .expect("a missing-sources notice must be raised");
    assert!(body.contains("3 of 3"), "counts the missing frames: {body}");

    // A project whose frames are all present shows neither the modal nor
    // the card, whatever its sources' recorded resolution.
    let (_dir2, clean_path) = sealed("project_file_dialogs_missing_clean", 2);
    let mut controller2 = AppController::new();
    common::open_and_settle(&mut controller2, &[clean_path]);
    assert!(controller2.sources_notice().is_none());
    assert!(controller2.gallery_failure_kind(0).is_none());
}

#[test]
fn missing_sources_wording_is_shared() {
    let (_dir, project_path) = sealed("project_file_dialogs_shared_wording", 3);
    let copy_dir = common::tree("project_file_dialogs_shared_wording_copy");
    let copied = copy_dir.join(project_path.file_name().unwrap());
    std::fs::copy(&project_path, &copied).unwrap();

    let mut controller = AppController::new();
    common::open_and_settle(&mut controller, &[copied]);
    let card_reason = controller.project_cards()[0].reason.clone();
    let (_title, body, _file_name) = controller.sources_notice().unwrap();
    assert!(
        body.starts_with(&card_reason),
        "the dialog's first sentence is the card's reason, verbatim: card={card_reason:?} body={body:?}"
    );

    // Built by one function, for two different counts.
    assert_eq!(
        focusforge_gui::overlay::sources_missing_sentence(1, 5),
        "A project remembers its frames by path. 1 of 5 cannot be found in this location."
    );
    assert_ne!(
        focusforge_gui::overlay::sources_missing_sentence(1, 5),
        focusforge_gui::overlay::sources_missing_sentence(2, 5)
    );
}

#[test]
fn locate_frames_verifies_by_hash() {
    let (dir, project_path) = sealed("project_file_dialogs_locate", 3);
    let copy_dir = common::tree("project_file_dialogs_locate_copy");
    let copied = copy_dir.join(project_path.file_name().unwrap());
    std::fs::copy(&project_path, &copied).unwrap();

    let mut controller = AppController::new();
    common::open_and_settle(&mut controller, &[copied]);
    assert!(controller.sources_notice().is_some());

    // A folder of same-named but different files is refused; card, frames
    // and pending mismatch unchanged.
    let wrong_dir = common::tree("project_file_dialogs_locate_wrong");
    let manifest = project::Project::open(&project_path)
        .unwrap()
        .manifest()
        .clone();
    for source in &manifest.sources {
        let name = Path::new(&source.path).file_name().unwrap();
        std::fs::write(wrong_dir.join(name), b"not the right bytes at all").unwrap();
    }
    let tab_id = controller.tab_id_at(0).unwrap();
    common::locate_and_settle(&mut controller, LocateTarget::Tab(tab_id), wrong_dir);
    assert!(
        controller.sources_notice().is_some(),
        "a wrong folder must not clear the card"
    );
    assert!(controller.last_refusal().is_some());

    // The right folder clears the card and relocates the tab's frames.
    common::locate_and_settle(&mut controller, LocateTarget::Tab(tab_id), dir);
    assert!(controller.sources_notice().is_none());
    assert!(controller.gallery_failure_kind(0).is_none());

    // From the mismatch dialog: the right folder opens the pending project
    // with no accepted mismatch.
    let (dir2, project_path2) = sealed("project_file_dialogs_locate_mismatch", 2);
    let manifest2 = project::Project::open(&project_path2)
        .unwrap()
        .manifest()
        .clone();
    // A separate folder holding the original, byte-identical frames --
    // `dir2` itself is about to have its own copy tampered, so it can no
    // longer verify against the recorded fingerprint.
    let good_dir = common::tree("project_file_dialogs_locate_mismatch_good");
    for source in &manifest2.sources {
        std::fs::copy(dir2.join(&source.path), good_dir.join(&source.path)).unwrap();
    }
    let changed = dir2.join(&manifest2.sources[0].path);
    common::rewrite_changed(&changed);

    let mut controller2 = AppController::new();
    let outcome = common::open_and_settle(&mut controller2, &[project_path2]);
    assert!(matches!(outcome, OpenOutcome::Mismatch(_)));
    common::locate_and_settle(&mut controller2, LocateTarget::Mismatch, good_dir);
    assert!(controller2.pending_mismatch().is_none());
    assert_eq!(
        controller2.project_tabs().len(),
        1,
        "the pending project opens with no accepted mismatch"
    );
}

#[test]
fn locate_frames_targets_a_tab_by_id_not_index() {
    // A: a ready tab with no missing sources, opened first purely so
    // closing it shifts every later tab's own index down.
    let (_a_dir, a_path) = sealed("project_file_dialogs_reorder_a", 2);

    // B and C: each sources-missing, resolvable at their own frames
    // directory, opened after A.
    let (b_dir, b_path) = sealed("project_file_dialogs_reorder_b", 2);
    let b_copy_dir = common::tree("project_file_dialogs_reorder_b_copy");
    let b_copied = b_copy_dir.join(b_path.file_name().unwrap());
    std::fs::copy(&b_path, &b_copied).unwrap();

    let (_c_dir, c_path) = sealed("project_file_dialogs_reorder_c", 2);
    let c_copy_dir = common::tree("project_file_dialogs_reorder_c_copy");
    let c_copied = c_copy_dir.join(c_path.file_name().unwrap());
    std::fs::copy(&c_path, &c_copied).unwrap();

    let mut controller = AppController::new();
    common::open_and_settle(&mut controller, &[a_path]);
    common::open_and_settle(&mut controller, &[b_copied]);
    common::open_and_settle(&mut controller, &[c_copied]);
    assert_eq!(controller.gallery_failure_kind(0), None);
    assert_eq!(controller.gallery_failure_kind(1), Some("sources-missing"));
    assert_eq!(controller.gallery_failure_kind(2), Some("sources-missing"));

    // Start verifying B (currently at index 1) by its own stable id.
    let b_id = controller.tab_id_at(1).unwrap();
    let handle = controller
        .locate_frames(LocateTarget::Tab(b_id), b_dir)
        .expect("a verification starts");

    // Close A (index 0) while the verification is still in flight: B and
    // C both shift one index down before the worker's result lands.
    controller.request_close(0);
    assert_eq!(
        controller.gallery_failure_kind(0),
        Some("sources-missing"),
        "B is now at index 0"
    );
    assert_eq!(
        controller.gallery_failure_kind(1),
        Some("sources-missing"),
        "C is now at index 1"
    );

    handle.join().expect("the locate worker did not panic");
    controller.settle_locate();

    // The verification settles against B, wherever it now sits -- not
    // whichever tab happens to occupy the index it was started at.
    assert_eq!(controller.gallery_failure_kind(0), None, "B's card clears");
    assert_eq!(
        controller.gallery_failure_kind(1),
        Some("sources-missing"),
        "C is untouched"
    );
}

#[test]
fn every_validation_state_is_rendered() {
    // Ready -> tab.
    let (_dir, ready) = sealed("project_file_dialogs_every_ready", 2);
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[ready]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(controller.project_tabs().len(), 1);

    // Mismatch -> mismatch dialog.
    let (dir, mismatch_path) = sealed("project_file_dialogs_every_mismatch", 2);
    let manifest = project::Project::open(&mismatch_path)
        .unwrap()
        .manifest()
        .clone();
    let changed = dir.join(&manifest.sources[0].path);
    common::rewrite_changed(&changed);
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[mismatch_path]),
        OpenOutcome::Mismatch(_)
    ));
    assert!(controller.mismatch_copy().is_some());

    // Missing -> tab + notice.
    let (_dir, missing_source_project) = sealed("project_file_dialogs_every_missing_src", 2);
    let copy_dir = common::tree("project_file_dialogs_every_missing_copy");
    let copied = copy_dir.join(missing_source_project.file_name().unwrap());
    std::fs::copy(&missing_source_project, &copied).unwrap();
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[copied]),
        OpenOutcome::Opened(_)
    ));
    assert_eq!(controller.project_tabs().len(), 1);
    assert!(controller.sources_notice().is_some());

    // Refused -> refusal dialog.
    let bad = common::tree("project_file_dialogs_every_refused").join("bad.ffp");
    std::fs::write(&bad, b"not a zip container").unwrap();
    let mut controller = AppController::new();
    assert!(matches!(
        common::open_and_settle(&mut controller, &[bad]),
        OpenOutcome::Refused(_)
    ));
    assert_eq!(controller.project_tabs().len(), 0);
    assert!(controller.refusal_title().is_some());
}

#[test]
fn refusals_report_their_cause_and_open_nothing() {
    let (dir, project_path) = sealed("project_file_dialogs_refusals", 2);
    let manifest_bytes = read_entry_bytes(&project_path, "manifest.json");
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
    let zmap_blob = manifest["zmap"]["blob"].as_str().unwrap().to_string();
    let zmap_entry = format!("blobs/{zmap_blob}.zst");
    let zmap_bytes = read_entry_bytes(&project_path, &zmap_entry);

    let mut seen: HashMap<&str, String> = HashMap::new();
    let mut check = |name: &'static str, path: PathBuf| {
        let mut controller = AppController::new();
        let outcome = common::open_and_settle(&mut controller, &[path]);
        assert!(
            matches!(outcome, OpenOutcome::Refused(_)),
            "{name}: must refuse"
        );
        assert_eq!(controller.project_tabs().len(), 0, "{name}: no tab");
        assert!(
            controller.pending_mismatch().is_none(),
            "{name}: no pending mismatch"
        );
        let message = controller.last_refusal().unwrap().to_string();
        for forbidden in ["blob", "zip", "delta", "snapshot", "No frames here"] {
            assert!(
                !message.contains(forbidden),
                "{name}: {message:?} names {forbidden:?}"
            );
        }
        seen.insert(name, message);
    };

    let bytes = std::fs::read(&project_path).unwrap();
    let truncated = dir.join("truncated.ffp");
    std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
    check("truncated", truncated);

    let foreign = dir.join("foreign.ffp");
    std::fs::write(&foreign, b"not a zip container at all").unwrap();
    check("foreign", foreign);

    let tampered = dir.join("tampered.ffp");
    mutate_container(
        &project_path,
        &tampered,
        &[(zmap_entry.clone(), corrupt_blob_bytes(&zmap_bytes))],
        &[],
    );
    check("tampered_zmap", tampered);

    let missing_entry = dir.join("missing_entry.ffp");
    mutate_container(&project_path, &missing_entry, &[], &[zmap_entry.as_str()]);
    check("missing_entry", missing_entry);

    assert_ne!(seen["truncated"], seen["foreign"]);
    assert_ne!(seen["truncated"], seen["tampered_zmap"]);
    assert_ne!(seen["foreign"], seen["tampered_zmap"]);
    assert_eq!(
        seen["tampered_zmap"], seen["missing_entry"],
        "hash, missing-entry and structural damage share one sentence"
    );
}

/// The refusal a controller is showing: its own title and what it offers.
fn refusal(controller: &AppController) -> (String, RefusalAction) {
    (
        controller
            .refusal_title()
            .expect("a refusal is raised")
            .to_string(),
        controller.refusal_action().expect("a refusal is raised"),
    )
}

/// Copies every `.tif` of a sealed project's directory into a fresh
/// directory, so the frames can be deleted and then located again.
fn copy_frames(from: &Path, to_name: &str) -> PathBuf {
    let to = common::tree(to_name);
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "tif") {
            std::fs::copy(&path, to.join(path.file_name().unwrap())).unwrap();
        }
    }
    to
}

#[test]
fn refusal_titles_name_their_own_cause() {
    use RefusalAction::{Close, PickFolder};
    let check = |name: &str, controller: &AppController, title: &str, action: RefusalAction| {
        assert_eq!(
            refusal(controller),
            (title.to_string(), action),
            "{name}: the refusal's own title and offer"
        );
    };

    let mut controller = AppController::new();
    let empty = common::tree("refusal_titles_empty");
    common::open_and_settle(&mut controller, &[empty]);
    check("empty folder", &controller, "No frames here", PickFolder);

    let set = common::tree("refusal_titles_sets");
    write_frame(&set.join("00.tif"), 1);
    let note = set.join("notes.txt");
    std::fs::write(&note, b"not a frame").unwrap();
    let mut controller = AppController::new();
    assert!(controller.open_paths(&[set.join("00.tif"), note]).is_none());
    check("text file", &controller, "No frames here", PickFolder);

    #[cfg(unix)]
    if let Some(root) = common::readonly_dir("refusal_titles_unreadable", 0o000, |_| {}) {
        let mut controller = AppController::new();
        common::open_and_settle(&mut controller, &[root.to_path_buf()]);
        check(
            "unreadable folder",
            &controller,
            "This folder cannot be read",
            PickFolder,
        );
    }

    let raw = set.join("raw.dng");
    std::fs::copy(
        common::workspace_root().join("fixtures/blackmagic-micro-dng/frame.dng"),
        &raw,
    )
    .unwrap();
    let mut controller = AppController::new();
    assert!(controller.open_paths(&[set.join("00.tif"), raw]).is_none());
    check(
        "mixed kinds",
        &controller,
        "These images are not one stack",
        Close,
    );

    let other = common::tree("refusal_titles_other_folder");
    write_frame(&other.join("00.tif"), 2);
    let mut controller = AppController::new();
    assert!(
        controller
            .open_paths(&[set.join("00.tif"), other.join("00.tif")])
            .is_none()
    );
    check(
        "several folders",
        &controller,
        "These images are not one stack",
        Close,
    );

    let bigtiff = common::tree("refusal_titles_bigtiff");
    std::fs::write(bigtiff.join("00.tif"), b"II+\0\x08\0\0\0").unwrap();
    std::fs::write(bigtiff.join("01.tif"), b"MM\0+\0\x08\0\0").unwrap();
    let mut controller = AppController::new();
    common::open_and_settle(&mut controller, &[bigtiff]);
    check(
        "BigTIFF header",
        &controller,
        "These frames cannot be read",
        Close,
    );

    let bad = common::tree("refusal_titles_corrupt").join("bad.ffp");
    std::fs::write(&bad, b"not a zip container").unwrap();
    let mut controller = AppController::new();
    common::open_and_settle(&mut controller, &[bad]);
    check(
        "corrupt project",
        &controller,
        "This project cannot be opened",
        Close,
    );

    let (dir, project_path) = sealed("refusal_titles_saved", 2);
    let mut controller = AppController::new();
    common::open_and_settle(&mut controller, &[project_path]);
    let id = controller.tab_id_at(0).unwrap();
    let wrong = common::tree("refusal_titles_wrong_folder");
    for name in ["frame-00.tif", "frame-01.tif"] {
        std::fs::write(wrong.join(name), b"not the right bytes at all").unwrap();
    }
    common::locate_and_settle(&mut controller, LocateTarget::Tab(id), wrong);
    check(
        "wrong locate folder",
        &controller,
        "These are not the project's frames",
        Close,
    );

    std::fs::remove_file(dir.join("frame-00.tif")).unwrap();
    assert!(!controller.start_fuse());
    check(
        "deleted frame",
        &controller,
        "A frame is no longer there",
        RefusalAction::LocateTab(id),
    );

    // The right frames are located, then the project file goes before it is
    // reopened.
    let (dir, project_path) = sealed("refusal_titles_relocated", 2);
    let good = copy_frames(&dir, "refusal_titles_relocated_good");
    common::rewrite_changed(&dir.join("frame-00.tif"));
    let mut controller = AppController::new();
    let outcome = common::open_and_settle(&mut controller, std::slice::from_ref(&project_path));
    assert!(matches!(outcome, OpenOutcome::Mismatch(_)));
    controller
        .locate_frames(LocateTarget::Mismatch, good)
        .expect("a mismatch is pending to locate against")
        .join()
        .expect("the locate worker did not panic");
    std::fs::remove_file(&project_path).unwrap();
    controller.settle_locate();
    check(
        "relocated project that cannot reopen",
        &controller,
        "This project cannot be opened",
        Close,
    );
    assert_eq!(
        controller.last_refusal(),
        Some("demo.ffp could not be reopened after its frames were located."),
        "relocated project that cannot reopen: the body names the project file"
    );

    let mut controller = common::controller_with_frames("refusal_titles_algorithm", 2);
    controller.set_fuse_algorithm("no-such-method");
    assert!(!controller.start_fuse());
    check(
        "unresolved algorithm",
        &controller,
        "This run cannot start",
        Close,
    );

    let mut controller = common::controller_with_frames("refusal_titles_memory", 2);
    controller.set_host_memory_cap("1");
    controller.set_published_preview_bytes(1);
    assert!(!controller.start_fuse());
    check(
        "source-preview memory",
        &controller,
        "This run cannot start",
        Close,
    );
}

/// Starts the active tab's run, slowed so it can be paused part-way, and
/// pauses it: a live run for a later tab to queue behind.
fn hold_run_paused(controller: &mut AppController) {
    controller.set_slow_run(1, Duration::from_secs(2));
    assert!(controller.start_fuse());
    let start = Instant::now();
    while controller.job_view().fraction <= 0.0 {
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "the run never ticked progress"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    controller.pause_or_resume();
}

#[test]
fn missing_frame_refusal_offers_locate() {
    let (dir, project_path) = sealed("missing_frame_locate", 3);
    let copies = copy_frames(&dir, "missing_frame_locate_copies");
    let mut controller = AppController::new();
    common::open_and_settle(&mut controller, &[project_path]);
    let id = controller.tab_id_at(0).unwrap();
    std::fs::remove_file(dir.join("frame-01.tif")).unwrap();

    assert!(!controller.start_fuse());
    assert_eq!(
        refusal(&controller),
        (
            "A frame is no longer there".to_string(),
            RefusalAction::LocateTab(id)
        )
    );
    let body = controller.last_refusal().unwrap();
    assert!(body.contains("frame-01.tif"), "the file is named: {body}");
    assert!(controller.images_panel().rows.iter().any(|r| r.unresolved));

    // A folder without the project's frames relocates nothing.
    let wrong = common::tree("missing_frame_locate_wrong");
    for name in ["frame-00.tif", "frame-01.tif", "frame-02.tif"] {
        std::fs::write(wrong.join(name), b"not the right bytes at all").unwrap();
    }
    common::locate_and_settle(&mut controller, LocateTarget::Tab(id), wrong);
    assert_eq!(
        controller.refusal_title(),
        Some("These are not the project's frames")
    );
    assert!(controller.images_panel().rows.iter().any(|r| r.unresolved));

    // A folder holding them relocates this tab and opens no other.
    common::locate_and_settle(&mut controller, LocateTarget::Tab(id), copies);
    assert_eq!(controller.project_tabs().len(), 1, "no new tab opens");
    assert!(controller.images_panel().rows.iter().all(|r| !r.unresolved));
    assert_eq!(controller.gallery_failure_kind(0), None);

    // Never saved: nothing to check a folder against, so only Close.
    let mut unsaved = common::controller_with_frames("missing_frame_unsaved", 2);
    let frames = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("missing_frame_unsaved_frames");
    std::fs::remove_file(frames.join("00.tif")).unwrap();
    assert!(!unsaved.start_fuse());
    assert_eq!(
        refusal(&unsaved),
        (
            "A frame is no longer there".to_string(),
            RefusalAction::Close
        )
    );
}

#[test]
fn queued_missing_frame_refusal_locates_the_queued_tab() {
    let mut controller = common::controller_with_frames("queued_missing_frame", 2);
    let running = controller.tab_id_at(0).unwrap();
    hold_run_paused(&mut controller);

    let (dir, project_path) = sealed("queued_missing_frame_saved", 2);
    common::open_and_settle(&mut controller, &[project_path]);
    let queued = controller.tab_id_at(1).unwrap();
    assert_ne!(queued, running);
    assert!(
        controller.start_fuse(),
        "the saved tab queues behind the run"
    );
    std::fs::remove_file(dir.join("frame-00.tif")).unwrap();

    controller.select_project(0);
    controller.pause_or_resume();
    common::wait_for_settle(&mut controller, Duration::from_secs(120));

    assert_eq!(
        refusal(&controller),
        (
            "A frame is no longer there".to_string(),
            RefusalAction::LocateTab(queued)
        ),
        "the locate targets the queued tab, not the active one"
    );
}

#[test]
fn both_source_dialogs_name_the_project_file_alike() {
    let fixtures = common::workspace_root().join("fixtures/projects");
    // The fixtures' frames sit beside the projects, as they do in place.
    let dir = common::tree("source_dialogs_alike");
    std::fs::create_dir_all(dir.join("frames")).unwrap();
    for frame in std::fs::read_dir(fixtures.join("frames")).unwrap() {
        let frame = frame.unwrap().path();
        std::fs::copy(&frame, dir.join("frames").join(frame.file_name().unwrap())).unwrap();
    }
    for (fixture, copy) in [
        ("missing.ffp", "missing.ffp"),
        ("missing.ffp", "other name.ffp"),
        ("mismatch.ffp", "mismatch.ffp"),
        ("mismatch.ffp", "other name.ffp"),
    ] {
        let path = dir.join(copy);
        std::fs::copy(fixtures.join(fixture), &path).unwrap();
        let mut controller = AppController::new();
        let outcome = common::open_and_settle(&mut controller, &[path]);
        let file_name = if fixture == "missing.ffp" {
            assert!(matches!(outcome, OpenOutcome::Opened(_)), "{copy}");
            assert_eq!(
                controller.project_tabs()[0].title,
                copy.trim_end_matches(".ffp")
            );
            controller.sources_notice().expect("sources are missing").2
        } else {
            assert!(matches!(outcome, OpenOutcome::Mismatch(_)), "{copy}");
            controller.mismatch_copy().expect("a mismatch is pending").2
        };
        assert_eq!(file_name, copy, "{fixture} opened as {copy}");
    }
}
