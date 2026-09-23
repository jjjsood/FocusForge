// SPDX-License-Identifier: GPL-3.0-or-later

//! The append surface: writing a second (and third, and fourth) checkpoint
//! onto an already-sealed project. Mirrors `project_format.rs`'s own
//! guarantees -- a failed write leaves the previous container untouched, two
//! appends of the same inputs are byte-identical apart from `created_at` --
//! re-pointed at the append path rather than reinvented for it.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use focusforge_core::io::{Rect, TiffSpec, TiffWriter};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::project::{self, AppendRequest, Metrics, Project, SealRequest};
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};

const W: u32 = 64;
const H: u32 = 48;
const SPP: u32 = 3;
const FRAMES: u32 = 4;

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

/// A smooth horizontal ramp, offset by `variant` -- distinct content per
/// append, so a carried-forward blob cannot pass by coincidence.
fn composite_samples(variant: u16) -> Vec<u16> {
    (0..(W as usize * H as usize * SPP as usize))
        .map(|i| {
            let x = (i / SPP as usize) % W as usize;
            let channel = i % SPP as usize;
            ((x * 65_535 / W as usize) as u16)
                .wrapping_add(channel as u16 * 97)
                .wrapping_add(variant)
        })
        .collect()
}

/// A piecewise-constant Z-Map, offset by `variant`. Two calls with the same
/// `variant` produce byte-identical planes -- what proves the blob-dedup
/// path when an append's Z-Map does not change.
fn zmap_samples(variant: u16) -> Vec<u16> {
    (0..(W as usize * H as usize))
        .map(|i| (((i / (W as usize * 4)) + variant as usize) % FRAMES as usize) as u16)
        .collect()
}

fn stage_planes(dir: &Path, composite_variant: u16, zmap_variant: u16) -> (PathBuf, PathBuf) {
    let composite = dir.join("composite.tif");
    let zmap = dir.join("zmap.tif");
    let mut writer = TiffWriter::create(
        &composite,
        TiffSpec {
            width: W,
            height: H,
            samples_per_pixel: SPP,
            description: Some("staged composite".to_string()),
        },
    )
    .unwrap();
    writer
        .write_tile(full(W, H), &composite_samples(composite_variant))
        .unwrap();
    writer.finish().unwrap();

    let mut map = ZMapWriter::create(&zmap, ZMapInfo::new(W, H, FRAMES).unwrap()).unwrap();
    map.write_tile(full(W, H), &zmap_samples(zmap_variant))
        .unwrap();
    map.finish().unwrap();
    (composite, zmap)
}

fn write_sources(dir: &Path, count: usize) -> Vec<PathBuf> {
    (0..count)
        .map(|i| {
            let path = dir.join(format!("frame-{i:02}.tif"));
            let mut w = TiffWriter::create(
                &path,
                TiffSpec {
                    width: W,
                    height: H,
                    samples_per_pixel: SPP,
                    description: None,
                },
            )
            .unwrap();
            let samples: Vec<u16> = (0..(W as usize * H as usize * SPP as usize))
                .map(|j| ((j + i * 13) % 65_536) as u16)
                .collect();
            w.write_tile(full(W, H), &samples).unwrap();
            w.finish().unwrap();
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
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    }
}

fn append_request<'a>(
    project_path: &'a Path,
    composite: &'a Path,
    zmap: &'a Path,
    sources: &'a [PathBuf],
    values: &'a ParamValues,
) -> AppendRequest<'a> {
    AppendRequest {
        project_path,
        staged_composite: composite,
        staged_zmap: zmap,
        sources,
        algorithm: "laplacian",
        params: values,
        align: None,
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    }
}

/// One sealed project in a fresh directory, plus the sources it was sealed
/// from -- what every append in this file starts from.
fn sealed(name: &str) -> (PathBuf, PathBuf, Vec<PathBuf>) {
    let dir = tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging, 0, 0);
    let sources = write_sources(&dir, 3);
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
    (dir, project_path, sources)
}

/// Appends one checkpoint onto `project_path`, with distinct composite and
/// Z-Map content per `(composite_variant, zmap_variant)`.
fn append_once(
    dir: &Path,
    project_path: &Path,
    sources: &[PathBuf],
    composite_variant: u16,
    zmap_variant: u16,
) -> project::SealedProject {
    let staging = dir.join(format!("staging-{composite_variant}-{zmap_variant}"));
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging, composite_variant, zmap_variant);
    let values = params();
    project::append(append_request(
        project_path,
        &composite,
        &zmap,
        sources,
        &values,
    ))
    .expect("append must succeed")
}

