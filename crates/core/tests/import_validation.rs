// SPDX-License-Identifier: GPL-3.0-or-later

//! A damaged project fails loudly: the three severities `Project::open` and
//! `Project::open_checked` produce -- refuse, block on a choice, warn -- and
//! the specific condition each covers. Every fixture is a good project,
//! damaged programmatically: truncated, tampered, or missing an entry a
//! record names.

mod common;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use focusforge_core::align::Registration;
use focusforge_core::io::{Rect, TiffSpec, TiffWriter};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::project::container::{ContainerWriter, Index};
use focusforge_core::project::{
    self, AppendRequest, BlobKind, Metrics, Op, Opened, Plane, Point, Project, ProjectError,
    SealRequest, blob,
};
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};

const SPP: u32 = 3;

fn tmp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn full(width: u32, height: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

fn composite_samples(w: u32, h: u32) -> Vec<u16> {
    (0..(w as usize * h as usize * SPP as usize))
        .map(|i| (i % 65_536) as u16)
        .collect()
}

fn zmap_noisy(w: u32, h: u32, frames: u32) -> Vec<u16> {
    (0..w * h)
        .map(|i| {
            let x = i % w;
            let y = i / w;
            let h1 = x
                .wrapping_mul(2_654_435_761)
                .wrapping_add(y.wrapping_mul(40_503));
            (h1 % frames) as u16
        })
        .collect()
}

fn stage_planes(dir: &Path, w: u32, h: u32, frames: u32) -> (PathBuf, PathBuf) {
    let composite = dir.join("composite.tif");
    let zmap = dir.join("zmap.tif");
    let mut writer = TiffWriter::create(
        &composite,
        TiffSpec {
            width: w,
            height: h,
            samples_per_pixel: SPP,
            description: Some("staged composite".to_string()),
        },
    )
    .unwrap();
    writer
        .write_tile(full(w, h), &composite_samples(w, h))
        .unwrap();
    writer.finish().unwrap();

    let mut map = ZMapWriter::create(&zmap, ZMapInfo::new(w, h, frames).unwrap()).unwrap();
    map.write_tile(full(w, h), &zmap_noisy(w, h, frames))
        .unwrap();
    map.finish().unwrap();
    (composite, zmap)
}

/// `count` distinct source frames written directly under `dir` -- the same
/// directory the project is later sealed into, so a clean project records
/// them `Relative`.
fn write_sources(dir: &Path, count: usize, w: u32, h: u32) -> Vec<PathBuf> {
    (0..count)
        .map(|i| {
            let path = dir.join(format!("frame-{i:02}.tif"));
            let mut writer = TiffWriter::create(
                &path,
                TiffSpec {
                    width: w,
                    height: h,
                    samples_per_pixel: SPP,
                    description: None,
                },
            )
            .unwrap();
            let samples: Vec<u16> = (0..(w as usize * h as usize * SPP as usize))
                .map(|j| ((j + i * 17) % 65_536) as u16)
                .collect();
            writer.write_tile(full(w, h), &samples).unwrap();
            writer.finish().unwrap();
            path
        })
        .collect()
}

fn params() -> ParamValues {
    let mut values = ParamValues::new();
    values.insert("levels".to_string(), ParamValue::Int(5));
    values
}

fn seal_request<'a>(
    project_path: &'a Path,
    composite: &'a Path,
    zmap: &'a Path,
    sources: &'a [PathBuf],
    values: &'a ParamValues,
) -> SealRequest<'a> {
    SealRequest {
        project_path,
        staged_composite: composite,
        staged_zmap: zmap,
        sources,
        algorithm: "laplacian",
        params: values,
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    }
}

/// One sealed project in a fresh directory, its `count` frames written
/// directly under it -- a clean project with `Relative` sources, so moving
/// or editing them on disk is exactly what a user could do to a copy.
fn sealed(name: &str, w: u32, h: u32, frames: u32) -> (PathBuf, PathBuf) {
    let dir = tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging, w, h, frames);
    let sources = write_sources(&dir, frames as usize, w, h);
    let values = params();
    let project_path = dir.join(format!("demo.{}", project::PROJECT_EXTENSION));
    project::seal(seal_request(
        &project_path,
        &composite,
        &zmap,
        &sources,
        &values,
    ))
    .expect("seal must succeed");
    (dir, project_path)
}

/// One sealed project over `fixtures/mantis-512`'s own golden planes, with
/// its own 8 source frames referenced.
fn sealed_mantis(name: &str) -> (PathBuf, PathBuf) {
    let dir = tmp_dir(name);
    let mantis = common::mantis_512_dir();
    // Copied rather than staged straight from the fixture: `seal` stages its
    // blobs beside `staged_composite`, and concurrent tests must not share
    // that directory.
    let composite = dir.join("golden-composite.tif");
    std::fs::copy(mantis.join("golden").join("laplacian.tif"), &composite).unwrap();
    let zmap = dir.join("golden-zmap.tif");
    std::fs::copy(mantis.join("golden").join("laplacian-zmap.tif"), &zmap).unwrap();
    let sources: Vec<PathBuf> = (0..8)
        .map(|i| mantis.join("frames").join(format!("{i:02}.tif")))
        .collect();
    let values = params();
    let project_path = dir.join(format!("demo.{}", project::PROJECT_EXTENSION));
    project::seal(seal_request(
        &project_path,
        &composite,
        &zmap,
        &sources,
        &values,
    ))
    .expect("seal must succeed");
    (dir, project_path)
}

