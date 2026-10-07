// SPDX-License-Identifier: GPL-3.0-or-later

//! The checkpoint store: branching onto an explicit parent, traversal over a
//! branched history, both delete cases, the shared-blob retention rule, the
//! two link-integrity refusals, the `parent` shape and the per-fuse record
//! branching and repointing read.

mod common;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use focusforge_core::align::Registration;
use focusforge_core::io::{Rect, TiffSpec, TiffWriter};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::project::container::{ContainerWriter, Index};
use focusforge_core::project::export::{ExportOptions, MetadataOptions};
use focusforge_core::project::{
    self, AppendRequest, Metrics, Op, Point, Project, ProjectError, SealRequest,
};
use focusforge_core::zmap::{ZMapInfo, ZMapReader, ZMapWriter};

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

fn composite_samples(w: u32, h: u32, salt: u16) -> Vec<u16> {
    (0..(w as usize * h as usize * SPP as usize))
        .map(|i| {
            let x = (i / SPP as usize) % w.max(1) as usize;
            let channel = i % SPP as usize;
            ((x * 65_535 / w.max(1) as usize) as u16)
                .wrapping_add(channel as u16 * 97)
                .wrapping_add(salt)
        })
        .collect()
}

fn zmap_noisy(w: u32, h: u32, frames: u32, salt: u32) -> Vec<u16> {
    (0..w * h)
        .map(|i| {
            let x = i % w;
            let y = i / w;
            let h1 = x
                .wrapping_mul(2_654_435_761)
                .wrapping_add(y.wrapping_mul(40_503))
                .wrapping_add(salt);
            (h1 % frames) as u16
        })
        .collect()
}

fn stage_planes(
    dir: &Path,
    name: &str,
    w: u32,
    h: u32,
    frames: u32,
    salt: u32,
) -> (PathBuf, PathBuf) {
    let composite = dir.join(format!("{name}-composite.tif"));
    let zmap = dir.join(format!("{name}-zmap.tif"));
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
        .write_tile(full(w, h), &composite_samples(w, h, salt as u16))
        .unwrap();
    writer.finish().unwrap();

    let mut map = ZMapWriter::create(&zmap, ZMapInfo::new(w, h, frames).unwrap()).unwrap();
    map.write_tile(full(w, h), &zmap_noisy(w, h, frames, salt))
        .unwrap();
    map.finish().unwrap();
    (composite, zmap)
}

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
                .map(|j| ((j + i * 13) % 65_536) as u16)
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
        registration: Registration::default(),
        composite_description: "re-fused composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    }
}

/// One sealed project in a fresh directory over a synthetic, noisy `w x h`
/// Z-Map with `frames` source frames.
fn sealed(name: &str, w: u32, h: u32, frames: u32) -> (PathBuf, PathBuf) {
    let dir = tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging, "root", w, h, frames, 0);
    let sources = write_sources(&dir, 3, w, h);
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

fn read_zmap_pixels(path: &Path) -> Vec<u16> {
    let reader = ZMapReader::open(path).unwrap();
    let (w, h) = reader.dimensions();
    let mut buf = vec![0u16; w as usize * h as usize];
    reader.read_tile_into(full(w, h), &mut buf).unwrap();
    buf
}

fn read_entry_bytes(path: &Path, name: &str) -> Vec<u8> {
    Index::read(path).unwrap().read_entry(path, name).unwrap()
}

