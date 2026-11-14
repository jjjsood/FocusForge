// SPDX-License-Identifier: GPL-3.0-or-later

//! The append surface: writing a second (and third, and fourth) checkpoint
//! onto an already-sealed project. Mirrors `project_format.rs`'s own
//! guarantees -- a failed write leaves the previous container untouched, two
//! appends of the same inputs are byte-identical apart from `created_at` --
//! re-pointed at the append path rather than reinvented for it.

use std::path::{Path, PathBuf};

use focusforge_core::io::{Rect, TiffSpec, TiffWriter};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::project::container::{ContainerWriter, Index};
use focusforge_core::project::{
    self, AppendRequest, BrushMode, Metrics, Op, ParentRef, Point, Project, SealRequest,
    StrokeSource,
};
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
    write_sources_seeded(dir, count, 0)
}

/// Like [`write_sources`], with sample content that differs per `seed`.
fn write_sources_seeded(dir: &Path, count: usize, seed: usize) -> Vec<PathBuf> {
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
                .map(|j| ((j + i * 13 + seed * 7) % 65_536) as u16)
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
        registration: focusforge_core::align::Registration::default(),
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
        registration: focusforge_core::align::Registration::default(),
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
    Index::read(path)
        .unwrap()
        .names()
        .map(str::to_string)
        .collect()
}

/// Copies `src` to `dst`, dropping one named entry entirely -- what produces
/// a manifest that names a blob the archive does not hold.
fn remove_entry(src: &Path, dst: &Path, entry: &str) {
    let index = Index::read(src).unwrap();
    let mut writer = ContainerWriter::create(dst).unwrap();
    for name in index.names().filter(|name| *name != entry) {
        writer
            .put_bytes(name, &index.read_entry(src, name).unwrap())
            .unwrap();
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

    let project = Project::open(&project_path).unwrap();
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

    let project = Project::open(&project_path).unwrap();
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

    let project = Project::open(&project_path).unwrap();
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
    let project = Project::open(&project_path).unwrap();
    assert_eq!(project.checkpoint().parent, None);
    assert_eq!(project.checkpoint_chain().unwrap().len(), 1);
    drop(project);

    let appended = append_once(&dir, &project_path, &sources, 1, 1);
    let reopened = Project::open(&project_path).unwrap();
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
// `seal_in_memory`: checkpoints kept outside any container, each written
// under its own recorded parent as one atomic replace of the target.
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
        registration: focusforge_core::align::Registration::default(),
        composite_description: "staged composite".to_string(),
        metrics: Metrics::default(),
        created_at,
        compressed_composite: None,
    }
}

fn fused_entry(
    held: project::HeldCheckpoint,
    parent: Option<ParentRef>,
) -> project::InMemoryCheckpoint {
    project::InMemoryCheckpoint {
        kind: project::InMemoryKind::Fused(held),
        parent,
    }
}

fn ops_entry(ops: Vec<Op>, created_at: i64, parent: ParentRef) -> project::InMemoryCheckpoint {
    project::InMemoryCheckpoint {
        kind: project::InMemoryKind::Ops { ops, created_at },
        parent: Some(parent),
    }
}

fn saved(id: &str) -> ParentRef {
    ParentRef::Saved(id.to_string())
}

/// One brush stroke painting frame `source_id` from `(x, y)` down and right.
fn stroke(source_id: u16, x: i32, y: i32) -> Op {
    Op::frame_stroke(
        source_id,
        vec![
            Point { x, y },
            Point {
                x: x + 12,
                y: y + 8,
            },
        ],
        5.0,
        0.0,
    )
}

/// `held` written as one linear chain: the first under the project's head
/// when `onto_existing`, else as the root; every later one under the one
/// before it.
fn seal_chain(
    target: &Path,
    held: &[project::HeldCheckpoint],
    onto_existing: bool,
) -> Result<Vec<String>, project::ProjectError> {
    let entries: Vec<project::InMemoryCheckpoint> = held
        .iter()
        .enumerate()
        .map(|(i, checkpoint)| {
            let parent = if i > 0 {
                Some(ParentRef::Entry(i - 1))
            } else if onto_existing {
                Some(saved(&head_of(target)))
            } else {
                None
            };
            fused_entry(checkpoint.clone(), parent)
        })
        .collect();
    project::seal_in_memory(target, &entries, &[], None, onto_existing)
}

/// Every file in `dir` whose name marks an unfinished write.
fn leftover_temporaries(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".partial"))
        .collect()
}

fn parent_of(path: &Path, id: &str) -> Option<String> {
    Project::open(path)
        .unwrap()
        .history()
        .get(id)
        .unwrap()
        .parent
        .clone()
}

fn head_of(path: &Path) -> String {
    Project::open(path).unwrap().checkpoint().id.clone()
}

fn all_ids(path: &Path) -> Vec<String> {
    let project = Project::open(path).unwrap();
    let mut ids: Vec<String> = project
        .history()
        .all()
        .iter()
        .map(|c| c.id.clone())
        .collect();
    ids.sort();
    ids
}

#[test]
fn seal_chain_keeps_each_checkpoints_created_at() {
    let dir = tmp_dir("seal_chain_created_at");
    let sources = write_sources(&dir, 3);
    let held = [
        held_checkpoint(&dir, &sources, 1, 1_000),
        held_checkpoint(&dir, &sources, 2, 2_000),
    ];
    let target = dir.join(format!("demo.{}", project::PROJECT_EXTENSION));
    let ids = seal_chain(&target, &held, false).expect("seal_in_memory must succeed");
    assert_eq!(ids.len(), 2);

    let chain = Project::open(&target).unwrap().checkpoint_chain().unwrap();
    let created: Vec<i64> = chain.iter().rev().map(|c| c.created_at).collect();
    assert_eq!(created, [1_000, 2_000], "oldest first, each its own time");
    assert!(leftover_temporaries(&dir).is_empty());
}