fn entry_names(path: &Path) -> Vec<String> {
    let file = std::fs::File::open(path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect()
}

/// Copies `src` to `dst`, dropping one named entry entirely -- what produces
/// a manifest that names a blob the archive does not hold.
fn remove_entry(src: &Path, dst: &Path, entry: &str) {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(src).unwrap()).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    let mut writer = zip::write::ZipWriter::new(std::fs::File::create(dst).unwrap());
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for name in names {
        if name == entry {
            continue;
        }
        let mut bytes = Vec::new();
        archive
            .by_name(&name)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        writer.start_file(&*name, options).unwrap();
        writer.write_all(&bytes).unwrap();
    }
    writer.finish().unwrap();
}

#[test]
fn appending_parents_the_new_checkpoint_on_the_head() {
    let (dir, project_path, sources) = sealed("append_parents");
    let first_head = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();

    let appended = append_once(&dir, &project_path, &sources, 1, 1);
    assert_ne!(appended.checkpoint, first_head);

    let mut project = Project::open(&project_path).unwrap();
    assert_eq!(project.checkpoint().id, appended.checkpoint);
    assert_eq!(
        project.checkpoint().parent.as_deref(),
        Some(first_head.as_str())
    );
    assert!(project.checkpoint().ops.is_empty());

    let chain = project.checkpoint_chain().unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].id, appended.checkpoint);
    assert_eq!(chain[1].id, first_head);
}

#[test]
fn an_earlier_checkpoint_and_its_blob_survive_an_append() {
    let (dir, project_path, sources) = sealed("append_survive");
    let first_project = Project::open(&project_path).unwrap();
    let first_head = first_project.checkpoint().id.clone();
    let first_zmap_blob = first_project.checkpoint().zmap_blob.clone();
    drop(first_project);

    append_once(&dir, &project_path, &sources, 1, 1);

    let names = entry_names(&project_path);
    assert!(
        names.contains(&format!("checkpoints/{first_head}.json")),
        "the first checkpoint's own entry must survive: {names:?}"
    );
    assert!(
        names.contains(&format!("blobs/{first_zmap_blob}.zst")),
        "the first checkpoint's own Z-Map blob must survive: {names:?}"
    );

    let mut project = Project::open(&project_path).unwrap();
    let chain = project.checkpoint_chain().unwrap();
    let root = chain.last().expect("a chain always has a root");
    assert_eq!(root.id, first_head);
    assert_eq!(root.parent, None);
    assert_eq!(root.zmap_blob, first_zmap_blob);
}

#[test]
fn the_chain_extends_past_two() {
    let (dir, project_path, sources) = sealed("append_chain");
    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();

    let second = append_once(&dir, &project_path, &sources, 1, 1);
    // A Z-Map identical to the one just written: proves the blob-dedup path
    // and the parent-aware id derivation both hold when the content hash
    // does not change.
    let third = append_once(&dir, &project_path, &sources, 2, 1);
    let fourth = append_once(&dir, &project_path, &sources, 3, 2);

    assert_ne!(third.checkpoint, second.checkpoint);
    assert_ne!(fourth.checkpoint, third.checkpoint);

    let mut project = Project::open(&project_path).unwrap();
    assert_eq!(project.checkpoint().id, fourth.checkpoint);
    let chain = project.checkpoint_chain().unwrap();
    let ids: Vec<&str> = chain.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            fourth.checkpoint.as_str(),
            third.checkpoint.as_str(),
            second.checkpoint.as_str(),
            root_id.as_str(),
        ]
    );
}

#[test]
fn a_single_checkpoint_project_still_opens_and_can_be_appended_to() {
    let (dir, project_path, sources) = sealed("append_legacy");
    let mut project = Project::open(&project_path).unwrap();
    assert_eq!(project.checkpoint().parent, None);
    assert_eq!(project.checkpoint_chain().unwrap().len(), 1);
    drop(project);

    let appended = append_once(&dir, &project_path, &sources, 1, 1);
    let mut reopened = Project::open(&project_path).unwrap();
    assert_eq!(reopened.checkpoint().id, appended.checkpoint);
    assert_eq!(reopened.checkpoint_chain().unwrap().len(), 2);
}

#[test]
fn a_failed_append_leaves_the_sealed_project_untouched() {
    let (dir, project_path, sources) = sealed("append_failed");
    let composite_blob = Project::open(&project_path)
        .unwrap()
        .plane(project::Plane::Composite)
        .blob
        .clone();
    let corrupted = project_path.with_extension("corrupted.ffp");
    remove_entry(
        &project_path,
        &corrupted,
        &format!("blobs/{composite_blob}.zst"),
    );

    let before = std::fs::read(&corrupted).unwrap();

    let staging = dir.join("staging-failed");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging, 1, 1);
    let values = params();
    let err = project::append(append_request(
        &corrupted, &composite, &zmap, &sources, &values,
    ))
    .expect_err("an append over a manifest naming a missing blob must fail");
    assert!(
        err.to_string().contains(&composite_blob),
        "the refusal must name the missing entry: {err}"
    );

    let after = std::fs::read(&corrupted).unwrap();
    assert_eq!(
        before, after,
        "a failed append must leave the sealed project byte-identical"
    );
    assert!(
        !project::partial_path(&corrupted).exists(),
        "a failed append must leave no .partial behind"
    );
}

