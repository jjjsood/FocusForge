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
    self, AppendRequest, Metrics, Op, ParentRef, Point, Project, SealRequest,
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
    Op::BrushStroke {
        source_id,
        path: vec![
            Point { x, y },
            Point {
                x: x + 12,
                y: y + 8,
            },
        ],
        radius: 5.0,
        softness: 0.0,
    }
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