#[test]
fn seal_chain_onto_an_existing_project_extends_its_chain() {
    let (dir, project_path, sources) = sealed("seal_chain_onto_existing");
    let root_id = head_of(&project_path);

    let held = [held_checkpoint(&dir, &sources, 1, 3_000)];
    seal_chain(&project_path, &held, true).expect("seal_in_memory must succeed");

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
fn seal_chain_failing_midway_leaves_the_target_untouched() {
    let (dir, project_path, sources) = sealed("seal_chain_fails");
    let before = std::fs::read(&project_path).unwrap();

    let first = held_checkpoint(&dir, &sources, 1, 3_000);
    let second = held_checkpoint(&dir, &sources, 2, 4_000);
    std::fs::remove_file(&second.composite).unwrap();

    assert!(seal_chain(&project_path, &[first, second], true).is_err());
    assert_eq!(
        std::fs::read(&project_path).unwrap(),
        before,
        "a failed save leaves the target byte-identical"
    );
    assert!(leftover_temporaries(&dir).is_empty());
}

#[test]
fn seal_chain_refuses_an_empty_chain() {
    let dir = tmp_dir("seal_chain_empty");
    let target = dir.join(format!("demo.{}", project::PROJECT_EXTENSION));
    assert!(seal_chain(&target, &[], false).is_err());
    assert!(!target.exists());
}

#[test]
fn sealing_in_memory_checkpoints_writes_each_under_its_own_parent() {
    // Never saved: fused A (root), ops onto A, fused B under A (not under
    // the ops checkpoint, the newest), ops onto B.
    let dir = tmp_dir("in_memory_parents_new");
    let sources = write_sources(&dir, 3);
    let target = dir.join(format!("demo.{}", project::PROJECT_EXTENSION));
    let entries = [
        fused_entry(held_checkpoint(&dir, &sources, 1, 1_000), None),
        ops_entry(vec![stroke(2, 10, 10)], 1_100, ParentRef::Entry(0)),
        fused_entry(
            held_checkpoint(&dir, &sources, 2, 1_200),
            Some(ParentRef::Entry(0)),
        ),
        ops_entry(vec![stroke(3, 20, 20)], 1_300, ParentRef::Entry(2)),
    ];
    let ids = project::seal_in_memory(&target, &entries, &[], None, false).unwrap();
    assert_eq!(ids.len(), 4);
    assert_eq!(parent_of(&target, ids[0].as_str()), None);
    assert_eq!(
        parent_of(&target, ids[1].as_str()).as_deref(),
        Some(ids[0].as_str())
    );
    assert_eq!(
        parent_of(&target, ids[2].as_str()).as_deref(),
        Some(ids[0].as_str()),
        "a fused entry is written under its own parent, not the newest checkpoint"
    );
    assert_eq!(
        parent_of(&target, ids[3].as_str()).as_deref(),
        Some(ids[2].as_str())
    );
    assert_eq!(all_ids(&target).len(), 4);
    assert!(leftover_temporaries(&dir).is_empty());

    // Opened: two saved checkpoints; ops onto the older, a fused entry onto
    // that ops entry, and a fused entry onto the older one again.
    let (dir, project_path, sources) = sealed("in_memory_parents_open");
    let root = head_of(&project_path);
    let newer = append_once(&dir, &project_path, &sources, 1, 1).checkpoint;
    assert_eq!(head_of(&project_path), newer);
    let before = all_ids(&project_path);
    let entries = [
        ops_entry(vec![stroke(2, 10, 10)], 5_000, saved(&root)),
        fused_entry(
            held_checkpoint(&dir, &sources, 2, 5_100),
            Some(ParentRef::Entry(0)),
        ),
        fused_entry(
            held_checkpoint(&dir, &sources, 3, 5_200),
            Some(saved(&root)),
        ),
    ];
    let ids = project::seal_in_memory(&project_path, &entries, &[], None, true).unwrap();
    assert_eq!(
        parent_of(&project_path, ids[0].as_str()).as_deref(),
        Some(root.as_str())
    );
    assert_eq!(
        parent_of(&project_path, ids[1].as_str()).as_deref(),
        Some(ids[0].as_str())
    );
    assert_eq!(
        parent_of(&project_path, ids[2].as_str()).as_deref(),
        Some(root.as_str()),
        "a fused entry whose parent is not the head is written under that parent"
    );
    let after = all_ids(&project_path);
    for id in &before {
        assert!(after.contains(id), "saved checkpoint {id} survives");
    }
    assert_eq!(after.len(), before.len() + 3);
    assert!(leftover_temporaries(&dir).is_empty());
}

#[test]
fn in_memory_op_checkpoints_name_their_fused_ancestors_map() {
    let (dir, project_path, sources) = sealed("in_memory_ops_fused_map");
    let root = head_of(&project_path);
    let first_ops = vec![stroke(2, 10, 10), stroke(1, 30, 25)];
    let second_ops = vec![stroke(3, 25, 12)];
    let third_ops = vec![stroke(1, 5, 30)];
    let entries = [
        ops_entry(first_ops.clone(), 7_000, saved(&root)),
        ops_entry(second_ops.clone(), 7_100, ParentRef::Entry(0)),
        fused_entry(
            held_checkpoint(&dir, &sources, 2, 7_200),
            Some(saved(&root)),
        ),
        ops_entry(third_ops.clone(), 7_300, ParentRef::Entry(2)),
    ];
    let ids = project::seal_in_memory(&project_path, &entries, &[], None, true).unwrap();

    let project = Project::open(&project_path).unwrap();
    let zmap_of = |id: &str| project.history().get(id).unwrap().zmap_blob.clone();
    let cases: [(usize, &[Op], String); 4] = [
        (0, &first_ops, zmap_of(&root)),
        (1, &second_ops, zmap_of(&root)),
        (2, &[], zmap_of(&ids[2])),
        (3, &third_ops, zmap_of(&ids[2])),
    ];
    for (entry, ops, fused_zmap) in cases {
        let got = project.history().get(ids[entry].as_str()).unwrap();
        assert_eq!(got.ops, ops, "entry {entry} carries its own ops");
        assert_eq!(
            got.zmap_blob, fused_zmap,
            "entry {entry} names its fused ancestor's map"
        );
    }

    for (entry, created_at) in [(0, 7_000), (1, 7_100), (2, 7_200), (3, 7_300)] {
        assert_eq!(
            project
                .history()
                .get(ids[entry].as_str())
                .unwrap()
                .created_at,
            created_at
        );
    }
}

#[test]
fn sealing_in_memory_checkpoints_refuses_a_bad_parent_and_leaves_the_target_untouched() {
    let (dir, project_path, sources) = sealed("in_memory_refusals");
    let root = head_of(&project_path);
    let before = std::fs::read(&project_path).unwrap();
    let held = |variant: u16, at: i64| held_checkpoint(&dir, &sources, variant, at);

    let refused: Vec<(&str, Vec<project::InMemoryCheckpoint>)> = vec![
        (
            "forward index",
            vec![
                fused_entry(held(1, 1_000), Some(saved(&root))),
                fused_entry(held(2, 1_100), Some(ParentRef::Entry(2))),
                fused_entry(held(3, 1_200), Some(saved(&root))),
            ],
        ),
        (
            "self index",
            vec![fused_entry(held(1, 1_000), Some(ParentRef::Entry(0)))],
        ),
        (
            "unknown saved id",
            vec![ops_entry(vec![stroke(2, 10, 10)], 1_000, saved("nope"))],
        ),
        (
            "parentless entry onto an existing project",
            vec![fused_entry(held(1, 1_000), None)],
        ),
        ("no entries and no deletes", Vec::new()),
    ];
    for (what, entries) in refused {
        assert!(
            project::seal_in_memory(&project_path, &entries, &[], None, true).is_err(),
            "{what} must be refused"
        );
        assert_eq!(
            std::fs::read(&project_path).unwrap(),
            before,
            "{what} leaves the target byte-identical"
        );
        assert!(leftover_temporaries(&dir).is_empty(), "{what}");
    }

    let second = held(2, 1_100);
    let entries = [
        fused_entry(held(1, 1_000), Some(saved(&root))),
        fused_entry(second.clone(), Some(ParentRef::Entry(0))),
    ];
    std::fs::remove_file(&second.composite).unwrap();
    assert!(project::seal_in_memory(&project_path, &entries, &[], None, true).is_err());
    assert_eq!(std::fs::read(&project_path).unwrap(), before);
    assert!(leftover_temporaries(&dir).is_empty());

    // A project that does not exist yet: nothing to write, nothing created.
    let target = dir.join(format!("fresh.{}", project::PROJECT_EXTENSION));
    assert!(project::seal_in_memory(&target, &[], &[], None, false).is_err());
    let parented = [fused_entry(held(4, 1_000), Some(saved(&root)))];
    assert!(project::seal_in_memory(&target, &parented, &[], None, false).is_err());
    assert!(!target.exists());
    assert!(leftover_temporaries(&dir).is_empty());
}

/// A saved root with a -> {b, c}: the project, its sources and those ids.
struct Branched {
    dir: PathBuf,
    path: PathBuf,
    sources: Vec<PathBuf>,
    root: String,
    a: String,
    b: String,
    c: String,
}

fn branched(name: &str) -> Branched {
    let (dir, path, sources) = sealed(name);
    let root = head_of(&path);
    let a = append_once(&dir, &path, &sources, 1, 1).checkpoint;
    let onto = |parent: &str, op: Op, at: i64| {
        project::append_ops_onto(&path, parent, vec![op], None, at)
            .unwrap()
            .checkpoint
    };
    let b = onto(&a, stroke(1, 5, 5), 2_000);
    let c = onto(&a, stroke(2, 30, 30), 2_100);
    Branched {
        dir,
        path,
        sources,
        root,
        a,
        b,
        c,
    }
}

fn visible_sorted(path: &Path) -> Vec<String> {
    let mut ids: Vec<String> = Project::open(path)
        .unwrap()
        .history()
        .visible()
        .iter()
        .map(|c| c.id.clone())
        .collect();
    ids.sort();
    ids
}

#[test]
fn sealing_hides_pending_deletes_and_writes_entries_under_them() {
    // Delete a: it stays stored (b and c are built on it), and the entries
    // under a and under b are written where they were recorded.
    let t = branched("in_memory_hides");
    let entries = [
        ops_entry(vec![stroke(3, 10, 10)], 3_000, saved(&t.b)),
        ops_entry(vec![stroke(1, 20, 20)], 3_100, saved(&t.a)),
        ops_entry(vec![stroke(2, 25, 25)], 3_200, ParentRef::Entry(0)),
    ];
    let ids: Vec<String> =
        project::seal_in_memory(&t.path, &entries, &[saved(&t.a)], None, true).unwrap();
    assert_eq!(ids.len(), 3);
    assert_eq!(parent_of(&t.path, &ids[0]).as_deref(), Some(t.b.as_str()));
    assert_eq!(parent_of(&t.path, &ids[1]).as_deref(), Some(t.a.as_str()));
    assert_eq!(
        parent_of(&t.path, &ids[2]).as_deref(),
        Some(ids[0].as_str())
    );
    assert_eq!(all_ids(&t.path).len(), 7);
    let mut visible = vec![t.root.clone(), t.b.clone(), t.c.clone()];
    visible.extend(ids.iter().cloned());
    visible.sort();
    assert_eq!(visible_sorted(&t.path), visible);
    assert!(
        Project::open(&t.path)
            .unwrap()
            .history()
            .get(&t.a)
            .unwrap()
            .hidden
    );
    assert!(visible.contains(&head_of(&t.path)));

    // A delete with nothing visible left under it removes the checkpoint.
    let t = branched("in_memory_hides_leaf");
    let ids = project::seal_in_memory(&t.path, &[], &[saved(&t.b)], None, true).unwrap();
    assert!(ids.is_empty());
    assert!(!all_ids(&t.path).contains(&t.b));
    assert_eq!(all_ids(&t.path).len(), 3);

    // Hidden first, collected with its last visible descendant.
    let t = branched("in_memory_hides_all_below");
    let deletes = [saved(&t.a), saved(&t.b), saved(&t.c)];
    project::seal_in_memory(&t.path, &[], &deletes, None, true).unwrap();
    assert_eq!(all_ids(&t.path), std::slice::from_ref(&t.root));
    assert_eq!(head_of(&t.path), t.root);

    // Refusals leave the bytes alone.
    let t = branched("in_memory_hides_refused");
    let before = std::fs::read(&t.path).unwrap();
    let entry = || {
        [fused_entry(
            held_checkpoint(&t.dir, &t.sources, 2, 3_100),
            Some(saved(&t.root)),
        )]
    };
    for (what, deletes, onto_existing) in [
        ("unknown id", vec![saved("nope")], true),
        ("a delete on a new project", vec![saved(&t.a)], false),
    ] {
        assert!(
            project::seal_in_memory(&t.path, &entry(), &deletes, None, onto_existing).is_err(),
            "{what}"
        );
        assert_eq!(std::fs::read(&t.path).unwrap(), before, "{what}");
    }
    let everything = [saved(&t.root), saved(&t.a), saved(&t.b), saved(&t.c)];
    assert!(project::seal_in_memory(&t.path, &[], &everything, None, true).is_err());
    assert_eq!(std::fs::read(&t.path).unwrap(), before, "every visible");
    // A failure midway with a delete pending.
    let broken = entry();
    let project::InMemoryKind::Fused(held) = &broken[0].kind else {
        unreachable!("the entry is fused")
    };
    std::fs::remove_file(&held.composite).unwrap();
    assert!(project::seal_in_memory(&t.path, &broken, &[saved(&t.a)], None, true).is_err());
    assert_eq!(std::fs::read(&t.path).unwrap(), before);
    assert!(leftover_temporaries(&t.dir).is_empty());
}

fn is_hidden(path: &Path, id: &str) -> bool {
    Project::open(path)
        .unwrap()
        .history()
        .get(id)
        .unwrap()
        .hidden
}

/// A fused entry whose planes are gone, so writing it would fail.
fn unreadable_entry(
    dir: &Path,
    sources: &[PathBuf],
    variant: u16,
    parent: Option<ParentRef>,
) -> project::InMemoryCheckpoint {
    let held = held_checkpoint(dir, sources, variant, 4_000 + i64::from(variant));
    std::fs::remove_file(&held.composite).unwrap();
    std::fs::remove_file(&held.zmap).unwrap();
    fused_entry(held, parent)
}

#[test]
fn sealing_keeps_the_pinned_row_and_skips_unneeded_entries() {
    // The pin on a hidden saved row keeps it stored; the next save without
    // the pin collects it.
    let t = branched("pin_saved");
    project::seal_in_memory(&t.path, &[], &[saved(&t.b)], Some(&saved(&t.b)), true).unwrap();
    assert!(all_ids(&t.path).contains(&t.b));
    assert!(is_hidden(&t.path, &t.b));
    let later = [ops_entry(vec![stroke(3, 40, 40)], 5_000, saved(&t.c))];
    project::seal_in_memory(&t.path, &later, &[], None, true).unwrap();
    assert!(
        !all_ids(&t.path).contains(&t.b),
        "the unpinned row is collected"
    );

    // A pinned op-bearing row keeps its hidden parent stored too.
    let t = branched("pin_with_hidden_parent");
    let deletes = [saved(&t.a), saved(&t.b)];
    project::seal_in_memory(&t.path, &[], &deletes, Some(&saved(&t.b)), true).unwrap();
    assert!(is_hidden(&t.path, &t.a) && is_hidden(&t.path, &t.b));
    let mut visible = vec![t.root.clone(), t.c.clone()];
    visible.sort();
    assert_eq!(visible_sorted(&t.path), visible);

    // A deleted entry nothing builds on is not written, and its planes are
    // never read.
    let t = branched("skip_unneeded");
    let entries = [
        unreadable_entry(&t.dir, &t.sources, 5, Some(saved(&t.root))),
        ops_entry(vec![stroke(3, 10, 10)], 6_000, saved(&t.c)),
    ];
    let deletes = [ParentRef::Entry(0)];
    assert_eq!(
        project::kept_entries(&entries, &deletes, None),
        [false, true]
    );
    let ids = project::seal_in_memory(&t.path, &entries, &deletes, None, true).unwrap();
    assert_eq!(ids.len(), 1, "one id per written entry");
    assert_eq!(all_ids(&t.path).len(), 5);

    // A deleted entry a written one is built on is stored hidden.
    let t = branched("hidden_entry");
    let entries = [
        ops_entry(vec![stroke(3, 10, 10)], 6_000, saved(&t.c)),
        ops_entry(vec![stroke(1, 20, 20)], 6_100, ParentRef::Entry(0)),
    ];
    let deletes = [ParentRef::Entry(0)];
    assert_eq!(
        project::kept_entries(&entries, &deletes, None),
        [true, true]
    );
    let ids = project::seal_in_memory(&t.path, &entries, &deletes, None, true).unwrap();
    assert_eq!(ids.len(), 2);
    assert!(is_hidden(&t.path, &ids[0]));
    assert!(!is_hidden(&t.path, &ids[1]));

    // Pinned, an entry nothing builds on is stored hidden.
    let t = branched("pinned_entry");
    let entries = [
        ops_entry(vec![stroke(3, 10, 10)], 6_000, saved(&t.c)),
        ops_entry(vec![stroke(1, 20, 20)], 6_100, saved(&t.b)),
    ];
    let deletes = [ParentRef::Entry(0)];
    let keep = ParentRef::Entry(0);
    assert_eq!(
        project::kept_entries(&entries, &deletes, Some(&keep)),
        [true, true]
    );
    let ids = project::seal_in_memory(&t.path, &entries, &deletes, Some(&keep), true).unwrap();
    assert_eq!(ids.len(), 2);
    assert!(
        is_hidden(&t.path, &ids[0]),
        "the pinned entry is stored hidden"
    );
    assert_eq!(all_ids(&t.path).len(), 6);

    // A new project: a hidden root with a written child, and a skipped leaf.
    let (dir, _, sources) = sealed("pin_new_project");
    let entries = [
        fused_entry(held_checkpoint(&dir, &sources, 7, 7_000), None),
        fused_entry(
            held_checkpoint(&dir, &sources, 8, 7_100),
            Some(ParentRef::Entry(0)),
        ),
        unreadable_entry(&dir, &sources, 9, Some(ParentRef::Entry(1))),
    ];
    let deletes = [ParentRef::Entry(0), ParentRef::Entry(2)];
    let target = dir.join(format!("fresh.{}", project::PROJECT_EXTENSION));
    let ids = project::seal_in_memory(&target, &entries, &deletes, None, false).unwrap();
    assert_eq!(ids.len(), 2);
    assert_eq!(all_ids(&target).len(), 2);
    assert!(is_hidden(&target, &ids[0]));
}

/// A default stroke painted from checkpoint `id`.
fn checkpoint_stroke(id: &str) -> Op {
    Op::BrushStroke {
        source: StrokeSource::Checkpoint(id.to_string()),
        path: vec![Point { x: 3, y: 4 }, Point { x: 14, y: 9 }],
        radius: 5.0,
        softness: 0.0,
        mode: BrushMode::Normal,
        opacity: 1.0,
    }
}

fn manifest_entry(path: &Path) -> serde_json::Value {
    let bytes = Index::read(path)
        .unwrap()
        .read_entry(path, "manifest.json")
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn stored_format_version(path: &Path) -> Option<serde_json::Value> {
    manifest_entry(path).get("format_version").cloned()
}

/// Every stored stroke of every checkpoint record, as written.
fn stored_strokes(path: &Path) -> Vec<serde_json::Value> {
    let index = Index::read(path).unwrap();
    let mut strokes = Vec::new();
    for name in index.names().filter(|n| n.starts_with("checkpoints/")) {
        let record: serde_json::Value =
            serde_json::from_slice(&index.read_entry(path, name).unwrap()).unwrap();
        for op in record["ops"].as_array().unwrap() {
            strokes.push(op["brush-stroke"].clone());
        }
    }
    strokes
}

/// One sealed project per save entry point, each saved through that entry
/// point with `ops(root)` as its stroke(s). Returns the projects' paths.
fn saved_by_every_entry_point(tag: &str, ops: impl Fn(&str) -> Vec<Op>) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    let (_dir, path, _) = sealed(&format!("{tag}_append_ops"));
    project::append_ops(&path, ops(&head_of(&path)), None).unwrap();
    paths.push(path);

    let (_dir, path, _) = sealed(&format!("{tag}_append_ops_at"));
    project::append_ops_at(&path, ops(&head_of(&path)), None, 4_000).unwrap();
    paths.push(path);

    let (dir, path, sources) = sealed(&format!("{tag}_append_ops_onto"));
    let root = head_of(&path);
    append_once(&dir, &path, &sources, 1, 1);
    project::append_ops_onto(&path, &root, ops(&root), None, 4_100).unwrap();
    paths.push(path);

    let (_dir, path, _) = sealed(&format!("{tag}_append_op_chain"));
    let root = head_of(&path);
    let mut chain = vec![stroke(1, 2, 2)];
    chain.extend(ops(&root));
    chain.push(stroke(2, 6, 6));
    project::append_op_chain(&path, None, chain, None, 4_200).unwrap();
    paths.push(path);

    let (_dir, path, _) = sealed(&format!("{tag}_seal_in_memory"));
    let root = head_of(&path);
    let entries = [ops_entry(ops(&root), 4_300, saved(&root))];
    project::seal_in_memory(&path, &entries, &[], None, true).unwrap();
    paths.push(path);

    paths
}

#[test]
fn checkpoint_stroke_stamps_the_format_version() {
    let paths = saved_by_every_entry_point("stamp", |root| vec![checkpoint_stroke(root)]);
    for path in &paths {
        assert_eq!(
            stored_format_version(path),
            Some(serde_json::json!(2)),
            "{}",
            path.display()
        );
        assert!(Project::open(path).is_ok());
    }

    // A later save of another kind keeps the stamp.
    let path = &paths[0];
    project::append_ops(path, vec![stroke(1, 1, 1)], None).unwrap();
    assert_eq!(stored_format_version(path), Some(serde_json::json!(2)));

    let dir = path.parent().unwrap().to_path_buf();
    let sources = write_sources(&dir, 3);
    append_once(&dir, path, &sources, 5, 5);
    assert_eq!(
        stored_format_version(path),
        Some(serde_json::json!(2)),
        "a re-fuse append"
    );

    project::container::compact(path).unwrap();
    assert_eq!(
        stored_format_version(path),
        Some(serde_json::json!(2)),
        "compact"
    );
    assert!(Project::open(path).is_ok());
}

fn colour_of(path: &Path, id: &str) -> u32 {
    Project::open(path)
        .unwrap()
        .history()
        .colour_index(id)
        .unwrap()
}

/// The `colour_index` as stored in checkpoint `id`'s record.
fn stored_colour_of(path: &Path, id: &str) -> Option<u64> {
    let index = Index::read(path).unwrap();
    let record: serde_json::Value = serde_json::from_slice(
        &index
            .read_entry(path, &format!("checkpoints/{id}.json"))
            .unwrap(),
    )
    .unwrap();
    record.get("colour_index").map(|v| v.as_u64().unwrap())
}

#[test]
fn checkpoint_colour_index_numbers_each_write_one_above_the_largest() {
    let (dir, path, sources) = sealed("colour_numbers");
    let root = head_of(&path);
    let at = Project::open(&path).unwrap().checkpoint().created_at;
    assert_eq!(stored_colour_of(&path, &root), Some(0));

    let a = project::append_ops_at(&path, vec![stroke(1, 1, 1)], None, at + 10)
        .unwrap()
        .checkpoint;
    let b = project::append_ops_at(&path, vec![stroke(2, 2, 2)], None, at + 20)
        .unwrap()
        .checkpoint;
    let fused = append_once(&dir, &path, &sources, 3, 3).checkpoint;
    let branch = project::append_ops_onto(&path, &root, vec![stroke(0, 3, 3)], None, at + 30)
        .unwrap()
        .checkpoint;
    let chained =
        project::append_op_chain(&path, Some(&a), vec![stroke(1, 4, 4)], None, at + 40).unwrap();
    for (id, expected) in [
        (&root, 0),
        (&a, 1),
        (&b, 2),
        (&fused, 3),
        (&branch, 4),
        (&chained[0], 5),
    ] {
        assert_eq!(
            stored_colour_of(&path, id),
            Some(expected),
            "stored for {id}"
        );
        assert_eq!(colour_of(&path, id), expected as u32, "read for {id}");
    }
    assert_eq!(
        stored_format_version(&path),
        None,
        "the key raises no version"
    );

    // A hidden checkpoint keeps its number; dropping a leaf shifts no other.
    project::delete_checkpoint(&path, &a).unwrap();
    assert!(
        Project::open(&path)
            .unwrap()
            .history()
            .get(&a)
            .unwrap()
            .hidden
    );
    assert_eq!(stored_colour_of(&path, &a), Some(1));
    project::delete_checkpoint(&path, &branch).unwrap();
    assert!(
        Project::open(&path)
            .unwrap()
            .history()
            .get(&branch)
            .is_err()
    );
    for (id, expected) in [(&root, 0), (&a, 1), (&b, 2), (&fused, 3), (&chained[0], 5)] {
        assert_eq!(
            stored_colour_of(&path, id),
            Some(expected),
            "after the delete: {id}"
        );
    }
}

#[test]
fn checkpoint_colour_index_of_a_record_without_the_key_follows_creation_order() {
    let (_dir, path, _) = sealed("colour_legacy");
    let root = head_of(&path);
    let at = Project::open(&path).unwrap().checkpoint().created_at;
    let a = project::append_ops_at(&path, vec![stroke(1, 1, 1)], None, at + 10)
        .unwrap()
        .checkpoint;
    let b = project::append_ops_at(&path, vec![stroke(2, 2, 2)], None, at + 20)
        .unwrap()
        .checkpoint;

    let stripped = path.with_file_name("stripped.ffp");
    let index = Index::read(&path).unwrap();
    let mut writer = ContainerWriter::create(&stripped).unwrap();
    for name in index.names() {
        let mut bytes = index.read_entry(&path, name).unwrap();
        if name.starts_with("checkpoints/") {
            let mut record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            record.as_object_mut().unwrap().remove("colour_index");
            bytes = serde_json::to_vec(&record).unwrap();
        }
        writer.put_bytes(name, &bytes).unwrap();
    }
    writer.finish().unwrap();

    for id in [&root, &a, &b] {
        assert_eq!(stored_colour_of(&stripped, id), None);
    }
    assert_eq!(
        [&root, &a, &b].map(|id| colour_of(&stripped, id)),
        [0, 1, 2]
    );
    assert_eq!(stored_format_version(&stripped), None);

    // The next write continues above the derived numbers.
    let c = project::append_ops_at(&stripped, vec![stroke(0, 5, 5)], None, at + 30)
        .unwrap()
        .checkpoint;
    assert_eq!(stored_colour_of(&stripped, &c), Some(3));
}

#[test]
fn frame_strokes_keep_the_old_format() {
    let paths = saved_by_every_entry_point("frame_only", |_| vec![stroke(2, 7, 7)]);
    for path in &paths {
        assert_eq!(stored_format_version(path), None, "{}", path.display());
        let strokes = stored_strokes(path);
        assert!(!strokes.is_empty());
        for stored in strokes {
            assert!(stored.get("source_id").is_some(), "{stored}");
            assert!(stored.get("source").is_none(), "{stored}");
        }
    }
}

// ---------------------------------------------------------------------
// `pixel_grid`: whether two checkpoints share one pixel grid.
// ---------------------------------------------------------------------

use focusforge_core::align::{Registration, Transform};
use focusforge_core::project::{GridMismatch, PixelGrid, ProjectError};

/// Appends a re-fuse of `sources` recorded with `registration`.
fn append_refuse(
    dir: &Path,
    project_path: &Path,
    sources: &[PathBuf],
    registration: Registration,
    variant: u16,
) -> String {
    let staging = dir.join(format!("refuse-{variant}"));
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging, variant, variant);
    let values = params();
    let mut request = append_request(project_path, &composite, &zmap, sources, &values);
    request.registration = registration;
    project::append(request)
        .expect("append must succeed")
        .checkpoint
}

