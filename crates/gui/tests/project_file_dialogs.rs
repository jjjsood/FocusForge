// SPDX-License-Identifier: GPL-3.0-or-later

//! Opening a `.ffp` renders three severities -- refusal, blocking choice,
//! warning -- at the shell, and `Locate frames…` verifies a folder
//! by content hash rather than by name.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use focusforge_core::align::Registration;
use focusforge_core::io::{Rect, TiffSpec, TiffWriter};
use focusforge_core::params::ParamValues;
use focusforge_core::project::container::{ContainerWriter, Index};
use focusforge_core::project::{self, Metrics, SealRequest};
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};
use focusforge_gui::entry::LocateTarget;
use focusforge_gui::{AppController, OpenOutcome};

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
    let mut bytes = std::fs::read(&changed_path).unwrap();
    bytes[0] ^= 0xFF;
    std::fs::write(&changed_path, &bytes).unwrap();

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
    let mut bytes = std::fs::read(&changed).unwrap();
    bytes[0] ^= 0xFF;
    std::fs::write(&changed, &bytes).unwrap();

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
    let mut bytes = std::fs::read(&changed).unwrap();
    bytes[0] ^= 0xFF;
    std::fs::write(&changed, &bytes).unwrap();
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