fn read_whole_file(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

fn archive_entry_names(path: &Path) -> HashSet<String> {
    Index::read(path)
        .unwrap()
        .names()
        .map(str::to_string)
        .collect()
}

/// `result.unwrap_err()`, without requiring the `Ok` side to implement
/// `Debug` -- [`Project`] and [`project::Deleted`] deliberately do not.
fn err<T>(result: Result<T, ProjectError>) -> ProjectError {
    match result {
        Err(e) => e,
        Ok(_) => panic!("expected an error"),
    }
}

fn read_checkpoint_json(path: &Path, id: &str) -> serde_json::Value {
    let bytes = read_entry_bytes(path, &format!("checkpoints/{id}.json"));
    serde_json::from_slice(&bytes).unwrap()
}

/// Copies `src` to `dst`, replacing the named entries in `replace` and
/// appending the entries in `add` -- a raw rewrite of a real container, so a
/// test can inject a malformed link the production writer would never
/// produce.
fn mutate_container(
    src: &Path,
    dst: &Path,
    replace: &[(String, Vec<u8>)],
    add: &[(String, Vec<u8>)],
) {
    let index = Index::read(src).unwrap();
    let mut writer = ContainerWriter::create(dst).unwrap();
    for name in index.names() {
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

#[test]
fn branching_adds_a_child_and_rewrites_nothing() {
    let (_dir, project_path) = sealed("checkpoint_store_branch_add", 64, 48, 8);
    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();

    let sealed_a = project::append_ops_at(
        &project_path,
        vec![stroke(2, 10, 10, 1.0)],
        None,
        1_700_000_000,
    )
    .expect("append onto the head must succeed");

    let root_json_before = read_entry_bytes(&project_path, &format!("checkpoints/{root_id}.json"));
    let a_json_before = read_entry_bytes(
        &project_path,
        &format!("checkpoints/{}.json", sealed_a.checkpoint),
    );

    let sealed_b = project::append_ops_onto(
        &project_path,
        &root_id,
        vec![stroke(5, 50, 35, 1.0)],
        None,
        1_700_000_100,
    )
    .expect("branching onto the root must succeed");

    let root_json_after = read_entry_bytes(&project_path, &format!("checkpoints/{root_id}.json"));
    let a_json_after = read_entry_bytes(
        &project_path,
        &format!("checkpoints/{}.json", sealed_a.checkpoint),
    );
    assert_eq!(
        root_json_before, root_json_after,
        "root entry must be byte-unchanged"
    );
    assert_eq!(
        a_json_before, a_json_after,
        "sibling entry must be byte-unchanged"
    );

    let project = Project::open(&project_path).unwrap();
    let b = project.history().get(&sealed_b.checkpoint).unwrap().clone();
    assert_eq!(b.parent.as_deref(), Some(root_id.as_str()));

    let root_blob = project.history().get(&root_id).unwrap().zmap_blob.clone();
    assert_eq!(
        b.zmap_blob, root_blob,
        "an op checkpoint names the fused map"
    );
    assert_eq!(
        b.ops,
        vec![stroke(5, 50, 35, 1.0)],
        "B holds its own stroke only"
    );
}

#[test]
fn traversal_over_a_branched_history() {
    let (_dir, project_path) = sealed("checkpoint_store_traversal", 48, 32, 6);
    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();

    let a = project::append_ops_at(
        &project_path,
        vec![stroke(1, 5, 5, 1.0)],
        None,
        1_700_000_000,
    )
    .unwrap()
    .checkpoint;
    let a2 = project::append_ops_at(
        &project_path,
        vec![stroke(2, 6, 6, 1.0)],
        None,
        1_700_000_010,
    )
    .unwrap()
    .checkpoint;
    let b = project::append_ops_onto(
        &project_path,
        &root_id,
        vec![stroke(3, 20, 20, 1.0)],
        None,
        1_700_000_020,
    )
    .unwrap()
    .checkpoint;

    let project = Project::open(&project_path).unwrap();
    let history = project.history();

    assert!(matches!(
        history.get("unknown"),
        Err(ProjectError::UnknownCheckpoint { id, .. }) if id == "unknown"
    ));

    let all: HashSet<String> = history.all().into_iter().map(|c| c.id.clone()).collect();
    assert_eq!(
        all,
        HashSet::from([root_id.clone(), a.clone(), a2.clone(), b.clone()])
    );

    let ancestors: HashSet<String> = history
        .ancestors(&a2)
        .unwrap()
        .into_iter()
        .map(|c| c.id.clone())
        .collect();
    assert_eq!(
        ancestors,
        HashSet::from([a2.clone(), a.clone(), root_id.clone()])
    );

    let children: HashSet<String> = history
        .children(&root_id)
        .into_iter()
        .map(|c| c.id.clone())
        .collect();
    assert_eq!(children, HashSet::from([a.clone(), b.clone()]));

    let descendants: HashSet<String> = history
        .descendants(&root_id)
        .into_iter()
        .map(|c| c.id.clone())
        .collect();
    assert_eq!(descendants, HashSet::from([a, a2, b]));
}

#[test]
fn leaf_delete_touches_nothing_else() {
    let (dir, project_path) = sealed("checkpoint_store_leaf_delete", 48, 32, 6);
    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();

    let mut project = Project::open(&project_path).unwrap();
    let root = project.checkpoint().clone();
    let before = dir.join("root-before.tif");
    project::materialize_zmap(&mut project, &root, &before).unwrap();
    let root_pixels_before = read_zmap_pixels(&before);

    let leaf = project::append_ops_at(
        &project_path,
        vec![stroke(1, 5, 5, 1.0)],
        None,
        1_700_000_000,
    )
    .unwrap();

    let deleted = project::delete_checkpoint(&project_path, &leaf.checkpoint).unwrap();
    assert_eq!(deleted.removed, vec![leaf.checkpoint.clone()]);
    assert_eq!(deleted.head, root_id);

    let names = archive_entry_names(&project_path);
    assert!(!names.contains(&format!("checkpoints/{}.json", leaf.checkpoint)));
    assert!(
        names.contains(&format!("blobs/{}.zst", leaf.zmap.hash)),
        "the root's blob stays indexed"
    );

    let mut project = Project::open(&project_path).unwrap();
    let root_again = project.history().get(&root_id).unwrap().clone();
    let after = dir.join("root-after.tif");
    project::materialize_zmap(&mut project, &root_again, &after).unwrap();
    assert_eq!(root_pixels_before, read_zmap_pixels(&after));
}

#[test]
fn delete_reads_no_source_frame() {
    let (_dir, project_path) = sealed("checkpoint_store_delete_no_sources", 48, 32, 6);
    let leaf = project::append_ops_at(
        &project_path,
        vec![stroke(1, 5, 5, 1.0)],
        None,
        1_700_000_000,
    )
    .unwrap();

    // The frames named in the manifest live under the project's own
    // directory; deleting them proves the delete path never opens one.
    let project = Project::open(&project_path).unwrap();
    for source in project.manifest().sources.clone() {
        let path = project.source_path(&source);
        let _ = std::fs::remove_file(path);
    }

    let composite_hash = project.manifest().composite.blob.clone();
    let composite_before = read_entry_bytes(&project_path, &format!("blobs/{composite_hash}.zst"));
    drop(project);

    project::delete_checkpoint(&project_path, &leaf.checkpoint)
        .expect("delete must not need a source frame");

    let composite_after = read_entry_bytes(&project_path, &format!("blobs/{composite_hash}.zst"));
    assert_eq!(composite_before, composite_after);
}

#[test]
fn dangling_parent_is_an_error() {
    let (dir, project_path) = sealed("checkpoint_store_dangling_parent", 32, 24, 4);
    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();
    let mut orphan = read_checkpoint_json(&project_path, &root_id);
    orphan["id"] = serde_json::json!("orphan");
    orphan["parent"] = serde_json::json!("ghost");

    let corrupt = dir.join("dangling.ffp");
    mutate_container(
        &project_path,
        &corrupt,
        &[],
        &[(
            "checkpoints/orphan.json".to_string(),
            serde_json::to_vec_pretty(&orphan).unwrap(),
        )],
    );

    let e = err(Project::open(&corrupt));
    assert!(matches!(
        e,
        ProjectError::DanglingParent { checkpoint, parent, .. }
            if checkpoint == "orphan" && parent == "ghost"
    ));
}

#[test]
fn parent_cycle_is_refused() {
    let (dir, project_path) = sealed("checkpoint_store_cycle", 32, 24, 4);
    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();

    // A self-parent.
    let mut self_cycle = read_checkpoint_json(&project_path, &root_id);
    self_cycle["parent"] = serde_json::json!(root_id);
    let self_corrupt = dir.join("self-cycle.ffp");
    mutate_container(
        &project_path,
        &self_corrupt,
        &[(
            format!("checkpoints/{root_id}.json"),
            serde_json::to_vec_pretty(&self_cycle).unwrap(),
        )],
        &[],
    );
    assert!(matches!(
        err(Project::open(&self_corrupt)),
        ProjectError::ParentCycle { .. }
    ));

    // A two-checkpoint cycle: root -> a -> root.
    let a = project::append_ops_at(
        &project_path,
        vec![stroke(1, 5, 5, 1.0)],
        None,
        1_700_000_000,
    )
    .unwrap()
    .checkpoint;
    let mut root_pointing_at_child = read_checkpoint_json(&project_path, &root_id);
    root_pointing_at_child["parent"] = serde_json::json!(a);
    let two_corrupt = dir.join("two-cycle.ffp");
    mutate_container(
        &project_path,
        &two_corrupt,
        &[(
            format!("checkpoints/{root_id}.json"),
            serde_json::to_vec_pretty(&root_pointing_at_child).unwrap(),
        )],
        &[],
    );
    assert!(matches!(
        err(Project::open(&two_corrupt)),
        ProjectError::ParentCycle { .. }
    ));
}

#[test]
fn parent_stays_a_single_optional_link() {
    let (_dir, project_path) = sealed("checkpoint_store_parent_shape", 32, 24, 4);
    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();
    let child = project::append_ops_at(
        &project_path,
        vec![stroke(1, 5, 5, 1.0)],
        None,
        1_700_000_000,
    )
    .unwrap()
    .checkpoint;

    let root_json = read_checkpoint_json(&project_path, &root_id);
    assert!(root_json["parent"].is_null());
    assert!(root_json.get("parents").is_none());

    let child_json = read_checkpoint_json(&project_path, &child);
    assert!(child_json["parent"].is_string());
    assert!(child_json.get("parents").is_none());
}

#[test]
fn single_parentless_checkpoint_is_a_valid_history() {
    let (_dir, project_path) = sealed("checkpoint_store_single", 32, 24, 4);
    let project = Project::open(&project_path).unwrap();
    let root_id = project.checkpoint().id.clone();
    let history = project.history();

    assert_eq!(history.all().len(), 1);
    assert_eq!(
        history
            .ancestors(&root_id)
            .unwrap()
            .into_iter()
            .map(|c| c.id.clone())
            .collect::<Vec<_>>(),
        vec![root_id.clone()]
    );
    assert!(history.children(&root_id).is_empty());
    assert!(history.descendants(&root_id).is_empty());
}

#[test]
fn delete_of_the_head_repoints_to_the_newest_survivor() {
    let (_dir, project_path) = sealed("checkpoint_store_repoint", 48, 32, 6);
    let root = Project::open(&project_path).unwrap();
    let root_created_at = root.checkpoint().created_at;
    let root_fuse_composite = root
        .checkpoint()
        .fuse
        .as_ref()
        .unwrap()
        .composite
        .blob
        .clone();
    drop(root);

    let a = project::append_ops_at(
        &project_path,
        vec![stroke(1, 5, 5, 1.0)],
        None,
        root_created_at + 10,
    )
    .unwrap()
    .checkpoint;
    let b = project::append_ops_at(
        &project_path,
        vec![stroke(2, 6, 6, 1.0)],
        None,
        root_created_at + 20,
    )
    .unwrap()
    .checkpoint;

    let deleted = project::delete_checkpoint(&project_path, &b).unwrap();
    assert_eq!(deleted.head, a);

    let project = Project::open(&project_path).unwrap();
    assert_eq!(project.checkpoint().id, a);
    assert_eq!(project.manifest().checkpoint, a);
    assert_eq!(project.manifest().composite.blob, root_fuse_composite);
}

#[test]
fn append_after_branching_keeps_every_sibling() {
    let (_dir, project_path) = sealed("checkpoint_store_append_keeps_siblings", 48, 32, 6);
    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();

    let a = project::append_ops_at(
        &project_path,
        vec![stroke(1, 5, 5, 1.0)],
        None,
        1_700_000_000,
    )
    .unwrap()
    .checkpoint;
    let b = project::append_ops_onto(
        &project_path,
        &root_id,
        vec![stroke(2, 20, 20, 1.0)],
        None,
        1_700_000_010,
    )
    .unwrap()
    .checkpoint;

    // The head is B (the newer of the two branches): a further append
    // must still leave A's entry in the container.
    let c = project::append_ops_at(
        &project_path,
        vec![stroke(3, 25, 25, 1.0)],
        None,
        1_700_000_020,
    )
    .unwrap()
    .checkpoint;

    let names = archive_entry_names(&project_path);
    for id in [&root_id, &a, &b, &c] {
        assert!(
            names.contains(&format!("checkpoints/{id}.json")),
            "{id} must survive the append"
        );
    }
}

#[test]
fn branching_across_a_refuse_takes_that_fuses_planes() {
    let (dir, project_path) = sealed("checkpoint_store_cross_fuse_branch", 48, 32, 6);
    let project = Project::open(&project_path).unwrap();
    let root_id = project.checkpoint().id.clone();
    let root_fuse = project.fuse_state(&root_id).unwrap();

    let sources: Vec<PathBuf> = project
        .manifest()
        .sources
        .iter()
        .map(|s| project.source_path(s))
        .collect();
    let values = params();
    let staging = dir.join("refuse-staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite2, zmap2) = stage_planes(&staging, "refuse", 48, 32, 6, 97);
    let refuse = project::append_at(
        append_request(&project_path, &composite2, &zmap2, &sources, &values),
        1_700_000_050,
    )
    .unwrap();
    assert_ne!(refuse.composite.hash, root_fuse.composite.blob);

    let branched = project::append_ops_onto(
        &project_path,
        &root_id,
        vec![stroke(1, 5, 5, 1.0)],
        None,
        1_700_000_100,
    )
    .unwrap();

    let branched_project = Project::open(&project_path).unwrap();
    let manifest = branched_project.manifest();
    assert_eq!(manifest.checkpoint, branched.checkpoint);
    assert_eq!(manifest.composite.blob, root_fuse.composite.blob);
    assert_eq!(manifest.align, root_fuse.align);
    assert_eq!(manifest.registration.crop, root_fuse.registration.crop);
    assert_eq!(manifest.sources, root_fuse.sources);
    assert_ne!(manifest.composite.blob, refuse.composite.hash);
}

#[test]
fn delete_of_a_refuse_restores_the_earlier_fuse() {
    let (dir, project_path) = sealed("checkpoint_store_delete_refuse", 48, 32, 6);
    let project = Project::open(&project_path).unwrap();
    let root_id = project.checkpoint().id.clone();
    let root_fuse_composite = project.fuse_state(&root_id).unwrap().composite.blob.clone();
    let sources: Vec<PathBuf> = project
        .manifest()
        .sources
        .iter()
        .map(|s| project.source_path(s))
        .collect();
    drop(project);

    let values = params();
    let staging = dir.join("refuse-staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite2, zmap2) = stage_planes(&staging, "refuse", 48, 32, 6, 71);
    let refuse = project::append_at(
        append_request(&project_path, &composite2, &zmap2, &sources, &values),
        1_700_000_050,
    )
    .unwrap();
    assert_ne!(refuse.composite.hash, root_fuse_composite);

    let deleted = project::delete_checkpoint(&project_path, &refuse.checkpoint).unwrap();
    assert_eq!(deleted.head, root_id);

    let project = Project::open(&project_path).unwrap();
    assert_eq!(project.manifest().composite.blob, root_fuse_composite);
    let names = archive_entry_names(&project_path);
    assert!(!names.contains(&format!("blobs/{}.zst", refuse.composite.hash)));
}

#[test]
fn unrecorded_fuse_is_refused_by_name() {
    let (dir, project_path) = sealed("checkpoint_store_unrecorded_fuse", 48, 32, 6);
    let project = Project::open(&project_path).unwrap();
    let root_id = project.checkpoint().id.clone();
    let sources: Vec<PathBuf> = project
        .manifest()
        .sources
        .iter()
        .map(|s| project.source_path(s))
        .collect();
    drop(project);

    let values = params();
    let staging = dir.join("refuse-staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite2, zmap2) = stage_planes(&staging, "refuse", 48, 32, 6, 43);
    project::append_at(
        append_request(&project_path, &composite2, &zmap2, &sources, &values),
        1_700_000_050,
    )
    .unwrap();

    // Strip the root's own recorded planes, once with a re-fuse above it and
    // once with only an op append above it (the head's own fused ancestor).
    let refused_without_fuse = |name: &str| {
        let mut root_without_fuse = read_checkpoint_json(&project_path, &root_id);
        root_without_fuse.as_object_mut().unwrap().remove("fuse");
        let stripped = dir.join(name);
        mutate_container(
            &project_path,
            &stripped,
            &[(
                format!("checkpoints/{root_id}.json"),
                serde_json::to_vec_pretty(&root_without_fuse).unwrap(),
            )],
            &[],
        );
        let before = read_whole_file(&stripped);
        let e = match Project::open(&stripped) {
            Ok(_) => panic!("an op-less checkpoint without a fuse record must be refused"),
            Err(e) => e,
        };
        assert_eq!(before, read_whole_file(&stripped));
        e
    };
    let after_refuse = refused_without_fuse("stripped-refuse.ffp");
    assert_fuse_record_refusal(&after_refuse, &root_id);

    let (dir2, project_path2) = sealed("checkpoint_store_unrecorded_fuse_head", 48, 32, 6);
    let root_created_at = Project::open(&project_path2)
        .unwrap()
        .checkpoint()
        .created_at;
    append_ops(
        &project_path2,
        vec![stroke(1, 5, 5, 1.0)],
        root_created_at + 10,
    );
    let root2 = root_id_of(&project_path2);
    let mut root_without_fuse = read_checkpoint_json(&project_path2, &root2);
    root_without_fuse.as_object_mut().unwrap().remove("fuse");
    let stripped = dir2.join("stripped-head.ffp");
    mutate_container(
        &project_path2,
        &stripped,
        &[(
            format!("checkpoints/{root2}.json"),
            serde_json::to_vec_pretty(&root_without_fuse).unwrap(),
        )],
        &[],
    );
    let before = read_whole_file(&stripped);
    let e = match Project::open(&stripped) {
        Ok(_) => panic!("the head's fused ancestor without a fuse record must be refused"),
        Err(e) => e,
    };
    assert_fuse_record_refusal(&e, &root2);
    assert_eq!(before, read_whole_file(&stripped));
}

fn assert_fuse_record_refusal(e: &ProjectError, id: &str) {
    match e {
        ProjectError::Container { message, .. } => assert!(
            message.contains(&format!("checkpoints/{id}.json"))
                && message.contains("no fuse record"),
            "the refusal must name the checkpoint file and the missing record: {message}"
        ),
        other => panic!("expected a container error, got {other:?}"),
    }
}

/// The id of the project's oldest checkpoint, the sealed root.
fn root_id_of(path: &Path) -> String {
    let project = Project::open(path).unwrap();
    let chain = project.checkpoint_chain().unwrap();
    chain.last().unwrap().id.clone()
}

/// Every indexed record but the manifest, by name: what a delete must leave
/// alone for each checkpoint it does not touch.
fn entries_but_manifest(path: &Path) -> BTreeMap<String, Vec<u8>> {
    let index = Index::read(path).unwrap();
    index
        .names()
        .filter(|name| *name != "manifest.json")
        .map(|name| (name.to_string(), index.read_entry(path, name).unwrap()))
        .collect()
}

/// A re-fuse written under the head: a fused checkpoint with its own
/// composite and a full Z-Map.
fn refuse_onto_head(dir: &Path, project_path: &Path, salt: u32, created_at: i64) -> String {
    let project = Project::open(project_path).unwrap();
    let sources: Vec<PathBuf> = project
        .manifest()
        .sources
        .iter()
        .map(|s| project.source_path(s))
        .collect();
    drop(project);
    let values = params();
    let staging = dir.join(format!("refuse-staging-{salt}"));
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging, "refuse", 48, 32, sources.len() as u32, salt);
    project::append_at(
        append_request(project_path, &composite, &zmap, &sources, &values),
        created_at,
    )
    .unwrap()
    .checkpoint
}

fn append_ops(project_path: &Path, ops: Vec<Op>, created_at: i64) -> project::SealedProject {
    project::append_ops_at(project_path, ops, None, created_at).unwrap()
}

fn visible_ids(project_path: &Path) -> HashSet<String> {
    let project = Project::open(project_path).unwrap();
    project
        .history()
        .visible()
        .into_iter()
        .map(|c| c.id.clone())
        .collect()
}

#[test]
fn deleting_a_leaf_stops_indexing_it_and_leaves_its_bytes_as_dead_data() {
    let (_dir, project_path) = sealed("checkpoint_store_leaf_shrinks", 48, 32, 6);
    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();
    let leaf = append_ops(&project_path, vec![stroke(1, 5, 5, 1.0)], 1_700_000_000);

    let size_before = read_whole_file(&project_path).len();
    let index_before = Index::read(&project_path).unwrap();
    let entries_before = entries_but_manifest(&project_path);
    let deleted = project::delete_checkpoint(&project_path, &leaf.checkpoint).unwrap();
    assert_eq!(deleted.hidden, leaf.checkpoint);
    assert_eq!(deleted.removed, vec![leaf.checkpoint.clone()]);
    assert_eq!(deleted.head, root_id);

    let project = Project::open(&project_path).unwrap();
    assert!(
        project
            .history()
            .all()
            .iter()
            .all(|c| c.id != leaf.checkpoint)
    );
    let checkpoint_record = format!("checkpoints/{}.json", leaf.checkpoint);
    let names = archive_entry_names(&project_path);
    assert!(!names.contains(&checkpoint_record));
    assert!(read_whole_file(&project_path).len() > size_before);
    let dropped_len = index_before.get(&checkpoint_record).unwrap().1;
    assert!(
        Index::read(&project_path).unwrap().dead_bytes() >= index_before.dead_bytes() + dropped_len
    );

    let mut expected = entries_before;
    expected.remove(&format!("checkpoints/{}.json", leaf.checkpoint));
    assert_eq!(
        expected,
        entries_but_manifest(&project_path),
        "every other entry is byte-identical"
    );
}

#[test]
fn hiding_a_parent_keeps_its_child_byte_identical() {
    let (dir, project_path) = sealed("checkpoint_store_hide_parent", 48, 32, 6);
    let root_created_at = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .created_at;
    let parent = refuse_onto_head(&dir, &project_path, 71, root_created_at + 10);
    let child = append_ops(
        &project_path,
        vec![stroke(2, 6, 6, 1.0)],
        root_created_at + 20,
    );

    let outputs = |tag: &str| {
        let mut project = Project::open(&project_path).unwrap();
        let checkpoint = project.history().get(&child.checkpoint).unwrap().clone();
        let zmap = dir.join(format!("{tag}-zmap.tif"));
        let render = dir.join(format!("{tag}-render.tif"));
        let export = dir.join(format!("{tag}-export.tif"));
        project::materialize_zmap(&mut project, &checkpoint, &zmap).unwrap();
        project::render_checkpoint(&mut project, &child.checkpoint, &render).unwrap();
        project::export_checkpoint(
            &mut project,
            &child.checkpoint,
            &export,
            &ExportOptions::default(),
            &MetadataOptions::default(),
        )
        .unwrap();
        [zmap, render, export].map(|p| std::fs::read(p).unwrap())
    };
    let outputs_before = outputs("before");
    let entries_before = entries_but_manifest(&project_path);

    let deleted = project::delete_checkpoint(&project_path, &parent).unwrap();
    assert_eq!(deleted.hidden, parent);
    assert!(deleted.removed.is_empty());
    assert_eq!(deleted.head, child.checkpoint);

    let project = Project::open(&project_path).unwrap();
    assert!(!visible_ids(&project_path).contains(&parent));
    assert!(project.history().get(&parent).unwrap().hidden);
    drop(project);
    let dead_after_hide = Index::read(&project_path).unwrap().dead_bytes();

    let parent_json = format!("checkpoints/{parent}.json");
    let mut expected = entries_before;
    let original_json = expected.remove(&parent_json).unwrap();
    let mut after = entries_but_manifest(&project_path);
    let rewritten = after.remove(&parent_json).unwrap();
    assert_eq!(
        expected, after,
        "the parent's blobs and every other entry stay byte-identical"
    );
    let mut original: serde_json::Value = serde_json::from_slice(&original_json).unwrap();
    original["hidden"] = true.into();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&rewritten).unwrap(),
        original,
        "only the hidden flag changes"
    );

    assert_eq!(outputs_before, outputs("after"));

    let deleted = project::delete_checkpoint(&project_path, &child.checkpoint).unwrap();
    let removed: HashSet<String> = deleted.removed.into_iter().collect();
    assert_eq!(
        removed,
        HashSet::from([parent.clone(), child.checkpoint.clone()])
    );
    let index = Index::read(&project_path).unwrap();
    assert!(index.dead_bytes() > dead_after_hide);
    for id in &removed {
        assert!(!index.contains(&format!("checkpoints/{id}.json")));
    }
}

#[test]
fn the_last_visible_checkpoint_cannot_be_deleted() {
    let (_dir, project_path) = sealed("checkpoint_store_last_visible", 48, 32, 6);
    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();
    let before = read_whole_file(&project_path);
    let e = err(project::delete_checkpoint(&project_path, &root_id));
    assert!(matches!(
        &e,
        ProjectError::LastVisibleDelete { checkpoint, .. } if *checkpoint == root_id
    ));
    assert!(e.to_string().contains(&root_id));
    assert_eq!(before, read_whole_file(&project_path));

    let child = append_ops(&project_path, vec![stroke(1, 5, 5, 1.0)], 1_700_000_000).checkpoint;
    let deleted = project::delete_checkpoint(&project_path, &root_id).unwrap();
    assert_eq!(deleted.hidden, root_id);
    assert!(deleted.removed.is_empty(), "the child is built on the root");
    assert_eq!(visible_ids(&project_path), HashSet::from([child.clone()]));

    let before = read_whole_file(&project_path);
    let e = err(project::delete_checkpoint(&project_path, &child));
    assert!(matches!(
        e,
        ProjectError::LastVisibleDelete { checkpoint, .. } if checkpoint == child
    ));
    for unknown in [root_id.as_str(), "no-such-checkpoint"] {
        let e = err(project::delete_checkpoint(&project_path, unknown));
        assert!(
            matches!(&e, ProjectError::UnknownCheckpoint { id, .. } if id == unknown),
            "{unknown}: {e}"
        );
    }
    assert_eq!(before, read_whole_file(&project_path));
}

#[test]
fn the_head_moves_to_the_newest_visible_checkpoint() {
    let (dir, project_path) = sealed("checkpoint_store_head_moves", 48, 32, 6);
    let root_created_at = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .created_at;
    let parent = refuse_onto_head(&dir, &project_path, 71, root_created_at + 10);
    let parent_composite = Project::open(&project_path)
        .unwrap()
        .manifest()
        .composite
        .blob
        .clone();
    let child = append_ops(
        &project_path,
        vec![stroke(1, 5, 5, 1.0)],
        root_created_at + 20,
    );
    let head = append_ops(
        &project_path,
        vec![stroke(2, 6, 6, 1.0)],
        root_created_at + 30,
    );

    let manifest_before = read_entry_bytes(&project_path, "manifest.json");
    let deleted = project::delete_checkpoint(&project_path, &parent).unwrap();
    assert_eq!(deleted.head, head.checkpoint, "a visible head stays");
    assert_eq!(
        manifest_before,
        read_entry_bytes(&project_path, "manifest.json"),
        "hiding a non-head leaves the manifest as it was"
    );

    let deleted = project::delete_checkpoint(&project_path, &head.checkpoint).unwrap();
    assert_eq!(deleted.head, child.checkpoint);
    assert_eq!(deleted.removed, vec![head.checkpoint.clone()]);

    let project = Project::open(&project_path).unwrap();
    assert!(project.history().get(&parent).unwrap().hidden);
    let manifest = project.manifest();
    assert_eq!(manifest.checkpoint, child.checkpoint);
    assert_eq!(
        manifest.zmap.blob,
        project.history().get(&child.checkpoint).unwrap().zmap_blob
    );
    assert_eq!(
        manifest.composite.blob, parent_composite,
        "rebuilt from the hidden fused ancestor"
    );
    assert_eq!(project.checkpoint().id, child.checkpoint);
}

#[test]
fn hidden_checkpoints_survive_a_reopen() {
    let (dir, project_path) = sealed("checkpoint_store_hidden_reopen", 48, 32, 6);
    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();
    let root_created_at = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .created_at;
    let parent = refuse_onto_head(&dir, &project_path, 71, root_created_at + 10);
    let child = append_ops(
        &project_path,
        vec![stroke(1, 5, 5, 1.0)],
        root_created_at + 20,
    )
    .checkpoint;
    project::delete_checkpoint(&project_path, &parent).unwrap();

    let project = Project::open(&project_path).unwrap();
    assert_eq!(project.checkpoint().id, child);
    assert_eq!(
        visible_ids(&project_path),
        HashSet::from([root_id.clone(), child.clone()])
    );
    let hidden: Vec<&str> = project
        .history()
        .all()
        .into_iter()
        .filter(|c| c.hidden)
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(hidden, vec![parent.as_str()]);
    assert_eq!(project.history().ancestors(&child).unwrap().len(), 3);

    assert!(
        read_checkpoint_json(&project_path, &parent)["hidden"]
            .as_bool()
            .unwrap()
    );
    for id in [&child, &root_id] {
        assert_eq!(
            read_checkpoint_json(&project_path, id)["hidden"],
            serde_json::json!(false)
        );
    }

    let mut child_without_hidden = read_checkpoint_json(&project_path, &child);
    child_without_hidden
        .as_object_mut()
        .unwrap()
        .remove("hidden");
    let without = dir.join("without-hidden.ffp");
    mutate_container(
        &project_path,
        &without,
        &[(
            format!("checkpoints/{child}.json"),
            serde_json::to_vec_pretty(&child_without_hidden).unwrap(),
        )],
        &[],
    );
    let e = match Project::open(&without) {
        Ok(_) => panic!("a checkpoint without `hidden` must be refused"),
        Err(e) => e,
    };
    assert!(
        e.to_string().contains(&format!("checkpoints/{child}.json")),
        "the refusal must name the checkpoint file: {e}"
    );
}