#[test]
fn pixel_grid_checkpoints_on_one_fuse_are_always_compatible() {
    let t = branched("pixel_grid_same_fuse");
    let project = Project::open(&t.path).unwrap();
    let ids = [&t.root, &t.a, &t.b, &t.c];
    for edited in ids {
        for candidate in ids {
            assert_eq!(
                project.grid_mismatch(edited, candidate).unwrap(),
                None,
                "{edited} vs {candidate}"
            );
        }
    }
}

#[test]
fn pixel_grid_reasons_survive_a_reopen_from_disk() {
    let (dir, path, sources) = sealed("pixel_grid_reasons");
    let root = head_of(&path);
    let crop = Registration {
        crop: Some(Rect {
            x: 4,
            y: 2,
            width: 32,
            height: 24,
        }),
        ..Registration::default()
    };
    let shifted = Registration {
        transforms: vec![
            Transform::identity(),
            Transform::translation(0.25, 0.0),
            Transform::identity(),
        ],
        ..Registration::default()
    };
    let other_dir = dir.join("other-content");
    std::fs::create_dir_all(&other_dir).unwrap();
    let other_sources = write_sources_seeded(&other_dir, 3, 1);

    let cropped = append_refuse(&dir, &path, &sources, crop.clone(), 1);
    let aligned = append_refuse(&dir, &path, &sources, shifted.clone(), 2);
    let other = append_refuse(&dir, &path, &other_sources, Registration::default(), 3);
    let both = append_refuse(&dir, &path, &other_sources, crop, 4);
    let identities = Registration {
        transforms: vec![Transform::identity(); 3],
        ..Registration::default()
    };
    let explicit = append_refuse(&dir, &path, &sources, identities, 5);

    let project = Project::open(&path).unwrap();
    let judge = |edited: &str, candidate: &str| project.grid_mismatch(edited, candidate).unwrap();
    for (candidate, want) in [
        (&cropped, Some(GridMismatch::Crop)),
        (&aligned, Some(GridMismatch::Alignment)),
        (&other, Some(GridMismatch::SourceFrames)),
        (&both, Some(GridMismatch::SourceFrames)),
        (&explicit, None),
    ] {
        assert_eq!(judge(&root, candidate), want, "root vs {candidate}");
        assert_eq!(judge(candidate, &root), want, "{candidate} vs root");
    }
}

