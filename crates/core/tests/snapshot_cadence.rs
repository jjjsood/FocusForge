// SPDX-License-Identifier: GPL-3.0-or-later

//! The snapshot cadence: the root checkpoint of every project is full, the
//! writer's 32-link cap on any chain it produces, the byte budget that can
//! fire before the cap, storage-choice invisibility at the record level,
//! and that an all-full project opens and is not rewritten.

mod common;

use std::path::{Path, PathBuf};

use focusforge_core::align::Registration;
use focusforge_core::io::{Rect, TiffSpec, TiffWriter};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::project::container::Index;
use focusforge_core::project::{
    self, BlobKind, Metrics, Op, Point, Project, SealRequest, ZMapChain, ZMapStorage,
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

fn composite_samples(w: u32, h: u32) -> Vec<u16> {
    (0..(w as usize * h as usize * SPP as usize))
        .map(|i| {
            let x = (i / SPP as usize) % w.max(1) as usize;
            let channel = i % SPP as usize;
            ((x * 65_535 / w.max(1) as usize) as u16).wrapping_add(channel as u16 * 97)
        })
        .collect()
}

/// A high-entropy Z-Map, deliberately **not** piecewise-constant: it keeps a
/// full-map blob from compressing to nothing on its own, so a delta's size
/// against it is a real comparison.
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

fn request<'a>(
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

/// One sealed project in a fresh directory over a synthetic, noisy `w x h`
/// Z-Map with `frames` source frames.
fn sealed(name: &str, w: u32, h: u32, frames: u32) -> (PathBuf, PathBuf) {
    let dir = tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging, w, h, frames);
    let sources = write_sources(&dir, 3, w, h);
    let values = params();
    let project_path = dir.join(format!("demo.{}", project::PROJECT_EXTENSION));
    project::seal_at(
        request(&project_path, &composite, &zmap, &sources, &values),
        1_700_000_000,
    )
    .expect("seal must succeed");
    (dir, project_path)
}

/// One sealed project over `fixtures/mantis-512`'s own already-fused golden
/// planes, with its own 8 source frames referenced.
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
    project::seal(request(&project_path, &composite, &zmap, &sources, &values))
        .expect("seal must succeed");
    (dir, project_path)
}

/// A one-point brush stroke of `radius`, small enough that its footprint --
/// and so its delta -- stays a fraction of the map it edits.
fn stroke(source_id: u16, x: i32, y: i32, radius: f32) -> Op {
    Op::BrushStroke {
        source_id,
        path: vec![Point { x, y }],
        radius,
        softness: 0.0,
    }
}

