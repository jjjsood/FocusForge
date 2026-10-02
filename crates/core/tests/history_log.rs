// SPDX-License-Identifier: GPL-3.0-or-later

//! Sealing a non-empty op list as a checkpoint: reading it back with every
//! op and field intact, the append-only property over a checkpoint carrying
//! a field this build does not know, the stored Z-Map blob matching an
//! independent replay of the parent's map, and a reset after an op-bearing
//! head resolving its fused ancestor from disk.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use focusforge_core::align::{Registration, Transform};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::project::{
    self, AlignRecord, Metrics, Op, Point, Project, SealRequest, materialize_zmap,
};
use focusforge_core::retouch::replay;
use focusforge_core::tile::Rect;
use focusforge_core::zmap::{ZMapInfo, ZMapReader};

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
        },
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");
    (dir, project_path, sources)
}

fn entry_names(path: &Path) -> Vec<String> {
    let file = std::fs::File::open(path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect()
}

fn read_all_entries(path: &Path) -> std::collections::HashMap<String, Vec<u8>> {
    let file = std::fs::File::open(path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    names
        .into_iter()
        .map(|name| {
            let mut bytes = Vec::new();
            archive
                .by_name(&name)
                .unwrap()
                .read_to_end(&mut bytes)
                .unwrap();
            (name, bytes)
        })
        .collect()
}

/// Rewrites `path`'s `entry_name` JSON object with one extra, unknown field
/// -- so a serde re-serialize of that entry would drop the field and this
/// test would catch it.
fn inject_unknown_field(path: &Path, entry_name: &str) {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for name in &names {
        let mut bytes = Vec::new();
        archive
            .by_name(name)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        if name == entry_name {
            let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            value
                .as_object_mut()
                .unwrap()
                .insert("totally-unknown-field".to_string(), serde_json::json!(true));
            bytes = serde_json::to_vec_pretty(&value).unwrap();
        }
        entries.push((name.clone(), bytes));
    }
    drop(archive);

    let mut writer = zip::write::ZipWriter::new(std::fs::File::create(path).unwrap());
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, bytes) in entries {
        writer.start_file(&*name, options).unwrap();
        writer.write_all(&bytes).unwrap();
    }
    writer.finish().unwrap();
}

fn one_of_each_replayable_variant() -> Vec<Op> {
    vec![
        Op::BrushStroke {
            source_id: 2,
            path: vec![Point { x: 3, y: 4 }, Point { x: 10, y: 12 }],
            radius: 3.5,
            softness: 0.25,
        },
        Op::ResetRegion {
            rect: Rect {
                x: 1,
                y: 1,
                width: 4,
                height: 3,
            },
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

    let ops = one_of_each_replayable_variant();
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
    let new_blobs: Vec<&&String> = new_entries
        .iter()
        .filter(|e| e.starts_with("blobs/"))
        .collect();
    assert_eq!(
        new_blobs.len(),
        1,
        "exactly one new blob (the Z-Map, no composite) must be added: {new_entries:?}"
    );
    assert!(
        new_blobs[0].ends_with(&format!("{}.zst", sealed_ops.zmap.hash)),
        "the one new blob must be the sealed Z-Map: {new_blobs:?}"
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

    project::append_ops(&project_path, one_of_each_replayable_variant(), None)
        .expect("append_ops must succeed");

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
    project::append_ops(&project_path, one_of_each_replayable_variant(), None)
        .expect("append_ops must succeed");
    let after = Project::open(&project_path).unwrap().manifest().clone();
    assert_eq!(after.align, manifest.align);
    assert_eq!(after.registration, manifest.registration);

    // A manifest JSON with the field missing is refused.
    let mut value = serde_json::to_value(&manifest).unwrap();
    value.as_object_mut().unwrap().remove("registration");
    let err = serde_json::from_value::<focusforge_core::project::Manifest>(value).unwrap_err();
    assert!(err.to_string().contains("registration"), "{err}");
}

/// Reads a checkpoint's Z-Map blob into a full-map index buffer.
fn read_full_zmap(
    project: &mut Project,
    checkpoint: &project::Checkpoint,
    path: &Path,
) -> Vec<u16> {
    materialize_zmap(project, checkpoint, path).expect("materialize_zmap must succeed");
    let reader = ZMapReader::open(path).expect("the materialized Z-Map must open");
    let info = ZMapInfo::new(W, H, FRAMES).unwrap();
    let mut indices = vec![0u16; info.tile_len(full(W, H))];
    reader
        .read_tile_into(full(W, H), &mut indices)
        .expect("reading the full map must succeed");
    indices
}

#[test]
fn append_ops_zmap_blob_matches_a_replay_of_the_parents_map() {
    let (dir, project_path, _sources) = sealed("history_log_matches_replay");
    let mut project = Project::open(&project_path).unwrap();
    let parent = project.checkpoint().clone();
    let parent_indices = read_full_zmap(&mut project, &parent, &dir.join("parent-zmap.tif"));

    let info = ZMapInfo::new(W, H, FRAMES).unwrap();
    let full_rect = full(W, H);
    let ops = one_of_each_replayable_variant();
    let mut expected = parent_indices.clone();
    let mut fused_from_parent =
        |rect: Rect, buf: &mut [u16]| -> Result<(), focusforge_core::zmap::ZMapError> {
            for row in 0..rect.height {
                let src = ((rect.y + row) * info.width() + rect.x) as usize;
                let dst = (row * rect.width) as usize;
                buf[dst..dst + rect.width as usize]
                    .copy_from_slice(&parent_indices[src..src + rect.width as usize]);
            }
            Ok(())
        };
    replay::replay(&ops, info, full_rect, &mut expected, &mut fused_from_parent)
        .expect("in-memory replay must succeed");

    project::append_ops(&project_path, ops, None).expect("append_ops must succeed");

    let mut reopened = Project::open(&project_path).unwrap();
    let head = reopened.checkpoint().clone();
    let actual = read_full_zmap(&mut reopened, &head, &dir.join("head-zmap.tif"));

    assert_eq!(
        actual, expected,
        "the Z-Map blob append_ops wrote must match an independent replay of the parent's map"
    );
}

#[test]
fn append_ops_reset_after_ops_resolves_the_fused_ancestor_from_disk() {
    let (dir, project_path, _sources) = sealed("history_log_reset_after_ops");
    let mut project = Project::open(&project_path).unwrap();
    let fused_root = project.checkpoint().clone();
    let fused_indices = read_full_zmap(&mut project, &fused_root, &dir.join("fused-zmap.tif"));

    let info = ZMapInfo::new(W, H, FRAMES).unwrap();
    let brush_rect = Rect {
        x: 2,
        y: 2,
        width: 6,
        height: 5,
    };
    let brush = Op::BrushStroke {
        source_id: 1,
        path: vec![Point { x: 4, y: 4 }],
        radius: 3.0,
        softness: 0.0,
    };
    project::append_ops(&project_path, vec![brush], None)
        .expect("the first append_ops (brush, onto the fused root) must succeed");

    let mut after_brush = Project::open(&project_path).unwrap();
    let brushed = after_brush.checkpoint().clone();
    assert!(
        !brushed.ops.is_empty(),
        "the head must now carry the brush op"
    );
    let brushed_indices = read_full_zmap(&mut after_brush, &brushed, &dir.join("brushed-zmap.tif"));
    let region = |indices: &[u16], rect: Rect| -> Vec<u16> {
        let mut out = Vec::with_capacity(info.tile_len(rect));
        for row in 0..rect.height {
            let start = ((rect.y + row) * info.width() + rect.x) as usize;
            out.extend_from_slice(&indices[start..start + rect.width as usize]);
        }
        out
    };
    assert_ne!(
        region(&brushed_indices, brush_rect),
        region(&fused_indices, brush_rect),
        "the brush must actually change its region before the reset undoes it"
    );

    let reset_rect = Rect {
        x: 3,
        y: 3,
        width: 3,
        height: 3,
    };
    project::append_ops(
        &project_path,
        vec![Op::ResetRegion { rect: reset_rect }],
        None,
    )
    .expect("the second append_ops (reset, onto an op-bearing head) must succeed");

    let mut final_project = Project::open(&project_path).unwrap();
    let final_head = final_project.checkpoint().clone();
    let final_indices =
        read_full_zmap(&mut final_project, &final_head, &dir.join("final-zmap.tif"));

    assert_eq!(
        region(&final_indices, reset_rect),
        region(&fused_indices, reset_rect),
        "the reset must restore the fused root's indices, not the op-bearing parent's"
    );
}