#[test]
fn an_append_is_reproducible() {
    let (dir, project_path, sources) = sealed("append_repro");
    let staging = dir.join("staging-repro");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging, 1, 1);
    let values = params();

    let copy_a = dir.join("copy_a.ffp");
    let copy_b = dir.join("copy_b.ffp");
    std::fs::copy(&project_path, &copy_a).unwrap();
    std::fs::copy(&project_path, &copy_b).unwrap();

    project::append_at(
        append_request(&copy_a, &composite, &zmap, &sources, &values),
        1_700_000_000,
    )
    .unwrap();
    project::append_at(
        append_request(&copy_b, &composite, &zmap, &sources, &values),
        1_700_000_000,
    )
    .unwrap();

    assert_eq!(
        std::fs::read(&copy_a).unwrap(),
        std::fs::read(&copy_b).unwrap(),
        "two appends of the same inputs at the same instant are not byte-identical"
    );
}

// ---------------------------------------------------------------------
// `seal_held`: a chain of checkpoints kept outside any container, written
// as one atomic replace of the target.
// ---------------------------------------------------------------------

fn held_checkpoint(
    dir: &Path,
    sources: &[PathBuf],
    variant: u16,
    created_at: i64,
) -> project::HeldCheckpoint {
    let staging = dir.join(format!("held-{variant}"));
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging, variant, variant);
    project::HeldCheckpoint {
        composite,
        zmap,
        width: W,
        height: H,
        sources: sources.to_vec(),
        algorithm: "laplacian".to_string(),
        params: params(),
        align: None,
        composite_description: "staged composite".to_string(),
        metrics: Metrics::default(),
        created_at,
    }
}

/// Every file in `dir` whose name marks an unfinished write.
fn leftover_temporaries(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".saving") || name.contains(".partial"))
        .collect()
}

#[test]
fn seal_held_keeps_each_checkpoints_created_at() {
    let dir = tmp_dir("seal_held_created_at");
    let sources = write_sources(&dir, 3);
    let held = [
        held_checkpoint(&dir, &sources, 1, 1_000),
        held_checkpoint(&dir, &sources, 2, 2_000),
    ];
    let target = dir.join(format!("demo.{}", project::PROJECT_EXTENSION));
    let sealed = project::seal_held(&target, &held, false).expect("seal_held must succeed");
    assert_eq!(sealed.path, target);

    let chain = Project::open(&target).unwrap().checkpoint_chain().unwrap();
    let created: Vec<i64> = chain.iter().rev().map(|c| c.created_at).collect();
    assert_eq!(created, [1_000, 2_000], "oldest first, each its own time");
    assert!(leftover_temporaries(&dir).is_empty());
}

#[test]
fn seal_held_onto_an_existing_project_extends_its_chain() {
    let (dir, project_path, sources) = sealed("seal_held_onto_existing");
    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();

    let held = [held_checkpoint(&dir, &sources, 1, 3_000)];
    project::seal_held(&project_path, &held, true).expect("seal_held must succeed");

    let chain = Project::open(&project_path)
        .unwrap()
        .checkpoint_chain()
        .unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].created_at, 3_000);
    assert_eq!(chain[0].parent.as_deref(), Some(root_id.as_str()));
    assert!(leftover_temporaries(&dir).is_empty());
}

#[test]
fn seal_held_failing_midway_leaves_the_target_untouched() {
    let (dir, project_path, sources) = sealed("seal_held_fails");
    let before = std::fs::read(&project_path).unwrap();

    let first = held_checkpoint(&dir, &sources, 1, 3_000);
    let second = held_checkpoint(&dir, &sources, 2, 4_000);
    std::fs::remove_file(&second.composite).unwrap();

    assert!(project::seal_held(&project_path, &[first, second], true).is_err());
    assert_eq!(
        std::fs::read(&project_path).unwrap(),
        before,
        "a failed save leaves the target byte-identical"
    );
    assert!(leftover_temporaries(&dir).is_empty());
}

#[test]
fn seal_held_refuses_an_empty_chain() {
    let dir = tmp_dir("seal_held_empty");
    let target = dir.join(format!("demo.{}", project::PROJECT_EXTENSION));
    assert!(project::seal_held(&target, &[], false).is_err());
    assert!(!target.exists());
}