#[test]
fn pixel_grid_unknown_candidate_is_an_error_not_a_verdict() {
    let t = branched("pixel_grid_unknown");
    let project = Project::open(&t.path).unwrap();
    assert!(matches!(
        project.grid_mismatch(&t.a, "no-such-checkpoint"),
        Err(ProjectError::UnknownCheckpoint { .. })
    ));
    assert!(matches!(
        project.grid_mismatch("no-such-checkpoint", &t.a),
        Err(ProjectError::UnknownCheckpoint { .. })
    ));
}

#[test]
fn pixel_grid_held_fuse_matches_saved_by_resolved_path() {
    let (dir, path, sources) = sealed("pixel_grid_held");
    let root = head_of(&path);
    let project = Project::open(&path).unwrap();
    let fuse = project.fuse_state(&root).unwrap();
    let saved = project.pixel_grid(&fuse);

    let same_files = held_checkpoint(&dir, &sources, 7, 1_000);
    assert_eq!(PixelGrid::held(&same_files).mismatch(&saved), None);
    assert_eq!(saved.mismatch(&PixelGrid::held(&same_files)), None);

    let copy_dir = dir.join("copies");
    std::fs::create_dir_all(&copy_dir).unwrap();
    let copies: Vec<PathBuf> = sources
        .iter()
        .map(|s| {
            let to = copy_dir.join(s.file_name().unwrap());
            std::fs::copy(s, &to).unwrap();
            to
        })
        .collect();
    let copied = held_checkpoint(&dir, &copies, 8, 1_000);
    assert_eq!(
        PixelGrid::held(&copied).mismatch(&saved),
        Some(GridMismatch::SourceFrames)
    );
}

