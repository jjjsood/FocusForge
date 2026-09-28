// SPDX-License-Identifier: GPL-3.0-or-later

//! A damaged project fails loudly: the three severities `Project::open` and
//! `Project::open_checked` produce -- refuse, block on a choice, warn -- and
//! the specific condition each covers. Every fixture is a good project,
//! damaged programmatically: truncated, tampered, rewritten with a bumped
//! `format_version`, or missing an entry a record names.

mod common;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use focusforge_core::io::{Rect, TiffSpec, TiffWriter};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::project::{
    self, AppendRequest, BlobKind, BlobRecord, FORMAT_VERSION, Metrics, Op, Opened, Plane, Point,
    Project, ProjectError, SealRequest, ZMapStorage, blob,
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
        crop: None,
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
/// its own 8 source frames referenced -- what a delta-producing append
/// needs a realistic edit to beat the full blob on.
fn sealed_mantis(name: &str) -> (PathBuf, PathBuf) {
    let dir = tmp_dir(name);
    let mantis = common::mantis_512_dir();
    let composite = mantis.join("golden").join("laplacian.tif");
    let zmap = mantis.join("golden").join("laplacian-zmap.tif");
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
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut bytes = Vec::new();
    archive
        .by_name(name)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
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
    use std::io::{Read, Write};
    let mut archive = zip::ZipArchive::new(std::fs::File::open(src).unwrap()).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    let mut writer = zip::write::ZipWriter::new(std::fs::File::create(dst).unwrap());
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for name in &names {
        if drop.contains(&name.as_str()) {
            continue;
        }
        let mut bytes = Vec::new();
        archive
            .by_name(name)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        if let Some(entry) = replace.iter().find(|entry| &entry.0 == name) {
            bytes = entry.1.clone();
        }
        writer.start_file(name.as_str(), options).unwrap();
        writer.write_all(&bytes).unwrap();
    }
    for (name, bytes) in add {
        writer.start_file(name.as_str(), options).unwrap();
        writer.write_all(bytes).unwrap();
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

    // A middle checkpoint, forced full, so the root's own snapshot sits
    // behind it rather than under the head's delta chain.
    let mid = project::append_ops_at(
        &project_path,
        vec![stroke(1, 5, 5, 1.0)],
        None,
        1_700_000_000,
        ZMapStorage::Full,
    )
    .expect("middle append must succeed");
    assert_eq!(mid.zmap.kind, BlobKind::Full);

    // A realistic brush edit, proven elsewhere to land as a delta over this
    // golden map.
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
    let sealed_ops = project::append_ops_at(&project_path, ops, None, 1_000, ZMapStorage::Smallest)
        .expect("append must succeed");
    assert_eq!(
        sealed_ops.zmap.kind,
        BlobKind::SparseDelta,
        "test edit must land as a delta to exercise the lazy chain"
    );

    // The root's own full snapshot, behind the middle checkpoint and off
    // the head's own resolve_chain: tamper it.
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
        "the untouched root snapshot must not have been hashed"
    );
    assert_eq!(
        verified,
        [mid.zmap.hash.clone(), sealed_ops.zmap.hash.clone()]
            .into_iter()
            .collect(),
        "verified_blobs must be exactly the head's own resolve_chain"
    );

    let root_checkpoint = project.history().get(&root_id).unwrap().clone();
    let out = dir.join("root-materialized.tif");
    match project::materialize_zmap(&mut project, &root_checkpoint, &out).unwrap_err() {
        ProjectError::HashMismatch { blob, .. } => assert_eq!(blob, root_zmap_hash),
        other => panic!("expected HashMismatch, got {other:?}"),
    }
    assert!(!out.exists(), "no output file may be left");
}

/// The set of blob hashes `target` resolves through, independent of
/// `resolve_chain` (`pub(super)`, unreachable from here) -- what open must
/// have hashed and nothing more.
fn expected_chain_hashes(blobs: &[BlobRecord], target: &str) -> HashSet<String> {
    let mut chain = HashSet::new();
    let mut current = target.to_string();
    loop {
        if let Some(full) = blobs
            .iter()
            .find(|b| b.kind == BlobKind::Full && b.hash == current)
        {
            chain.insert(full.hash.clone());
            return chain;
        }
        let delta = blobs
            .iter()
            .find(|b| {
                b.kind == BlobKind::SparseDelta && b.target.as_deref() == Some(current.as_str())
            })
            .expect("a non-full target must resolve through a delta");
        chain.insert(delta.hash.clone());
        current = delta.base.clone().expect("a delta always names its base");
    }
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
    let expected_hashes = expected_chain_hashes(&project.manifest().blobs, &head.zmap_blob);
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
fn future_format_version_is_refused() {
    let (dir, project_path) = sealed("import_validation_future_version", 32, 24, 3);
    let mut manifest = read_manifest_value(&project_path);
    manifest["format_version"] = serde_json::json!(FORMAT_VERSION + 1);
    let bumped = dir.join("bumped.ffp");
    mutate_container(
        &project_path,
        &bumped,
        &[(
            "manifest.json".to_string(),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )],
        &[],
        &[],
    );
    match err(Project::open(&bumped)) {
        ProjectError::FormatVersion {
            found, supported, ..
        } => {
            assert_eq!(found, FORMAT_VERSION + 1);
            assert_eq!(supported, FORMAT_VERSION);
        }
        other => panic!("expected FormatVersion, got {other:?}"),
    }

    // A future manifest that also dropped a field this build's `Manifest`
    // requires still gets the named refusal, not a generic parse failure.
    let mut dropped = manifest.clone();
    dropped.as_object_mut().unwrap().remove("hash_algorithm");
    let bumped_and_dropped = dir.join("bumped-dropped.ffp");
    mutate_container(
        &project_path,
        &bumped_and_dropped,
        &[(
            "manifest.json".to_string(),
            serde_json::to_vec_pretty(&dropped).unwrap(),
        )],
        &[],
        &[],
    );
    assert!(matches!(
        err(Project::open(&bumped_and_dropped)),
        ProjectError::FormatVersion { .. }
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
        ZMapStorage::Full,
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
        crop: None,
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