fn small_stroke(source_id: u16, x: i32, y: i32) -> Op {
    stroke(source_id, x, y, 3.0)
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

fn read_checkpoint_json(path: &Path, id: &str) -> serde_json::Value {
    let bytes = read_entry_bytes(path, &format!("checkpoints/{id}.json"));
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn root_checkpoint_is_always_full() {
    for (name, w, h) in [
        ("snapshot_cadence_root_64x48", 64u32, 48u32),
        ("snapshot_cadence_root_37x23", 37u32, 23u32),
    ] {
        let (_dir, project_path) = sealed(name, w, h, 8);
        let project = Project::open(&project_path).unwrap();
        let head = project.checkpoint().clone();
        let record = project
            .manifest()
            .blobs
            .iter()
            .find(|b| b.hash == head.zmap_blob)
            .expect("the root checkpoint's zmap_blob must resolve to a blob record directly");
        assert_eq!(record.kind, BlobKind::Full, "{name}: root must be full");

        let chain = project.zmap_chain(&head).unwrap();
        assert_eq!(chain.deltas, 0, "{name}: root has no deltas behind it");
    }
}

#[test]
fn root_checkpoint_is_always_full_over_a_branched_history() {
    let (_dir, project_path) = sealed("snapshot_cadence_root_branched", 64, 48, 8);
    let root_id = Project::open(&project_path)
        .unwrap()
        .checkpoint()
        .id
        .clone();

    project::append_ops_at(
        &project_path,
        vec![stroke(1, 10, 10, 1.0)],
        None,
        1_700_000_000,
        ZMapStorage::Smallest,
    )
    .unwrap();
    project::append_ops_onto(
        &project_path,
        &root_id,
        vec![stroke(2, 30, 20, 1.0)],
        None,
        1_700_000_010,
        ZMapStorage::Smallest,
    )
    .unwrap();

    let project = Project::open(&project_path).unwrap();
    let root = project.history().get(&root_id).unwrap();
    let record = project
        .manifest()
        .blobs
        .iter()
        .find(|b| b.hash == root.zmap_blob)
        .expect("the root's zmap_blob must resolve to a blob record directly");
    assert_eq!(
        record.kind,
        BlobKind::Full,
        "a branched root must still be full"
    );

    let chain = project.zmap_chain(root).unwrap();
    assert_eq!(
        chain.deltas, 0,
        "the root has no deltas behind it, branched or not"
    );
}

#[test]
fn open_resolves_at_most_thirty_two_links() {
    // A high frame count, not just a high pixel count: the deterministic
    // hash `zmap_noisy` folds into `frames` values compresses far better
    // than its pixel count suggests once `frames` is small (the noise
    // wraps into a short, zstd-visible cycle), so a wide frame count is
    // what actually keeps the full blob ahead of thirty-two one-pixel
    // deltas here.
    let (_dir, project_path) = sealed("snapshot_cadence_cap", 256, 256, 251);
    let mut max_deltas = 0usize;
    for i in 0..100i32 {
        let x = 5 + (i * 2) % 240;
        let y = 5 + (i * 3) % 240;
        let source_id = (i % 251) as u16;
        project::append_ops(&project_path, vec![stroke(source_id, x, y, 1.0)], None)
            .unwrap_or_else(|err| panic!("append #{i} must succeed: {err}"));

        let project = Project::open(&project_path).unwrap();
        let head = project.checkpoint().clone();
        let chain = project.zmap_chain(&head).unwrap();
        assert!(
            chain.deltas <= 32,
            "link #{i}: {} deltas exceeds the cap",
            chain.deltas
        );
        max_deltas = max_deltas.max(chain.deltas);
    }
    assert_eq!(
        max_deltas, 32,
        "over a hundred one-pixel edits the cap must fire, not just the never-reached budget"
    );

    let (dir2, uncapped_path) = sealed("snapshot_cadence_uncapped_40", 64, 48, 8);
    for i in 0..40i32 {
        let x = 10 + (i * 3) % 40;
        let y = 10 + (i * 2) % 30;
        let sealed_ops = project::append_ops_at(
            &uncapped_path,
            vec![stroke((i % 8) as u16, x, y, 1.0)],
            None,
            1_700_000_000 + i as i64,
            ZMapStorage::Smallest,
        )
        .unwrap_or_else(|err| panic!("uncapped append #{i} must succeed: {err}"));
        assert_eq!(
            sealed_ops.zmap.kind,
            BlobKind::SparseDelta,
            "link #{i} must land as a delta to build a real 40-long chain"
        );
    }
    let mut project = Project::open(&uncapped_path).unwrap();
    let head = project.checkpoint().clone();
    let chain = project.zmap_chain(&head).unwrap();
    assert_eq!(
        chain.deltas, 40,
        "an uncapped chain reports its true length"
    );

    let out = dir2.join("uncapped-40.tif");
    project::materialize_zmap(&mut project, &head, &out)
        .expect("a 40-link uncapped chain must still materialize, never refused on read");
}

#[test]
fn byte_budget_fires_when_deltas_outgrow_the_snapshot() {
    let (_dir, project_path) = sealed("snapshot_cadence_budget", 64, 48, 8);
    let mut budget_fired_early = false;
    for i in 0..20i32 {
        let x = 5 + (i * 3) % 54;
        let y = 5 + (i * 2) % 38;

        let project = Project::open(&project_path).unwrap();
        let head = project.checkpoint().clone();
        let predicted = project.zmap_chain(&head).unwrap();
        let predicted_needs_snapshot = predicted.needs_snapshot();
        drop(project);

        let sealed_ops =
            project::append_ops(&project_path, vec![stroke((i % 8) as u16, x, y, 5.0)], None)
                .unwrap_or_else(|err| panic!("append #{i} must succeed: {err}"));

        if predicted_needs_snapshot {
            assert_eq!(
                sealed_ops.zmap.kind,
                BlobKind::Full,
                "predicted snapshot #{i} must land full"
            );
            if predicted.deltas < 32 {
                budget_fired_early = true;
            }
        } else {
            assert_eq!(
                sealed_ops.zmap.kind,
                BlobKind::SparseDelta,
                "predicted delta #{i} must land as a delta"
            );
        }
    }
    assert!(
        budget_fired_early,
        "at least one snapshot must be forced by the byte budget before the cap could"
    );
}

#[test]
fn snapshots_do_not_change_the_result() {
    let (dir_a, project_a) = sealed("snapshot_cadence_equal_bounded", 96, 64, 8);
    let (dir_b, project_b) = sealed("snapshot_cadence_equal_smallest", 96, 64, 8);

    for i in 0..40i32 {
        let x = 5 + (i * 3) % 86;
        let y = 5 + (i * 2) % 54;
        let source_id = (i % 8) as u16;
        let created = 1_700_000_000 + i as i64;
        let sealed_a = project::append_ops_at(
            &project_a,
            vec![small_stroke(source_id, x, y)],
            None,
            created,
            ZMapStorage::Bounded,
        )
        .unwrap_or_else(|err| panic!("bounded append #{i} must succeed: {err}"));
        let sealed_b = project::append_ops_at(
            &project_b,
            vec![small_stroke(source_id, x, y)],
            None,
            created,
            ZMapStorage::Smallest,
        )
        .unwrap_or_else(|err| panic!("uncapped append #{i} must succeed: {err}"));
        assert_eq!(
            sealed_a.checkpoint, sealed_b.checkpoint,
            "checkpoint #{i} ids must match regardless of storage policy"
        );
    }

    let mut project_a_open = Project::open(&project_a).unwrap();
    let mut project_b_open = Project::open(&project_b).unwrap();
    let chain_a = project_a_open.checkpoint_chain().unwrap();
    let chain_b = project_b_open.checkpoint_chain().unwrap();
    assert_eq!(chain_a.len(), chain_b.len());

    for (checkpoint_a, checkpoint_b) in chain_a.iter().zip(&chain_b) {
        assert_eq!(checkpoint_a.id, checkpoint_b.id);
        let out_a = dir_a.join(format!("materialize-{}.tif", checkpoint_a.id));
        let out_b = dir_b.join(format!("materialize-{}.tif", checkpoint_b.id));
        project::materialize_zmap(&mut project_a_open, checkpoint_a, &out_a).unwrap();
        project::materialize_zmap(&mut project_b_open, checkpoint_b, &out_b).unwrap();
        assert_eq!(
            read_zmap_pixels(&out_a),
            read_zmap_pixels(&out_b),
            "checkpoint {} must materialize identically under both policies",
            checkpoint_a.id
        );
    }

    let full_count_a = project_a_open
        .manifest()
        .blobs
        .iter()
        .filter(|b| b.kind == BlobKind::Full)
        .count();
    let full_count_b = project_b_open
        .manifest()
        .blobs
        .iter()
        .filter(|b| b.kind == BlobKind::Full)
        .count();
    assert!(
        full_count_a > full_count_b,
        "the bounded project must hold at least one more full record than the uncapped one \
         ({full_count_a} vs {full_count_b})"
    );
}

/// Two 49-checkpoint histories over `fixtures/mantis-512`, one written
/// through the cadence and one uncapped -- run once, printing both timings
/// rather than asserting past the deterministic proxy below.
#[test]
#[ignore]
fn open_cost_is_bounded_over_a_long_history() {
    let (dir_bounded, bounded_path) = sealed_mantis("snapshot_cadence_open_cost_bounded");
    let (dir_unbounded, unbounded_path) = sealed_mantis("snapshot_cadence_open_cost_unbounded");

    for i in 0..49i32 {
        let x = 20 + (i * 7) % 460;
        let y = 20 + (i * 5) % 460;
        let source_id = (i % 8) as u16;
        let created = 1_700_000_000 + i as i64;
        project::append_ops_at(
            &bounded_path,
            vec![small_stroke(source_id, x, y)],
            None,
            created,
            ZMapStorage::Bounded,
        )
        .unwrap_or_else(|err| panic!("bounded append #{i} must succeed: {err}"));
        project::append_ops_at(
            &unbounded_path,
            vec![small_stroke(source_id, x, y)],
            None,
            created,
            ZMapStorage::Smallest,
        )
        .unwrap_or_else(|err| panic!("unbounded append #{i} must succeed: {err}"));
    }

    let mut bounded_project = Project::open(&bounded_path).unwrap();
    let mut unbounded_project = Project::open(&unbounded_path).unwrap();
    let bounded_head = bounded_project.checkpoint().clone();
    let unbounded_head = unbounded_project.checkpoint().clone();

    let bounded_chain = bounded_project.zmap_chain(&bounded_head).unwrap();
    let unbounded_chain = unbounded_project.zmap_chain(&unbounded_head).unwrap();

    let bounded_out = dir_bounded.join("open-bounded.tif");
    let start = std::time::Instant::now();
    project::materialize_zmap(&mut bounded_project, &bounded_head, &bounded_out).unwrap();
    let bounded_time = start.elapsed();

    let unbounded_out = dir_unbounded.join("open-unbounded.tif");
    let start = std::time::Instant::now();
    project::materialize_zmap(&mut unbounded_project, &unbounded_head, &unbounded_out).unwrap();
    let unbounded_time = start.elapsed();

    let bounded_bytes = bounded_chain.snapshot_bytes + bounded_chain.delta_bytes;
    let unbounded_bytes = unbounded_chain.snapshot_bytes + unbounded_chain.delta_bytes;

    println!(
        "bounded:   {:?} open, {} links, {bounded_bytes} B resolved, file {} B",
        bounded_time,
        bounded_chain.deltas,
        std::fs::metadata(&bounded_path).unwrap().len()
    );
    println!(
        "unbounded: {:?} open, {} links, {unbounded_bytes} B resolved, file {} B",
        unbounded_time,
        unbounded_chain.deltas,
        std::fs::metadata(&unbounded_path).unwrap().len()
    );

    assert!(bounded_chain.deltas <= 32);
    // Content dedup can merge a no-op edit into an earlier record, so the chain may be shorter than the edit count.
    assert!(
        unbounded_chain.deltas >= 40,
        "an uncapped chain over 49 edits must not be capped"
    );
    assert!(unbounded_chain.deltas > bounded_chain.deltas);
    assert!(bounded_bytes < unbounded_bytes);
}

#[test]
fn cadence_is_one_policy_of_two_constants() {
    let cap = project::SNAPSHOT_CHECKPOINT_CAP;
    let budget = project::SNAPSHOT_BYTE_BUDGET;
    let snapshot_bytes = 1_000u64;

    assert!(
        !ZMapChain {
            snapshot_bytes,
            deltas: cap - 1,
            delta_bytes: snapshot_bytes * budget,
        }
        .needs_snapshot(),
        "one below the cap, exactly at the budget: not yet"
    );
    assert!(
        ZMapChain {
            snapshot_bytes,
            deltas: cap,
            delta_bytes: 0,
        }
        .needs_snapshot(),
        "exactly at the cap: forced regardless of bytes"
    );
    assert!(
        !ZMapChain {
            snapshot_bytes,
            deltas: 0,
            delta_bytes: snapshot_bytes * budget,
        }
        .needs_snapshot(),
        "exactly at the budget, not over it: not yet"
    );
    assert!(
        ZMapChain {
            snapshot_bytes,
            deltas: 0,
            delta_bytes: snapshot_bytes * budget + 1,
        }
        .needs_snapshot(),
        "one byte past the budget: forced"
    );
    assert!(
        !ZMapChain {
            snapshot_bytes: 0,
            deltas: 0,
            delta_bytes: 0,
        }
        .needs_snapshot(),
        "a zero-byte snapshot with no deltas yet: not yet"
    );
    assert!(
        ZMapChain {
            snapshot_bytes: 0,
            deltas: 0,
            delta_bytes: 1,
        }
        .needs_snapshot(),
        "a zero-byte snapshot: any delta at all exceeds it"
    );
}

#[test]
fn checkpoint_record_does_not_reveal_storage_kind() {
    let (_dir, project_path) = sealed("snapshot_cadence_invisible", 64, 48, 8);
    let mut forced_full_id: Option<String> = None;
    let mut delta_id: Option<String> = None;
    for i in 0..10i32 {
        let x = 5 + (i * 5) % 54;
        let y = 5 + (i * 4) % 38;

        let project = Project::open(&project_path).unwrap();
        let head = project.checkpoint().clone();
        let predicted_needs_snapshot = project.zmap_chain(&head).unwrap().needs_snapshot();
        drop(project);

        let sealed_ops =
            project::append_ops(&project_path, vec![stroke((i % 8) as u16, x, y, 5.0)], None)
                .unwrap_or_else(|err| panic!("append #{i} must succeed: {err}"));

        if predicted_needs_snapshot && forced_full_id.is_none() {
            assert_eq!(sealed_ops.zmap.kind, BlobKind::Full);
            forced_full_id = Some(sealed_ops.checkpoint.clone());
        } else if !predicted_needs_snapshot
            && sealed_ops.zmap.kind == BlobKind::SparseDelta
            && delta_id.is_none()
        {
            delta_id = Some(sealed_ops.checkpoint.clone());
        }
        if forced_full_id.is_some() && delta_id.is_some() {
            break;
        }
    }
    let forced_full_id = forced_full_id
        .expect("the byte budget must force at least one snapshot within 10 edits at this size");
    let delta_id =
        delta_id.expect("at least one sparse-delta checkpoint must appear within 10 edits");

    let full_json = read_checkpoint_json(&project_path, &forced_full_id);
    let delta_json = read_checkpoint_json(&project_path, &delta_id);
    let full_keys: std::collections::BTreeSet<_> = full_json.as_object().unwrap().keys().collect();
    let delta_keys: std::collections::BTreeSet<_> =
        delta_json.as_object().unwrap().keys().collect();
    assert_eq!(
        full_keys, delta_keys,
        "a full and a delta checkpoint record must carry the same key set"
    );

    let project = Project::open(&project_path).unwrap();
    let blobs = &project.manifest().blobs;
    let full_zmap_blob = full_json["zmap_blob"].as_str().unwrap();
    blobs
        .iter()
        .find(|b| b.kind == BlobKind::Full && b.hash == full_zmap_blob)
        .expect("a full checkpoint's zmap_blob must be a full record's own hash");

    let delta_zmap_blob = delta_json["zmap_blob"].as_str().unwrap();
    blobs
        .iter()
        .find(|b| b.kind == BlobKind::SparseDelta && b.target.as_deref() == Some(delta_zmap_blob))
        .expect("a delta checkpoint's zmap_blob must be a delta record's target");
}

#[test]
fn full_only_project_is_not_rewritten() {
    let (dir, project_path) = sealed("snapshot_cadence_full_only", 64, 48, 8);
    for i in 0..3i32 {
        let x = 10 + i * 5;
        let y = 10 + i * 4;
        project::append_ops_at(
            &project_path,
            vec![small_stroke((i % 8) as u16, x, y)],
            None,
            1_700_000_000 + i as i64,
            ZMapStorage::Full,
        )
        .unwrap_or_else(|err| panic!("full append #{i} must succeed: {err}"));
    }

    let mut project = Project::open(&project_path).unwrap();
    let head = project.checkpoint().clone();
    let chain = project.zmap_chain(&head).unwrap();
    assert_eq!(
        chain.deltas, 0,
        "a full head resolves with 0 deltas behind it"
    );

    let bytes_before = std::fs::read(&project_path).unwrap();
    let out = dir.join("full-only-head.tif");
    project::materialize_zmap(&mut project, &head, &out).unwrap();
    let bytes_after_open = std::fs::read(&project_path).unwrap();
    assert_eq!(
        bytes_before, bytes_after_open,
        "opening and materializing the head must not rewrite the container"
    );

    let before_snapshot = dir.join("before-last-append.ffp");
    std::fs::copy(&project_path, &before_snapshot).unwrap();

    let sealed_ops = project::append_ops(&project_path, vec![small_stroke(4, 40, 30)], None)
        .expect("append_ops onto an all-full project must succeed");
    assert_eq!(
        sealed_ops.zmap.kind,
        BlobKind::SparseDelta,
        "0 deltas behind a full head, nothing blocks the smaller-of-two choice"
    );

    let before_names: Vec<String> = Index::read(&before_snapshot)
        .unwrap()
        .names()
        .map(str::to_string)
        .collect();
    for name in &before_names {
        if name == "manifest.json" {
            continue; // a new blob record and head change manifest.json
        }
        let before_bytes = read_entry_bytes(&before_snapshot, name);
        let after_bytes = read_entry_bytes(&project_path, name);
        assert_eq!(
            before_bytes, after_bytes,
            "entry {name} must be unchanged by the append"
        );
    }
}