// ---------------------------------------------------------------------
// Unsaved checkpoints as stroke sources: a stroke names entry `j` of the
// list as `held_key(j)`, renders from it before the save and is written
// with the id the entry gets.
// ---------------------------------------------------------------------

use focusforge_core::io::TiffStack;
use focusforge_core::project::export::{ExportOptions, MetadataOptions};
use focusforge_core::project::{
    CheckpointImage, PendingState, entry_colour_index, held_key, stroke_chain,
};

/// A 100 % normal stroke painting from entry `j` of the unsaved list, from
/// `(x, y)` down and right.
fn held_stroke(j: usize, x: i32, y: i32) -> Op {
    Op::BrushStroke {
        source: StrokeSource::Checkpoint(held_key(j)),
        path: vec![
            Point { x, y },
            Point {
                x: x + 11,
                y: y + 5,
            },
        ],
        radius: 5.0,
        softness: 0.0,
        mode: BrushMode::Normal,
        opacity: 1.0,
    }
}

/// Every sample of `image`, rendered against `project` when it needs one.
fn render_image(image: &CheckpointImage, project: Option<&mut Project>) -> Vec<u16> {
    let mut samples = vec![0u16; (W * H * SPP) as usize];
    image
        .render(project, image.full(), &mut |band, rect| {
            let row = rect.width as usize * SPP as usize;
            for (r, line) in band.chunks_exact(row).enumerate() {
                let at = ((rect.y as usize + r) * W as usize + rect.x as usize) * SPP as usize;
                samples[at..at + row].copy_from_slice(line);
            }
            Ok(())
        })
        .unwrap();
    samples
}