fn stroke(source_id: u16, x: i32, y: i32, radius: f32) -> Op {
    Op::BrushStroke {
        source_id,
        path: vec![Point { x, y }],
        radius,
        softness: 0.0,
    }
}

/// `result.unwrap_err()`, without requiring the `Ok` side to implement
/// `Debug` -- `Project` deliberately does not.
fn err<T>(result: Result<T, ProjectError>) -> ProjectError {
    match result {
        Err(e) => e,
        Ok(_) => panic!("expected an error"),
    }
}

fn read_entry_bytes(path: &Path, name: &str) -> Vec<u8> {
    Index::read(path).unwrap().read_entry(path, name).unwrap()
}

fn read_manifest_value(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&read_entry_bytes(path, "manifest.json")).unwrap()
}

fn read_checkpoint_json(path: &Path, id: &str) -> serde_json::Value {
    serde_json::from_slice(&read_entry_bytes(path, &format!("checkpoints/{id}.json"))).unwrap()
}

/// Copies `src` to `dst`, replacing the named entries in `replace` and
/// appending the entries in `add` -- a raw rewrite of a real container, so a
/// test can inject a malformed link or drop an entry the production writer
/// would never produce. `drop` names entries left out of the copy entirely.
fn mutate_container(
    src: &Path,
    dst: &Path,
    replace: &[(String, Vec<u8>)],
    add: &[(String, Vec<u8>)],
    drop: &[&str],
) {
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
    for (name, bytes) in add {
        writer.put_bytes(name, bytes).unwrap();
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
fn truncated_container_is_refused() {
    let (dir, project_path) = sealed("import_validation_truncated", 32, 24, 3);
    let bytes = std::fs::read(&project_path).unwrap();

    // Inside the first entry (manifest.json is written first).
    let inside_first_entry = dir.join("truncated-first-entry.ffp");
    std::fs::write(&inside_first_entry, &bytes[..60.min(bytes.len())]).unwrap();
    assert!(matches!(
        err(Project::open(&inside_first_entry)),
        ProjectError::Truncated { .. }
    ));

    // Inside the last blob, near the end of the file.
    let inside_last_blob = dir.join("truncated-last-blob.ffp");
    std::fs::write(&inside_last_blob, &bytes[..bytes.len() - 10]).unwrap();
    assert!(matches!(
        err(Project::open(&inside_last_blob)),
        ProjectError::Truncated { .. }
    ));
}

#[test]
fn tampered_blob_is_refused() {
    let (dir, project_path) = sealed("import_validation_tampered", 64, 48, 3);
    let project = Project::open(&project_path).unwrap();
    let head_zmap_hash = project.checkpoint().zmap_blob.clone();
    let composite_hash = project.manifest().composite.blob.clone();
    drop(project);

    // The head's own Z-Map snapshot: refused at open.
    let tampered_head = dir.join("tampered-head.ffp");
    let head_entry = format!("blobs/{head_zmap_hash}.zst");
    let original_head_bytes = read_entry_bytes(&project_path, &head_entry);
    mutate_container(
        &project_path,
        &tampered_head,
        &[(head_entry, corrupt_blob_bytes(&original_head_bytes))],
        &[],
        &[],
    );
    match err(Project::open(&tampered_head)) {
        ProjectError::HashMismatch { blob, .. } => assert_eq!(blob, head_zmap_hash),
        other => panic!("expected HashMismatch, got {other:?}"),
    }

    // The composite: not touched by open, so it opens cleanly, and is
    // refused only when actually read.
    let tampered_composite = dir.join("tampered-composite.ffp");
    let composite_entry = format!("blobs/{composite_hash}.zst");
    let original_composite_bytes = read_entry_bytes(&project_path, &composite_entry);
    mutate_container(
        &project_path,
        &tampered_composite,
        &[(
            composite_entry,
            corrupt_blob_bytes(&original_composite_bytes),
        )],
        &[],
        &[],
    );
    let mut project = Project::open(&tampered_composite)
        .expect("a tampered composite must not block open -- it is never touched");
    let out = dir.join("composite-out.tif");
    match project::materialize_plane(&mut project, Plane::Composite, &out).unwrap_err() {
        ProjectError::HashMismatch { blob, .. } => assert_eq!(blob, composite_hash),
        other => panic!("expected HashMismatch, got {other:?}"),
    }
    assert!(!out.exists());
}

#[test]
fn verification_is_lazy_but_never_skipped() {
    let (dir, project_path) = sealed_mantis("import_validation_lazy");
    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();
    let root_zmap_hash = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .zmap_blob
        .clone();

    // A re-fuse in the middle, so the head's op checkpoint names its map
    // and the root's stays off the head's own path.
    let staging = dir.join("mid-staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (mid_composite, mid_zmap) = stage_planes(&staging, 512, 512, 8);
    let sources: Vec<PathBuf> = (0..8)
        .map(|i| {
            common::mantis_512_dir()
                .join("frames")
                .join(format!("{i:02}.tif"))
        })
        .collect();
    let values = params();
    let mid = project::append(AppendRequest {
        project_path: &project_path,
        staged_composite: &mid_composite,
        staged_zmap: &mid_zmap,
        sources: &sources,
        algorithm: "laplacian",
        params: &values,
        align: None,
        registration: Registration::default(),
        composite_description: "re-fused composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("middle re-fuse must succeed");

    let ops = vec![Op::BrushStroke {
        source_id: 3,
        path: vec![
            Point { x: 200, y: 220 },
            Point { x: 230, y: 205 },
            Point { x: 260, y: 225 },
        ],
        radius: 12.0,
        softness: 0.3,
    }];
    let sealed_ops =
        project::append_ops_at(&project_path, ops, None, 1_000).expect("append must succeed");
    assert_eq!(sealed_ops.zmap.kind, BlobKind::Full);

    // The root's own Z-Map, behind the middle checkpoint and off the head's
    // own blob: tamper it.
    let tampered = dir.join("tampered-root.ffp");
    let root_entry = format!("blobs/{root_zmap_hash}.zst");
    let original_root_bytes = read_entry_bytes(&project_path, &root_entry);
    mutate_container(
        &project_path,
        &tampered,
        &[(root_entry, corrupt_blob_bytes(&original_root_bytes))],
        &[],
        &[],
    );

    let mut project = Project::open(&tampered)
        .expect("a tampered blob off the head's own path must not block open");

    let verified: HashSet<String> = project
        .verified_blobs()
        .iter()
        .map(|(hash, _)| hash.clone())
        .collect();
    assert!(
        !verified.contains(&root_zmap_hash),
        "the untouched root Z-Map must not have been hashed"
    );
    assert_eq!(sealed_ops.zmap.hash, mid.zmap.hash);
    assert_eq!(
        verified,
        HashSet::from([mid.zmap.hash.clone()]),
        "verified_blobs must be exactly the Z-Map the head names"
    );

    let root_checkpoint = project.history().get(&root_id).unwrap().clone();
    let out = dir.join("root-materialized.tif");
    match project::materialize_zmap(&mut project, &root_checkpoint, &out).unwrap_err() {
        ProjectError::HashMismatch { blob, .. } => assert_eq!(blob, root_zmap_hash),
        other => panic!("expected HashMismatch, got {other:?}"),
    }
    assert!(!out.exists(), "no output file may be left");
}

#[test]
fn open_does_not_hash_the_whole_store() {
    let (_dir, project_path) = sealed("import_validation_open_cost", 256, 256, 251);
    let one_checkpoint_bytes: u64 = Project::open(&project_path)
        .unwrap()
        .verified_blobs()
        .iter()
        .map(|(_, n)| n)
        .sum();

    for i in 0..50i32 {
        let x = 5 + (i * 2) % 240;
        let y = 5 + (i * 3) % 240;
        let source_id = (i % 251) as u16;
        project::append_ops(&project_path, vec![stroke(source_id, x, y, 1.0)], None)
            .unwrap_or_else(|err| panic!("append #{i} must succeed: {err}"));
    }

    let project = Project::open(&project_path).unwrap();
    let head = project.checkpoint().clone();
    let expected_hashes = HashSet::from([head.zmap_blob.clone()]);
    let verified_hashes: HashSet<String> = project
        .verified_blobs()
        .iter()
        .map(|(hash, _)| hash.clone())
        .collect();
    assert_eq!(verified_hashes, expected_hashes);

    let expected_bytes: u64 = expected_hashes
        .iter()
        .map(|hash| {
            project
                .manifest()
                .blobs
                .iter()
                .find(|b| &b.hash == hash)
                .unwrap()
                .uncompressed_len
        })
        .sum();
    let hashed_bytes: u64 = project.verified_blobs().iter().map(|(_, n)| n).sum();
    assert_eq!(hashed_bytes, expected_bytes);

    assert!(
        hashed_bytes < one_checkpoint_bytes * 2,
        "open cost grew with checkpoint count: {hashed_bytes} B over 50 checkpoints, \
         {one_checkpoint_bytes} B over one"
    );
}

#[test]
fn foreign_file_is_refused() {
    let dir = tmp_dir("import_validation_foreign");

    let tiff_path = dir.join("frame.tif");
    let mut writer = TiffWriter::create(
        &tiff_path,
        TiffSpec {
            width: 4,
            height: 4,
            samples_per_pixel: 1,
            description: None,
        },
    )
    .unwrap();
    writer.write_tile(full(4, 4), &[0u16; 16]).unwrap();
    writer.finish().unwrap();
    assert!(matches!(
        err(Project::open(&tiff_path)),
        ProjectError::NotAProject { .. }
    ));

    let text_path = dir.join("notes.txt");
    std::fs::write(&text_path, b"hello, this is not a project at all").unwrap();
    assert!(matches!(
        err(Project::open(&text_path)),
        ProjectError::NotAProject { .. }
    ));

    let short_path = dir.join("short.bin");
    std::fs::write(&short_path, b"ab").unwrap();
    assert!(matches!(
        err(Project::open(&short_path)),
        ProjectError::NotAProject { .. }
    ));

    let (_d, project_path) = sealed("import_validation_foreign_zip", 32, 24, 3);
    let no_manifest = dir.join("no-manifest.ffp");
    mutate_container(&project_path, &no_manifest, &[], &[], &["manifest.json"]);
    assert!(matches!(
        err(Project::open(&no_manifest)),
        ProjectError::NotAProject { .. }
    ));
}

#[test]
fn missing_entry_is_refused() {
    let (dir, project_path) = sealed("import_validation_missing_entry", 32, 24, 3);

    // A blob a record names, absent from the zip.
    let head_hash = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .zmap_blob
        .clone();
    let missing_blob = dir.join("missing-blob.ffp");
    mutate_container(
        &project_path,
        &missing_blob,
        &[],
        &[],
        &[&format!("blobs/{head_hash}.zst")],
    );
    match err(Project::open(&missing_blob)) {
        ProjectError::MissingEntry { entry, .. } => {
            assert_eq!(entry, format!("blobs/{head_hash}.zst"))
        }
        other => panic!("expected MissingEntry, got {other:?}"),
    }

    // A checkpoint's zmap_blob with no record at all.
    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();
    let mut orphan_blob = read_checkpoint_json(&project_path, &root_id);
    orphan_blob["zmap_blob"] =
        serde_json::json!("0000000000000000000000000000000000000000000000000000000000000000");
    let no_record = dir.join("no-record.ffp");
    mutate_container(
        &project_path,
        &no_record,
        &[(
            format!("checkpoints/{root_id}.json"),
            serde_json::to_vec_pretty(&orphan_blob).unwrap(),
        )],
        &[],
        &[],
    );
    assert!(matches!(
        err(Project::open(&no_record)),
        ProjectError::MissingEntry { .. }
    ));

    // The manifest's own head checkpoint file, missing.
    let mut manifest = read_manifest_value(&project_path);
    manifest["checkpoint"] = serde_json::json!("nonexistent-checkpoint");
    let missing_head = dir.join("missing-head.ffp");
    mutate_container(
        &project_path,
        &missing_head,
        &[(
            "manifest.json".to_string(),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )],
        &[],
        &[],
    );
    match err(Project::open(&missing_head)) {
        ProjectError::MissingEntry { entry, .. } => {
            assert_eq!(entry, "checkpoints/nonexistent-checkpoint.json")
        }
        other => panic!("expected MissingEntry, got {other:?}"),
    }
}

#[test]
fn fingerprint_mismatch_blocks_until_a_choice() {
    let (dir, project_path) = sealed("import_validation_mismatch", 32, 24, 3);
    let manifest = Project::open(&project_path).unwrap().manifest().clone();
    let changed_path = dir.join(&manifest.sources[1].path);
    let mut bytes = std::fs::read(&changed_path).unwrap();
    bytes[0] ^= 0xFF;
    std::fs::write(&changed_path, &bytes).unwrap();

    match Project::open_checked(&project_path).unwrap() {
        Opened::NeedsChoice(pending) => {
            assert_eq!(pending.changed().len(), 1);
            assert_eq!(pending.changed()[0].0, 1);
            assert_eq!(pending.changed()[0].1, changed_path);
        }
        Opened::Ready(_) => panic!("a same-named replacement must not open Ready"),
    }
}

#[test]
fn relocation_does_not_change_the_fingerprint() {
    let (dir, project_path) = sealed("import_validation_relocate_src", 32, 24, 3);
    let moved_dir = dir.parent().unwrap().join("import_validation_relocate_dst");
    let _ = std::fs::remove_dir_all(&moved_dir);
    std::fs::rename(&dir, &moved_dir).unwrap();
    let moved_project = moved_dir.join(project_path.file_name().unwrap());

    match Project::open_checked(&moved_project).unwrap() {
        Opened::Ready(project) => {
            let validation = project.validation();
            assert!(validation.accepted_mismatch.is_none());
            assert_eq!(validation.unresolved_sources, 0);
        }
        Opened::NeedsChoice(_) => panic!("relocating a project with its frames must not warn"),
    }
}

#[test]
fn frame_set_changes_change_the_fingerprint() {
    let (dir, project_path) = sealed("import_validation_frameset", 32, 24, 3);
    let manifest = Project::open(&project_path).unwrap().manifest().clone();

    let path0 = dir.join(&manifest.sources[0].path);
    let path1 = dir.join(&manifest.sources[1].path);
    let bytes0 = std::fs::read(&path0).unwrap();
    let bytes1 = std::fs::read(&path1).unwrap();
    std::fs::write(&path0, &bytes1).unwrap();
    std::fs::write(&path1, &bytes0).unwrap();

    match Project::open_checked(&project_path).unwrap() {
        Opened::NeedsChoice(pending) => {
            let changed_indices: HashSet<usize> =
                pending.changed().iter().map(|(index, _)| *index).collect();
            assert_eq!(changed_indices, [0usize, 1].into_iter().collect());
        }
        Opened::Ready(_) => panic!("swapping two frames' content must not open Ready"),
    }

    let excluded = blob::stack_fingerprint(&manifest.sources[1..]);
    assert_ne!(
        excluded, manifest.stack_fingerprint,
        "excluding a frame from the fingerprint must change the value"
    );
}

#[test]
fn proceeding_carries_the_mismatch_forward() {
    let (dir, project_path) = sealed("import_validation_proceed", 32, 24, 3);
    let manifest = Project::open(&project_path).unwrap().manifest().clone();
    let changed_path = dir.join(&manifest.sources[0].path);
    let mut bytes = std::fs::read(&changed_path).unwrap();
    bytes[0] ^= 0xFF;
    std::fs::write(&changed_path, &bytes).unwrap();

    let pending = match Project::open_checked(&project_path).unwrap() {
        Opened::NeedsChoice(pending) => pending,
        Opened::Ready(_) => panic!("expected NeedsChoice"),
    };
    let project = pending.proceed();
    let validation = project.validation();
    let mismatch = validation
        .accepted_mismatch
        .expect("proceed must carry the mismatch forward");
    assert_eq!(mismatch.changed.len(), 1);
    assert_eq!(mismatch.changed[0].0, 0);
    assert_eq!(mismatch.recorded_fingerprint, manifest.stack_fingerprint);
}

#[test]
fn unresolvable_sources_warn_and_still_open() {
    let (_dir, project_path) = sealed("import_validation_unresolvable", 32, 24, 3);
    let copy_dir = tmp_dir("import_validation_unresolvable_copy");
    let copied_project = copy_dir.join(project_path.file_name().unwrap());
    std::fs::copy(&project_path, &copied_project).unwrap();
    // The frames are deliberately not copied alongside.

    match Project::open_checked(&copied_project).unwrap() {
        Opened::Ready(mut project) => {
            assert!(!project.history().all().is_empty());
            let out = copy_dir.join("head.tif");
            let head = project.checkpoint().clone();
            project::materialize_zmap(&mut project, &head, &out)
                .expect("the head's own Z-Map needs no source frame");
            let validation = project.validation();
            assert_eq!(validation.unresolved_sources, 3);
        }
        Opened::NeedsChoice(_) => panic!("unresolvable sources must warn, not block"),
    }
}

#[test]
fn link_failures_are_structural_refusals() {
    let (dir, project_path) = sealed("import_validation_links", 32, 24, 3);
    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();

    // A dangling parent.
    let mut orphan = read_checkpoint_json(&project_path, &root_id);
    orphan["id"] = serde_json::json!("orphan");
    orphan["parent"] = serde_json::json!("ghost");
    let dangling = dir.join("dangling.ffp");
    mutate_container(
        &project_path,
        &dangling,
        &[],
        &[(
            "checkpoints/orphan.json".to_string(),
            serde_json::to_vec_pretty(&orphan).unwrap(),
        )],
        &[],
    );
    assert!(matches!(
        err(Project::open(&dangling)),
        ProjectError::DanglingParent { .. }
    ));
    assert!(matches!(
        err(Project::open_checked(&dangling)),
        ProjectError::DanglingParent { .. }
    ));

    // A parent cycle: a self-parent.
    let mut self_cycle = read_checkpoint_json(&project_path, &root_id);
    self_cycle["parent"] = serde_json::json!(root_id);
    let cycle = dir.join("cycle.ffp");
    mutate_container(
        &project_path,
        &cycle,
        &[(
            format!("checkpoints/{root_id}.json"),
            serde_json::to_vec_pretty(&self_cycle).unwrap(),
        )],
        &[],
        &[],
    );
    assert!(matches!(
        err(Project::open(&cycle)),
        ProjectError::ParentCycle { .. }
    ));
    assert!(matches!(
        err(Project::open_checked(&cycle)),
        ProjectError::ParentCycle { .. }
    ));

    // A checkpoint whose own stack_fingerprint disagrees with its parent's.
    let child = project::append_ops_at(
        &project_path,
        vec![stroke(1, 5, 5, 1.0)],
        None,
        1_700_000_000,
    )
    .unwrap();
    let mut tampered_child = read_checkpoint_json(&project_path, &child.checkpoint);
    tampered_child["stack_fingerprint"] =
        serde_json::json!("0000000000000000000000000000000000000000000000000000000000000000");
    let fp_disagree = dir.join("fp-disagree.ffp");
    mutate_container(
        &project_path,
        &fp_disagree,
        &[(
            format!("checkpoints/{}.json", child.checkpoint),
            serde_json::to_vec_pretty(&tampered_child).unwrap(),
        )],
        &[],
        &[],
    );
    assert!(matches!(
        err(Project::open(&fp_disagree)),
        ProjectError::FingerprintDisagrees { .. }
    ));
    assert!(matches!(
        err(Project::open_checked(&fp_disagree)),
        ProjectError::FingerprintDisagrees { .. }
    ));

    // A fused checkpoint whose own stack_fingerprint disagrees with its own
    // fuse record's sources.
    let mut tampered_root = read_checkpoint_json(&project_path, &root_id);
    tampered_root["stack_fingerprint"] =
        serde_json::json!("0000000000000000000000000000000000000000000000000000000000000000");
    let fused_fp_disagree = dir.join("fused-fp-disagree.ffp");
    mutate_container(
        &project_path,
        &fused_fp_disagree,
        &[(
            format!("checkpoints/{root_id}.json"),
            serde_json::to_vec_pretty(&tampered_root).unwrap(),
        )],
        &[],
        &[],
    );
    assert!(matches!(
        err(Project::open(&fused_fp_disagree)),
        ProjectError::FingerprintDisagrees { .. }
    ));
    assert!(matches!(
        err(Project::open_checked(&fused_fp_disagree)),
        ProjectError::FingerprintDisagrees { .. }
    ));

    // The manifest's own stack_fingerprint disagreeing with the head's and
    // with stack_fingerprint(&manifest.sources).
    let mut tampered_manifest = read_manifest_value(&project_path);
    tampered_manifest["stack_fingerprint"] =
        serde_json::json!("0000000000000000000000000000000000000000000000000000000000000000");
    let manifest_fp_disagree = dir.join("manifest-fp-disagree.ffp");
    mutate_container(
        &project_path,
        &manifest_fp_disagree,
        &[(
            "manifest.json".to_string(),
            serde_json::to_vec_pretty(&tampered_manifest).unwrap(),
        )],
        &[],
        &[],
    );
    assert!(matches!(
        err(Project::open(&manifest_fp_disagree)),
        ProjectError::FingerprintDisagrees { .. }
    ));
    assert!(matches!(
        err(Project::open_checked(&manifest_fp_disagree)),
        ProjectError::FingerprintDisagrees { .. }
    ));

    // An op-bearing checkpoint with no parent at all -- nothing for its
    // stack_fingerprint to disagree with.
    let mut ops_no_parent_json = read_checkpoint_json(&project_path, &child.checkpoint);
    ops_no_parent_json["parent"] = serde_json::Value::Null;
    let ops_no_parent = dir.join("ops-no-parent.ffp");
    mutate_container(
        &project_path,
        &ops_no_parent,
        &[(
            format!("checkpoints/{}.json", child.checkpoint),
            serde_json::to_vec_pretty(&ops_no_parent_json).unwrap(),
        )],
        &[],
        &[],
    );
    assert!(matches!(
        err(Project::open(&ops_no_parent)),
        ProjectError::OpsWithoutParent { .. }
    ));
    assert!(matches!(
        err(Project::open_checked(&ops_no_parent)),
        ProjectError::OpsWithoutParent { .. }
    ));

    // A legitimate re-fused chain, whose checkpoints carry different
    // fingerprints because the frame set they were fused from changed,
    // opens cleanly.
    let (dir2, refuse_project) = sealed("import_validation_refuse", 32, 24, 3);
    let staging2 = dir2.join("staging2");
    std::fs::create_dir_all(&staging2).unwrap();
    let (composite2, zmap2) = stage_planes(&staging2, 32, 24, 4);
    let new_sources = write_sources(&dir2, 4, 32, 24);
    let values = params();
    project::append(AppendRequest {
        project_path: &refuse_project,
        staged_composite: &composite2,
        staged_zmap: &zmap2,
        sources: &new_sources,
        algorithm: "laplacian",
        params: &values,
        align: None,
        registration: Registration::default(),
        composite_description: "re-fused composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("re-fuse append must succeed");

    let refused_project =
        Project::open(&refuse_project).expect("a legitimate re-fuse must open cleanly");
    let chain = refused_project.checkpoint_chain().unwrap();
    assert_ne!(
        chain[0].stack_fingerprint, chain[1].stack_fingerprint,
        "the re-fuse changed the frame set, so its fingerprint legitimately differs"
    );
}

#[test]
fn no_failure_returns_a_partial_project() {
    let (dir, project_path) = sealed("import_validation_no_partial", 32, 24, 3);

    let truncated = dir.join("truncated.ffp");
    let bytes = std::fs::read(&project_path).unwrap();
    std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();
    assert!(Project::open(&truncated).is_err());
    assert!(Project::open_checked(&truncated).is_err());

    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();
    let mut orphan = read_checkpoint_json(&project_path, &root_id);
    orphan["id"] = serde_json::json!("orphan2");
    orphan["parent"] = serde_json::json!("ghost2");
    let dangling = dir.join("dangling2.ffp");
    mutate_container(
        &project_path,
        &dangling,
        &[],
        &[(
            "checkpoints/orphan2.json".to_string(),
            serde_json::to_vec_pretty(&orphan).unwrap(),
        )],
        &[],
    );
    assert!(Project::open(&dangling).is_err());
    assert!(Project::open_checked(&dangling).is_err());

    // A blocking choice exposes only the mismatch -- `changed()`,
    // `recorded_fingerprint()`, `recomputed_fingerprint()` -- until
    // `proceed` hands back a usable project.
    let manifest = Project::open(&project_path).unwrap().manifest().clone();
    let changed_path = dir.join(&manifest.sources[0].path);
    let mut bytes = std::fs::read(&changed_path).unwrap();
    bytes[0] ^= 0xFF;
    std::fs::write(&changed_path, &bytes).unwrap();
    let pending = match Project::open_checked(&project_path).unwrap() {
        Opened::NeedsChoice(pending) => pending,
        Opened::Ready(_) => panic!("expected NeedsChoice"),
    };
    assert_eq!(pending.changed().len(), 1);
    let project = pending.proceed();
    assert!(
        !project.history().all().is_empty(),
        "only after proceed does history become reachable"
    );
}

#[test]
fn a_freshly_written_project_is_clean() {
    let (_dir, project_path) = sealed("import_validation_clean", 32, 24, 3);
    match Project::open_checked(&project_path).unwrap() {
        Opened::Ready(project) => {
            let validation = project.validation();
            assert_eq!(validation.unresolved_sources, 0);
            assert!(validation.accepted_mismatch.is_none());
        }
        Opened::NeedsChoice(_) => panic!("a freshly written project must not need a choice"),
    }
}

#[test]
fn located_frames_are_checked_by_content() {
    let (dir, project_path) = sealed("import_validation_locate", 32, 24, 3);
    let manifest = Project::open(&project_path).unwrap().manifest().clone();

    let wrong_dir = tmp_dir("import_validation_locate_wrong");
    for source in &manifest.sources {
        let name = Path::new(&source.path).file_name().unwrap();
        std::fs::write(wrong_dir.join(name), b"not the right bytes at all").unwrap();
    }
    let wrong = project::check_sources(&manifest.sources, |s| {
        wrong_dir.join(Path::new(&s.path).file_name().unwrap())
    })
    .unwrap();
    assert_eq!(wrong.unresolved.len(), 0);
    assert_eq!(wrong.changed.len(), manifest.sources.len());

    let right = project::check_sources(&manifest.sources, |s| {
        dir.join(Path::new(&s.path).file_name().unwrap())
    })
    .unwrap();
    assert_eq!(right.unresolved.len(), 0);
    assert_eq!(right.changed.len(), 0);
}

/// Appends a one-point stroke under the head: an op-bearing head is the only
/// state whose render reads source frames.
fn append_stroke(project_path: &Path) {
    let head = Project::open(project_path).unwrap().checkpoint().id.clone();
    let stroke = project::Op::BrushStroke {
        source_id: 1,
        path: vec![project::Point { x: 4, y: 4 }],
        radius: 2.0,
        softness: 0.0,
    };
    project::append_ops_onto(project_path, &head, vec![stroke], None, 1)
        .expect("the stroke must append");
}

#[test]
fn relocated_sources_are_read_from_the_located_folder() {
    let (dir, project_path) = sealed("import_validation_relocate_sources", 32, 24, 3);
    append_stroke(&project_path);
    let copy_dir = tmp_dir("import_validation_relocate_sources_copy");
    let copied_project = copy_dir.join(project_path.file_name().unwrap());
    std::fs::copy(&project_path, &copied_project).unwrap();
    // Frames are deliberately not copied alongside `copied_project`.

    let mut project = match Project::open_checked(&copied_project).unwrap() {
        Opened::Ready(project) => project,
        Opened::NeedsChoice(_) => panic!("moving the project must not change any frame's content"),
    };
    let head_id = project.checkpoint().id.clone();
    let image = project::CheckpointImage::prepare(&project, &head_id).unwrap();
    assert!(
        image
            .render(Some(&mut project), image.full(), &mut |_samples, _rect| Ok(
                ()
            ))
            .is_err(),
        "an edited render must refuse sources it cannot find"
    );

    let before = std::fs::read(&copied_project).unwrap();
    project.relocate_sources(dir.clone());
    let image = project::CheckpointImage::prepare(&project, &head_id)
        .expect("a located folder must resolve every source");
    image
        .render(Some(&mut project), image.full(), &mut |_samples, _rect| {
            Ok(())
        })
        .expect("render must read the located folder's frames");
    assert_eq!(
        std::fs::read(&copied_project).unwrap(),
        before,
        ".ffp bytes must be unchanged by relocation"
    );
}

/// A sealed project whose frames live in a sibling directory, so every
/// source is recorded `AsGiven`.
fn sealed_as_given(name: &str, w: u32, h: u32, frames: u32) -> (PathBuf, PathBuf) {
    let dir = tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging, w, h, frames);
    let frames_dir = dir.join("frames");
    std::fs::create_dir_all(&frames_dir).unwrap();
    let sources = write_sources(&frames_dir, frames as usize, w, h);
    let project_dir = dir.join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let project_path = project_dir.join(format!("demo.{}", project::PROJECT_EXTENSION));
    let values = params();
    project::seal(seal_request(
        &project_path,
        &composite,
        &zmap,
        &sources,
        &values,
    ))
    .expect("seal must succeed");
    (dir, project_path)
}

/// Where each of the project's recorded sources lives on disk.
fn source_paths(project_path: &Path) -> Vec<PathBuf> {
    let project = Project::open(project_path).unwrap();
    project
        .manifest()
        .sources
        .iter()
        .map(|source| project.source_path(source))
        .collect()
}

/// Flips the last byte of `path` through the same inode and puts its
/// modification time back, so size, mtime and file id all stay as they were.
fn alter_in_place(path: &Path) {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let metadata = file.metadata().unwrap();
    file.seek(SeekFrom::Start(metadata.len() - 1)).unwrap();
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte).unwrap();
    file.seek(SeekFrom::Start(metadata.len() - 1)).unwrap();
    file.write_all(&[byte[0] ^ 0xFF]).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
        .unwrap();
}

fn set_mtime(path: &Path, modified: std::time::SystemTime) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
}

/// Moves `path`'s modification time forward without touching its bytes.
fn bump_mtime(path: &Path) {
    let modified = std::fs::metadata(path).unwrap().modified().unwrap();
    set_mtime(path, modified + std::time::Duration::from_secs(60));
}

#[test]
fn unchanged_stamp_skips_rehash() {
    for frames in [3u32, 5] {
        for (label, (_dir, project_path)) in [
            (
                "relative",
                sealed(
                    &format!("import_validation_stamp_rel_{frames}"),
                    32,
                    24,
                    frames,
                ),
            ),
            (
                "as-given",
                sealed_as_given(
                    &format!("import_validation_stamp_abs_{frames}"),
                    32,
                    24,
                    frames,
                ),
            ),
        ] {
            let recorded = Project::open(&project_path)
                .unwrap()
                .manifest()
                .sources
                .clone();
            let paths = source_paths(&project_path);
            // Push the process memo off the recorded stamp: a touch that an
            // open re-hashes and remembers, then the mtime put back. Only
            // the recorded stamp can vouch for the frame after that.
            let original = std::fs::metadata(&paths[1]).unwrap().modified().unwrap();
            bump_mtime(&paths[1]);
            assert!(matches!(
                Project::open_checked(&project_path).unwrap(),
                Opened::Ready(_)
            ));
            set_mtime(&paths[1], original);
            alter_in_place(&paths[1]);
            assert_ne!(
                blake3::hash(&std::fs::read(&paths[1]).unwrap())
                    .to_hex()
                    .to_string(),
                recorded[1].hash,
                "{label}/{frames}: the test must really have changed the bytes"
            );
            match Project::open_checked(&project_path).unwrap() {
                Opened::Ready(_) => {}
                Opened::NeedsChoice(_) => {
                    panic!("{label}/{frames}: an unchanged stamp must not be re-hashed")
                }
            }
        }
    }
}

#[test]
fn touched_source_rehashes_once_and_stays_ready() {
    let (dir, project_path) = sealed("import_validation_touched", 32, 24, 3);
    let paths = source_paths(&project_path);
    let project_bytes = std::fs::read(&project_path).unwrap();
    let ready = |label: &str| match Project::open_checked(&project_path).unwrap() {
        Opened::Ready(_) => {}
        Opened::NeedsChoice(_) => panic!("{label}: expected Ready"),
    };

    // A bumped mtime: re-hashed, found unchanged.
    bump_mtime(&paths[0]);
    ready("touched frame");
    // The fresh stamp is now remembered, so an edit that keeps it is not seen.
    alter_in_place(&paths[0]);
    ready("remembered stamp");

    // A byte-identical copy in a new file (new inode).
    let copy = dir.join("copy.tif");
    std::fs::copy(&paths[1], &copy).unwrap();
    std::fs::rename(&copy, &paths[1]).unwrap();
    ready("identical copy");

    assert_eq!(
        std::fs::read(&project_path).unwrap(),
        project_bytes,
        "open must not write the project"
    );

    // A later append records each frame's current stamp.
    let staging = dir.join("staging-append");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging, 32, 24, 3);
    let values = params();
    project::append(AppendRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources: &paths,
        algorithm: "laplacian",
        params: &values,
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("append must succeed");
    let appended = Project::open(&project_path).unwrap();
    for (source, path) in appended.manifest().sources.iter().zip(&paths) {
        assert_eq!(source.stamp(), blob::FileStamp::of(path).unwrap());
    }
}

#[test]
fn missing_and_changed_sources_are_reported() {
    let (dir, project_path) = sealed("import_validation_missing_changed", 32, 24, 3);
    append_stroke(&project_path);
    let paths = source_paths(&project_path);

    std::fs::remove_file(&paths[0]).unwrap();
    match Project::open_checked(&project_path).unwrap() {
        Opened::Ready(project) => assert_eq!(project.validation().unresolved_sources, 1),
        Opened::NeedsChoice(_) => panic!("a missing frame warns, it does not block"),
    }

    std::fs::copy(&paths[2], &paths[1]).unwrap();
    let pending = match Project::open_checked(&project_path).unwrap() {
        Opened::NeedsChoice(pending) => pending,
        Opened::Ready(_) => panic!("a frame with other content must block"),
    };
    assert_eq!(pending.changed(), &[(1, dir.join("frame-01.tif"))]);

    let mut project = pending.proceed();
    let head = project.checkpoint().id.clone();
    let image = project::CheckpointImage::prepare(&project, &head).unwrap();
    let refusal = match image.render(Some(&mut project), image.full(), &mut |_samples, _rect| {
        Ok(())
    }) {
        Err(refusal) => refusal.to_string(),
        Ok(()) => panic!("a render over missing and changed frames must be refused"),
    };
    assert!(refusal.contains("missing: frame-00.tif"), "{refusal}");
    assert!(
        refusal.contains(&format!(
            "changed: frame-01.tif (found at {})",
            dir.join("frame-01.tif").display()
        )),
        "{refusal}"
    );
}

#[test]
fn append_reuses_source_hashes() {
    let (dir, project_path) = sealed("import_validation_append_memo", 32, 24, 3);
    let paths = source_paths(&project_path);
    let original: Vec<String> = paths
        .iter()
        .map(|path| {
            blake3::hash(&std::fs::read(path).unwrap())
                .to_hex()
                .to_string()
        })
        .collect();

    // A hash that ran again would see this byte.
    alter_in_place(&paths[1]);

    let staging = dir.join("staging-append");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging, 32, 24, 3);
    let values = params();
    project::append(AppendRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources: &paths,
        algorithm: "laplacian",
        params: &values,
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("append must succeed");

    let manifest = Project::open(&project_path).unwrap().manifest().clone();
    let mut expected = manifest.sources.clone();
    for (source, hash) in expected.iter_mut().zip(&original) {
        source.hash = hash.clone();
    }
    assert_eq!(manifest.sources, expected);
    assert_eq!(
        manifest.stack_fingerprint,
        blob::stack_fingerprint(&expected)
    );
}
