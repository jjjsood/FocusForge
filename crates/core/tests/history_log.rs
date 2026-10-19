// SPDX-License-Identifier: GPL-3.0-or-later

//! Sealing a non-empty op list as a checkpoint: reading it back with every
//! op and field intact, the append-only property over a checkpoint carrying
//! a field this build does not know, and the checkpoint naming its fused
//! ancestor's Z-Map with no blob of its own.

use std::path::{Path, PathBuf};

use focusforge_core::align::{Registration, Transform};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::project::container::{ContainerWriter, Index};
use focusforge_core::project::{self, AlignRecord, Metrics, Op, Point, Project, SealRequest};
use focusforge_core::tile::Rect;
use focusforge_core::zmap::ZMapInfo;

/// A width that differs from `project_append.rs`'s own fixture size, so the
/// two suites' helpers cannot be mistaken for one another.
const W: u32 = 48;
const H: u32 = 32;
const SPP: u32 = 3;
const FRAMES: u32 = 3;

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

fn composite_samples() -> Vec<u16> {
    (0..(W as usize * H as usize * SPP as usize))
        .map(|i| (i * 65_535 / (W as usize * H as usize * SPP as usize)) as u16)
        .collect()
}

fn zmap_samples() -> Vec<u16> {
    (0..(W as usize * H as usize))
        .map(|i| (i % FRAMES as usize) as u16)
        .collect()
}

fn stage_planes(dir: &Path) -> (PathBuf, PathBuf) {
    let composite = dir.join("composite.tif");
    let zmap = dir.join("zmap.tif");
    let mut writer = focusforge_core::io::TiffWriter::create(
        &composite,
        focusforge_core::io::TiffSpec {
            width: W,
            height: H,
            samples_per_pixel: SPP,
            description: Some("staged composite".to_string()),
        },
    )
    .unwrap();
    writer.write_tile(full(W, H), &composite_samples()).unwrap();
    writer.finish().unwrap();

    let mut map =
        focusforge_core::zmap::ZMapWriter::create(&zmap, ZMapInfo::new(W, H, FRAMES).unwrap())
            .unwrap();
    map.write_tile(full(W, H), &zmap_samples()).unwrap();
    map.finish().unwrap();
    (composite, zmap)
}