fn read_tiff(path: &Path) -> Vec<u16> {
    let stack = TiffStack::open_files(&[path.to_path_buf()]).unwrap();
    let mut samples = vec![0u16; (W * H * SPP) as usize];
    stack.read_tile_into(0, full(W, H), &mut samples).unwrap();
    samples
}

fn pixel(samples: &[u16], x: u32, y: u32) -> &[u16] {
    let at = (y * W + x) as usize * SPP as usize;
    &samples[at..at + SPP as usize]
}

/// The composite of the fuse entry `i` of `entries` is recorded on: the
/// nearest fused entry up its parents, else `saved_base`.
fn base_composite(entries: &[project::InMemoryCheckpoint], i: usize, saved_base: &Path) -> PathBuf {
    let mut at = i;
    loop {
        match (&entries[at].kind, &entries[at].parent) {
            (project::InMemoryKind::Fused(held), _) => return held.composite.clone(),
            (_, Some(ParentRef::Entry(j))) => at = *j,
            _ => return saved_base.to_path_buf(),
        }
    }
}

fn pending_state(
    composite: PathBuf,
    sources: &[PathBuf],
    ops: Vec<Op>,
    held: Vec<project::InMemoryCheckpoint>,
) -> PendingState {
    PendingState {
        composite,
        ops,
        sources: sources.to_vec(),
        registration: focusforge_core::align::Registration::default(),
        reference: 0,
        description: "staged composite".to_string(),
        held,
    }
}

/// Entry `i` of `entries` as the unsaved state renders it: a held fuse's
/// composite, or its taken ops over the fuse they are recorded on. `saved_base`
/// is the composite of the saved checkpoint an entry hangs from.
fn entry_render(
    entries: &[project::InMemoryCheckpoint],
    i: usize,
    sources: &[PathBuf],
    saved_base: &Path,
    project: Option<&mut Project>,
) -> Vec<u16> {
    let image = match &entries[i].kind {
        project::InMemoryKind::Fused(held) => CheckpointImage::prepare_held(held).unwrap(),
        project::InMemoryKind::Ops { .. } => {
            let state = pending_state(
                base_composite(entries, i, saved_base),
                sources,
                stroke_chain(entries, i).unwrap(),
                entries[..i].to_vec(),
            );
            CheckpointImage::prepare_pending(&state, project.as_deref()).unwrap()
        }
    };
    render_image(&image, project)
}

/// The unsaved list the render tests paint from, on a project saved as
/// `root`: a held fuse, ops over it, and ops over the saved root whose second
/// stroke paints from the held fuse.
fn source_entries(dir: &Path, sources: &[PathBuf], root: &str) -> Vec<project::InMemoryCheckpoint> {
    vec![
        fused_entry(held_checkpoint(dir, sources, 1, 1_000), Some(saved(root))),
        ops_entry(vec![stroke(1, 20, 12)], 1_100, ParentRef::Entry(0)),
        ops_entry(
            vec![stroke(2, 40, 30), held_stroke(0, 6, 36)],
            1_200,
            saved(root),
        ),
    ]
}

/// The checkpoint and reason of a refused `prepare_pending`.
fn refusal(result: Result<CheckpointImage, project::ProjectError>) -> (String, String) {
    match result {
        Err(project::ProjectError::CheckpointSourceRefused {
            checkpoint, reason, ..
        }) => (checkpoint, reason),
        Err(other) => panic!("a different error: {other}"),
        Ok(_) => panic!("the render was not refused"),
    }
}

#[test]
fn in_memory_source_renders() {
    let (dir, path, sources) = sealed("in_memory_source_renders");
    let root = head_of(&path);
    let entries = source_entries(&dir, &sources, &root);
    let staging = dir.join("pending-base");
    std::fs::create_dir_all(&staging).unwrap();
    let (base, _) = stage_planes(&staging, 0, 0);
    let mut project = Project::open(&path).unwrap();

    // One stroke per entry, each in its own part of the image, the last
    // reaching off the canvas.
    let strokes = [
        held_stroke(0, 4, 4),
        held_stroke(1, 24, 20),
        held_stroke(2, 46, 36),
        held_stroke(0, -3, -2),
    ];
    let state = pending_state(base.clone(), &sources, strokes.to_vec(), entries.clone());
    let image = CheckpointImage::prepare_pending(&state, Some(&project)).unwrap();
    let untouched = composite_samples(0);
    let view = render_image(&image, Some(&mut project));

    let expected: Vec<Vec<u16>> = (0..3)
        .map(|i| entry_render(&entries, i, &sources, &base, Some(&mut project)))
        .collect();
    assert_eq!(
        expected[0],
        composite_samples(1),
        "a held fuse is its composite"
    );
    for (entry, points) in [
        (0, [(4, 4), (15, 9)]),
        (1, [(24, 20), (35, 25)]),
        (2, [(46, 36), (57, 41)]),
        (0, [(0, 0), (8, 3)]),
    ] {
        for (x, y) in points {
            assert_eq!(
                pixel(&view, x, y),
                pixel(&expected[entry], x, y),
                "entry {entry} reproduced at ({x}, {y})"
            );
        }
    }
    for (entry, x, y) in [(1, 24, 20), (2, 46, 36)] {
        assert_ne!(
            pixel(&expected[entry], x, y),
            pixel(&untouched, x, y),
            "entry {entry} differs from the base at ({x}, {y})"
        );
    }
    for (x, y) in [(60, 2), (30, 44), (62, 22)] {
        assert_eq!(
            pixel(&view, x, y),
            pixel(&untouched, x, y),
            "({x}, {y}) is outside every footprint"
        );
    }

    // The pending export is the render, and a source budget below one stroke
    // area changes no sample.
    let out = dir.join("pending.tif");
    project::export_pending(
        &state,
        Some(&mut project),
        &out,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .unwrap();
    assert_eq!(read_tiff(&out), view, "export equals render");
    let squeezed = CheckpointImage::prepare_pending(&state, Some(&project))
        .unwrap()
        .with_source_bytes(1);
    assert_eq!(render_image(&squeezed, Some(&mut project)), view, "budget");

    // A never-saved list renders with no project.
    let never = vec![
        fused_entry(held_checkpoint(&dir, &sources, 2, 2_000), None),
        ops_entry(vec![stroke(1, 20, 12)], 2_100, ParentRef::Entry(0)),
    ];
    let state = pending_state(
        base.clone(),
        &sources,
        vec![held_stroke(0, 4, 4), held_stroke(1, 30, 20)],
        never.clone(),
    );
    let image = CheckpointImage::prepare_pending(&state, None).unwrap();
    let view = render_image(&image, None);
    let first = entry_render(&never, 0, &sources, &base, None);
    let second = entry_render(&never, 1, &sources, &base, None);
    assert_eq!(pixel(&view, 4, 4), pixel(&first, 4, 4));
    assert_eq!(pixel(&view, 30, 20), pixel(&second, 30, 20));

    // Refusals: past the list, and an entry painting from itself or later.
    let past = pending_state(
        base.clone(),
        &sources,
        vec![held_stroke(3, 4, 4)],
        entries.clone(),
    );
    assert_eq!(
        refusal(CheckpointImage::prepare_pending(&past, Some(&project))),
        (held_key(3), "is not in this project".to_string())
    );
    for (name, bad) in [("itself", 1), ("a later entry", 2)] {
        let list = vec![
            fused_entry(
                held_checkpoint(&dir, &sources, 3, 3_000),
                Some(saved(&root)),
            ),
            ops_entry(vec![held_stroke(bad, 4, 4)], 3_100, ParentRef::Entry(0)),
            ops_entry(vec![stroke(1, 2, 2)], 3_200, ParentRef::Entry(1)),
        ];
        let state = pending_state(base.clone(), &sources, vec![held_stroke(1, 20, 20)], list);
        assert_eq!(
            refusal(CheckpointImage::prepare_pending(&state, Some(&project))),
            (
                held_key(bad),
                "is not before the checkpoint painting from it".to_string()
            ),
            "an entry painting from {name}"
        );
    }
}

/// The checkpoint ids the stored strokes of `path` paint from, sorted.
fn stored_sources(path: &Path) -> Vec<String> {
    let mut ids: Vec<String> = stored_strokes(path)
        .iter()
        .filter_map(|stroke| stroke["source"]["checkpoint"].as_str().map(str::to_string))
        .collect();
    ids.sort();
    ids
}

#[test]
fn in_memory_source_rewritten_on_save() {
    for onto_existing in [true, false] {
        let (dir, path, sources) = sealed(&format!("in_memory_source_rewritten_{onto_existing}"));
        let root = head_of(&path);
        let staging = dir.join("pending-base");
        std::fs::create_dir_all(&staging).unwrap();
        let (base, _) = stage_planes(&staging, 0, 0);
        let fuse_parent = onto_existing.then(|| saved(&root));
        // e1 paints from its parent e0; e2 paints from e0 and from e1, which
        // is never its parent.
        let entries = vec![
            fused_entry(held_checkpoint(&dir, &sources, 1, 1_000), fuse_parent),
            ops_entry(
                vec![stroke(1, 20, 12), held_stroke(0, 6, 6)],
                1_100,
                ParentRef::Entry(0),
            ),
            ops_entry(
                vec![held_stroke(0, 40, 30), held_stroke(1, 6, 36)],
                1_200,
                if onto_existing {
                    saved(&root)
                } else {
                    ParentRef::Entry(0)
                },
            ),
        ];
        let target = if onto_existing {
            path.clone()
        } else {
            dir.join(format!("fresh.{}", project::PROJECT_EXTENSION))
        };
        let mut opened = onto_existing.then(|| Project::open(&path).unwrap());
        let before: Vec<Vec<u16>> = (0..entries.len())
            .map(|i| entry_render(&entries, i, &sources, &base, opened.as_mut()))
            .collect();
        drop(opened);
        assert_ne!(before[1], before[0], "the stroke of entry 1 changes it");

        let ids = project::seal_in_memory(&target, &entries, &[], None, onto_existing).unwrap();

        assert_eq!(ids.len(), 3);
        let mut expected = vec![ids[0].clone(), ids[0].clone(), ids[1].clone()];
        expected.sort();
        assert_eq!(
            stored_sources(&target),
            expected,
            "onto_existing {onto_existing}"
        );
        for stroke in stored_strokes(&target) {
            assert!(
                !stroke.to_string().contains("held:"),
                "an entry index reached the file: {stroke}"
            );
        }
        let mut reopened = Project::open(&target).unwrap();
        for (i, id) in ids.iter().enumerate() {
            let image = CheckpointImage::prepare(&reopened, id).unwrap();
            assert_eq!(
                render_image(&image, Some(&mut reopened)),
                before[i],
                "entry {i} renders as it did unsaved, onto_existing {onto_existing}"
            );
        }
    }
}

#[test]
fn in_memory_source_refused() {
    for onto_existing in [true, false] {
        let (dir, path, sources) = sealed(&format!("in_memory_source_refused_{onto_existing}"));
        let root = head_of(&path);
        let target = if onto_existing {
            path.clone()
        } else {
            dir.join(format!("fresh.{}", project::PROJECT_EXTENSION))
        };
        let before = onto_existing.then(|| std::fs::read(&target).unwrap());
        let first = || {
            fused_entry(
                held_checkpoint(&dir, &sources, 1, 1_000),
                onto_existing.then(|| saved(&root)),
            )
        };
        let refused = [
            (
                "out of range at op 0",
                vec![
                    first(),
                    ops_entry(vec![held_stroke(5, 6, 6)], 1_100, ParentRef::Entry(0)),
                ],
                "checkpoint 1 paints from checkpoint 5, which is not before it",
            ),
            (
                "itself at op 0",
                vec![
                    first(),
                    ops_entry(vec![held_stroke(1, 6, 6)], 1_100, ParentRef::Entry(0)),
                ],
                "checkpoint 1 paints from checkpoint 1, which is not before it",
            ),
            (
                "a later entry at op 1",
                vec![
                    first(),
                    ops_entry(
                        vec![stroke(1, 2, 2), held_stroke(2, 6, 6)],
                        1_100,
                        ParentRef::Entry(0),
                    ),
                    ops_entry(vec![stroke(2, 9, 9)], 1_200, ParentRef::Entry(1)),
                ],
                "checkpoint 1 paints from checkpoint 2, which is not before it",
            ),
            (
                "the entry itself at op 1 of a later entry",
                vec![
                    first(),
                    ops_entry(vec![stroke(1, 2, 2)], 1_100, ParentRef::Entry(0)),
                    ops_entry(
                        vec![stroke(2, 9, 9), held_stroke(2, 6, 6)],
                        1_200,
                        ParentRef::Entry(1),
                    ),
                ],
                "checkpoint 2 paints from checkpoint 2, which is not before it",
            ),
        ];
        for (what, entries, message) in refused {
            let err = project::seal_in_memory(&target, &entries, &[], None, onto_existing)
                .expect_err(what);
            assert!(
                err.to_string().contains(message),
                "{what}: `{err}` does not say `{message}`"
            );
            match &before {
                Some(bytes) => assert_eq!(&std::fs::read(&target).unwrap(), bytes, "{what}"),
                None => assert!(!target.exists(), "{what} created the project"),
            }
            assert!(leftover_temporaries(&dir).is_empty(), "{what}");
        }
    }
}

fn gone_stroke() -> Op {
    Op::BrushStroke {
        source: StrokeSource::Checkpoint(project::GONE_KEY.to_string()),
        path: vec![Point { x: 4, y: 4 }],
        radius: 5.0,
        softness: 0.0,
        mode: BrushMode::Normal,
        opacity: 1.0,
    }
}

#[test]
fn gone_source_names_no_checkpoint() {
    let (dir, path, sources) = sealed("gone_source_names_no_checkpoint");
    let root = head_of(&path);
    let staging = dir.join("pending-base");
    std::fs::create_dir_all(&staging).unwrap();
    let (base, _) = stage_planes(&staging, 0, 0);
    let project = Project::open(&path).unwrap();
    assert_eq!(project::held_index(project::GONE_KEY), None);
    let entries = vec![fused_entry(
        held_checkpoint(&dir, &sources, 1, 1_000),
        Some(saved(&root)),
    )];

    // The unsaved view refuses it, however many entries it holds.
    let state = pending_state(base, &sources, vec![gone_stroke()], entries.clone());
    assert_eq!(
        refusal(CheckpointImage::prepare_pending(&state, Some(&project))),
        (
            project::GONE_KEY.to_string(),
            "is not in this project".to_string()
        )
    );

    // A save refuses it and writes nothing.
    drop(project);
    let before = std::fs::read(&path).unwrap();
    let list = vec![
        entries[0].clone(),
        ops_entry(vec![gone_stroke()], 1_100, ParentRef::Entry(0)),
    ];
    let err = project::seal_in_memory(&path, &list, &[], None, true)
        .expect_err("a stroke painting from a checkpoint that was never written");
    assert!(err.to_string().contains(project::GONE_KEY), "{err}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(leftover_temporaries(&dir).is_empty());
}

#[test]
fn in_memory_source_kept() {
    let (dir, path, sources) = sealed("in_memory_source_kept");
    let root = head_of(&path);
    let fuse = || {
        fused_entry(
            held_checkpoint(&dir, &sources, 1, 1_000),
            Some(saved(&root)),
        )
    };
    let used = vec![
        fuse(),
        ops_entry(vec![held_stroke(0, 6, 6)], 1_100, saved(&root)),
    ];
    let unused = vec![
        fuse(),
        ops_entry(vec![stroke(1, 6, 6)], 1_100, saved(&root)),
    ];
    let source_deleted = [ParentRef::Entry(0)];
    assert_eq!(
        project::kept_entries(&used, &source_deleted, None),
        [true, true],
        "a deleted entry a written stroke paints from is kept"
    );
    assert_eq!(
        project::kept_entries(&unused, &source_deleted, None),
        [false, true],
        "unused by any entry, it is dropped"
    );
    assert_eq!(
        project::kept_entries(&used, &[ParentRef::Entry(0), ParentRef::Entry(1)], None),
        [false, false],
        "an unwritten entry's stroke keeps nothing"
    );

    let ids = project::seal_in_memory(&path, &used, &source_deleted, None, true).unwrap();
    assert_eq!(ids.len(), 2);
    assert!(is_hidden(&path, &ids[0]), "written, then hidden");
    assert_eq!(stored_sources(&path), [ids[0].clone()]);
    let mut project = Project::open(&path).unwrap();
    let image = CheckpointImage::prepare(&project, &ids[1]).unwrap();
    let samples = render_image(&image, Some(&mut project));
    assert_eq!(pixel(&samples, 6, 6), pixel(&composite_samples(1), 6, 6));
}

#[test]
fn in_memory_entries_keep_their_colour() {
    for onto_existing in [true, false] {
        let (dir, path, sources) = sealed(&format!("in_memory_entries_colour_{onto_existing}"));
        let target = if onto_existing {
            path.clone()
        } else {
            dir.join(format!("fresh.{}", project::PROJECT_EXTENSION))
        };
        let mut first_parent = None;
        if onto_existing {
            let at = Project::open(&path).unwrap().checkpoint().created_at;
            project::append_ops_at(&path, vec![stroke(1, 1, 1)], None, at + 10).unwrap();
            first_parent = Some(saved(&head_of(&path)));
        }
        let base = if onto_existing {
            Project::open(&path).unwrap().history().next_colour_index()
        } else {
            0
        };
        // Entry 1 is deleted and never written; the ones after it keep their numbers.
        let entries = vec![
            fused_entry(held_checkpoint(&dir, &sources, 1, 5_000), first_parent),
            ops_entry(vec![stroke(1, 2, 2)], 5_100, ParentRef::Entry(0)),
            ops_entry(vec![stroke(2, 3, 3)], 5_200, ParentRef::Entry(0)),
            fused_entry(
                held_checkpoint(&dir, &sources, 2, 5_300),
                Some(ParentRef::Entry(2)),
            ),
        ];
        let ids = project::seal_in_memory(
            &target,
            &entries,
            &[ParentRef::Entry(1)],
            None,
            onto_existing,
        )
        .unwrap();
        assert_eq!(ids.len(), 3, "entry 1 is not written");
        for (id, entry) in ids.iter().zip([0, 2, 3]) {
            let number = entry_colour_index(base, entry);
            assert_eq!(number, base + entry as u32);
            assert_eq!(
                stored_colour_of(&target, id),
                Some(u64::from(number)),
                "stored, entry {entry}, onto_existing {onto_existing}"
            );
            assert_eq!(colour_of(&target, id), number, "read, entry {entry}");
        }
    }
}