fn write_sources(dir: &Path, count: usize) -> Vec<PathBuf> {
    (0..count)
        .map(|i| {
            let path = dir.join(format!("frame-{i:02}.tif"));
            let mut w = focusforge_core::io::TiffWriter::create(
                &path,
                focusforge_core::io::TiffSpec {
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

/// One sealed project in a fresh directory, plus the sources it was sealed
/// from.
fn sealed(name: &str) -> (PathBuf, PathBuf, Vec<PathBuf>) {
    let dir = tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging);
    let sources = write_sources(&dir, 3);
    let values = params();
    let project_path = dir.join(format!("demo.{}", project::PROJECT_EXTENSION));
    project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources: &sources,
        algorithm: "laplacian",
        params: &values,
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");
    (dir, project_path, sources)
}

/// A sealed project whose manifest also carries an [`AlignRecord`] (one
/// [`Transform`] per source frame) and a crop rect.
fn sealed_with_align_and_crop(name: &str) -> (PathBuf, PathBuf, Vec<PathBuf>) {
    let dir = tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging);
    let sources = write_sources(&dir, 3);
    let values = params();
    let project_path = dir.join(format!("demo.{}", project::PROJECT_EXTENSION));
    let transforms: Vec<Transform> = (0..FRAMES)
        .map(|i| {
            let mut m = Transform::identity().m;
            m[0][2] = i as f64 * 7.0;
            Transform { m }
        })
        .collect();
    project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources: &sources,
        algorithm: "laplacian",
        params: &values,
        align: Some(AlignRecord {
            algorithm: "dummy-align".to_string(),
            params: ParamValues::new(),
            reference: 0,
        }),
        registration: Registration {
            transforms,
            crop: Some(full(10, 8)),
            equalization: None,
        },
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");
    (dir, project_path, sources)
}

fn entry_names(path: &Path) -> Vec<String> {
    Index::read(path)
        .unwrap()
        .names()
        .map(str::to_string)
        .collect()
}

fn read_all_entries(path: &Path) -> std::collections::HashMap<String, Vec<u8>> {
    let index = Index::read(path).unwrap();
    index
        .names()
        .map(|name| (name.to_string(), index.read_entry(path, name).unwrap()))
        .collect()
}

/// Rewrites `path`'s `entry_name` JSON object with one extra, unknown field
/// -- so a serde re-serialize of that entry would drop the field and this
/// test would catch it.
fn inject_unknown_field(path: &Path, entry_name: &str) {
    let index = Index::read(path).unwrap();
    let rewritten = path.with_extension("rewritten");
    let mut writer = ContainerWriter::create(&rewritten).unwrap();
    for name in index.names() {
        let mut bytes = index.read_entry(path, name).unwrap();
        if name == entry_name {
            let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            value
                .as_object_mut()
                .unwrap()
                .insert("totally-unknown-field".to_string(), serde_json::json!(true));
            bytes = serde_json::to_vec_pretty(&value).unwrap();
        }
        writer.put_bytes(name, &bytes).unwrap();
    }
    writer.finish().unwrap();
    std::fs::rename(&rewritten, path).unwrap();
}

fn two_strokes() -> Vec<Op> {
    vec![
        Op::BrushStroke {
            source_id: 2,
            path: vec![Point { x: 3, y: 4 }, Point { x: 10, y: 12 }],
            radius: 3.5,
            softness: 0.25,
        },
        Op::BrushStroke {
            source_id: 1,
            path: vec![Point { x: 5, y: 2 }],
            radius: 2.0,
            softness: 0.0,
        },
    ]
}

#[test]
fn checkpoint_with_ops_round_trips() {
    let (_dir, project_path, sources) = sealed("history_log_round_trip");
    let entries_before = entry_names(&project_path);
    let head_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();

    // append_ops must not need the sources: delete them before sealing.
    for source in &sources {
        std::fs::remove_file(source).unwrap();
    }

    let ops = two_strokes();
    let sealed_ops = project::append_ops(&project_path, ops.clone(), Some("edit".to_string()))
        .expect("append_ops must succeed with the project's source frames deleted");

    let project = Project::open(&project_path).unwrap();
    assert_eq!(project.checkpoint().id, sealed_ops.checkpoint);
    assert_eq!(project.checkpoint().ops, ops);
    assert_eq!(
        project.checkpoint().parent.as_deref(),
        Some(head_id.as_str())
    );
    assert_eq!(project.checkpoint().label.as_deref(), Some("edit"));

    let entries_after = entry_names(&project_path);
    let new_entries: Vec<&String> = entries_after
        .iter()
        .filter(|e| !entries_before.contains(e))
        .collect();
    assert!(
        new_entries.iter().all(|e| !e.starts_with("blobs/")),
        "an op append writes no blob: {new_entries:?}"
    );
    assert_eq!(
        sealed_ops.zmap.hash,
        Project::open(&project_path)
            .unwrap()
            .history()
            .get(&head_id)
            .unwrap()
            .zmap_blob,
        "the op checkpoint names its fused ancestor's Z-Map"
    );
}

#[test]
fn sealing_a_checkpoint_mutates_no_existing_entry() {
    // Via `append_ops`.
    let (_dir, project_path, _sources) = sealed("history_log_append_only_ops");
    let head_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();
    inject_unknown_field(&project_path, &format!("checkpoints/{head_id}.json"));
    let before = read_all_entries(&project_path);

    project::append_ops(&project_path, two_strokes(), None).expect("append_ops must succeed");

    let after = read_all_entries(&project_path);
    for (name, bytes) in before.iter().filter(|(name, _)| *name != "manifest.json") {
        assert_eq!(
            after.get(name),
            Some(bytes),
            "entry {name} must be byte-identical after append_ops"
        );
    }

    // Via the plain op-less `append`.
    let (dir2, project_path2, sources2) = sealed("history_log_append_only_plain");
    let head_id2 = Project::open(&project_path2)
        .unwrap()
        .checkpoint()
        .id
        .clone();
    inject_unknown_field(&project_path2, &format!("checkpoints/{head_id2}.json"));
    let before2 = read_all_entries(&project_path2);

    let staging2 = dir2.join("staging-2");
    std::fs::create_dir_all(&staging2).unwrap();
    let (composite2, zmap2) = stage_planes(&staging2);
    project::append(project::AppendRequest {
        project_path: &project_path2,
        staged_composite: &composite2,
        staged_zmap: &zmap2,
        sources: &sources2,
        algorithm: "laplacian",
        params: &params(),
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("append must succeed");

    let after2 = read_all_entries(&project_path2);
    for (name, bytes) in before2.iter().filter(|(name, _)| *name != "manifest.json") {
        assert_eq!(
            after2.get(name),
            Some(bytes),
            "entry {name} must be byte-identical after append"
        );
    }
}

#[test]
fn append_ops_refuses_an_empty_list() {
    let (_dir, project_path, _sources) = sealed("history_log_empty_list");
    match project::append_ops(&project_path, Vec::new(), None) {
        Err(focusforge_core::project::ProjectError::EmptyOpList { .. }) => {}
        other => panic!("expected EmptyOpList, got {other:?}"),
    }
}

#[test]
fn alignment_and_crop_are_recorded() {
    let (_dir, project_path, _sources) = sealed_with_align_and_crop("history_log_align_crop");
    let manifest = Project::open(&project_path).unwrap().manifest().clone();
    manifest.align.as_ref().expect("align must round-trip");
    assert_eq!(manifest.registration.transforms.len(), FRAMES as usize);
    assert_eq!(manifest.registration.transforms[1].m[0][2], 7.0);
    assert_eq!(manifest.registration.transforms[2].m[0][2], 14.0);
    assert_eq!(manifest.registration.crop, Some(full(10, 8)));

    // `append_ops` carries both forward, unchanged.
    project::append_ops(&project_path, two_strokes(), None).expect("append_ops must succeed");
    let after = Project::open(&project_path).unwrap().manifest().clone();
    assert_eq!(after.align, manifest.align);
    assert_eq!(after.registration, manifest.registration);

    // A manifest JSON with the field missing is refused.
    let mut value = serde_json::to_value(&manifest).unwrap();
    value.as_object_mut().unwrap().remove("registration");
    let err = serde_json::from_value::<focusforge_core::project::Manifest>(value).unwrap_err();
    assert!(err.to_string().contains("registration"), "{err}");
}
