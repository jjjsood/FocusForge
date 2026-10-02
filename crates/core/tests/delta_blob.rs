// SPDX-License-Identifier: GPL-3.0-or-later

//! Sparse-delta checkpoints: bit-identical decode through every read path,
//! the sparse-delta blob kind and its size and hash rules, the two storage refusals,
//! the materialized-parent rule, the whole-map fallback and the format
//! version a delta raises.

mod common;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use focusforge_core::align::Registration;
use focusforge_core::io::{Rect, TiffSpec, TiffWriter};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::project::{
    self, BlobKind, BlobRecord, Metrics, Op, Plane, Point, Project, ProjectError, SealRequest,
    ZMapStorage, blob, partial_path,
};
use focusforge_core::retouch::replay;
use focusforge_core::zmap::{ZMapError, ZMapInfo, ZMapReader, ZMapWriter};

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

/// A high-entropy Z-Map, deliberately **not** piecewise-constant: a delta
/// is meant to beat the full blob on the *edit* itself, not on a map that
/// already compresses to nothing on its own.
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
    project::seal(request(&project_path, &composite, &zmap, &sources, &values))
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

fn small_stroke(source_id: u16, x: i32, y: i32) -> Op {
    Op::BrushStroke {
        source_id,
        path: vec![Point { x, y }],
        radius: 3.0,
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

/// `base`'s own indices over `tile`, row-major -- what a replay call needs
/// as its resident tile buffer.
fn extract(base: &[u16], info: ZMapInfo, tile: Rect) -> Vec<u16> {
    let mut out = Vec::with_capacity(info.tile_len(tile));
    for y in tile.y..tile.y + tile.height {
        let start = (y * info.width() + tile.x) as usize;
        out.extend_from_slice(&base[start..start + tile.width as usize]);
    }
    out
}

/// An independent oracle for what `ops` must turn `base` into: replays
/// `ops` against a tile-sized buffer read out of `base`, then splices the
/// result back over a copy of `base` -- exactly the shape
/// `retouch::write_edited` defines a child map to have. Never shares code
/// with the delta encoder or decoder under test.
fn expected_after(base: &[u16], info: ZMapInfo, ops: &[Op]) -> Vec<u16> {
    let tile = replay::replay_tile(ops, info).expect("ops must have a footprint");
    let mut tile_buf = extract(base, info, tile);
    let mut fused = |rect: Rect, buf: &mut [u16]| -> Result<(), ZMapError> {
        for row in 0..rect.height {
            let src_start = ((rect.y + row) * info.width() + rect.x) as usize;
            let dst_start = (row * rect.width) as usize;
            buf[dst_start..dst_start + rect.width as usize]
                .copy_from_slice(&base[src_start..src_start + rect.width as usize]);
        }
        Ok(())
    };
    replay::replay(ops, info, tile, &mut tile_buf, &mut fused).expect("replay must succeed");
    let mut out = base.to_vec();
    for row in 0..tile.height {
        let dst_start = ((tile.y + row) * info.width() + tile.x) as usize;
        let src_start = (row * tile.width) as usize;
        out[dst_start..dst_start + tile.width as usize]
            .copy_from_slice(&tile_buf[src_start..src_start + tile.width as usize]);
    }
    out
}

/// Asserts that `project_path`'s head -- which must be stored as a delta --
/// decodes to `expected` through all three read paths: `materialize_zmap`,
/// `materialize_plane(Plane::ZMap)` and `read_plane_into(Plane::ZMap)`.
fn assert_all_paths_bit_identical(dir: &Path, project_path: &Path, expected: &[u16]) {
    let mut project = Project::open(project_path).unwrap();
    let head = project.checkpoint().clone();
    let record = project
        .manifest()
        .blobs
        .iter()
        .find(|b| b.hash == head.zmap_blob || b.target.as_deref() == Some(head.zmap_blob.as_str()))
        .expect("head zmap_blob must resolve to a blob record");
    assert_eq!(
        record.kind,
        BlobKind::SparseDelta,
        "test setup must produce a delta-stored head"
    );

    let via_materialize_zmap = dir.join("via-materialize-zmap.tif");
    project::materialize_zmap(&mut project, &head, &via_materialize_zmap).unwrap();
    assert_eq!(read_zmap_pixels(&via_materialize_zmap), expected);

    let via_materialize_plane = dir.join("via-materialize-plane.tif");
    project::materialize_plane(&mut project, Plane::ZMap, &via_materialize_plane).unwrap();
    assert_eq!(read_zmap_pixels(&via_materialize_plane), expected);

    let width = project.manifest().zmap.width;
    let mut streamed = vec![0u16; expected.len()];
    project
        .read_plane_into(Plane::ZMap, &mut |band, rect| {
            let start = rect.y as usize * width as usize;
            streamed[start..start + band.len()].copy_from_slice(band);
        })
        .unwrap();
    assert_eq!(streamed, expected);
}

fn read_entry_bytes(path: &Path, name: &str) -> Vec<u8> {
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

/// Copies `src` to `dst`, replacing the named entries in `replace` and
/// appending the entries in `add` -- a raw rewrite of a real container, so a
/// test can inject a manifest record or a blob the production writer would
/// never produce, without the writer growing a hook that exists only for
/// tests.
fn mutate_container(
    src: &Path,
    dst: &Path,
    replace: &[(&str, Vec<u8>)],
    add: &[(String, Vec<u8>)],
) {
    let mut archive = zip::ZipArchive::new(std::fs::File::open(src).unwrap()).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    let mut writer = zip::write::ZipWriter::new(std::fs::File::create(dst).unwrap());
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for name in &names {
        let mut bytes = Vec::new();
        archive
            .by_name(name)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        if let Some(entry) = replace.iter().find(|entry| entry.0 == name.as_str()) {
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

/// Builds a corrupted copy of `project_path` under `dir` whose head
/// checkpoint's `zmap_blob` is `target_hash`, resolved through exactly one
/// hand-placed blob record -- what both storage refusals need: a project
/// whose head resolves through a record no real write path produced.
fn corrupted_delta_head(
    dir: &Path,
    name: &str,
    project_path: &Path,
    target_hash: &str,
    extra_record: serde_json::Value,
    extra_blob: Option<(&str, Vec<u8>)>,
) -> PathBuf {
    let mut manifest = read_manifest_value(project_path);
    let old_checkpoint_id = manifest["checkpoint"].as_str().unwrap().to_string();
    manifest["blobs"].as_array_mut().unwrap().push(extra_record);
    let new_checkpoint_id = format!("{name}-checkpoint");
    manifest["checkpoint"] = serde_json::json!(new_checkpoint_id);

    let mut checkpoint: serde_json::Value = serde_json::from_slice(&read_entry_bytes(
        project_path,
        &format!("checkpoints/{old_checkpoint_id}.json"),
    ))
    .unwrap();
    checkpoint["id"] = serde_json::json!(new_checkpoint_id);
    checkpoint["zmap_blob"] = serde_json::json!(target_hash);

    let mut add = vec![(
        format!("checkpoints/{new_checkpoint_id}.json"),
        serde_json::to_vec_pretty(&checkpoint).unwrap(),
    )];
    if let Some((hash, bytes)) = extra_blob {
        add.push((format!("blobs/{hash}.zst"), bytes));
    }
    let corrupt = dir.join(format!("{name}.ffp"));
    mutate_container(
        project_path,
        &corrupt,
        &[(
            "manifest.json",
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )],
        &add,
    );
    corrupt
}

#[test]
fn delta_decodes_bit_identically() {
    // The fixtures/mantis-512 golden Z-Map.
    {
        let (dir, project_path) = sealed_mantis("delta_blob_ac1_mantis");
        let mantis = common::mantis_512_dir();
        let base = read_zmap_pixels(&mantis.join("golden").join("laplacian-zmap.tif"));
        let info = ZMapInfo::new(512, 512, 8).unwrap();
        let ops = vec![Op::BrushStroke {
            source_id: 4,
            path: vec![Point { x: 256, y: 256 }],
            radius: 10.0,
            softness: 0.2,
        }];
        let expected = expected_after(&base, info, &ops);
        project::append_ops(&project_path, ops, None).expect("append_ops must succeed");
        assert_all_paths_bit_identical(&dir, &project_path, &expected);
    }

    // A 37x23 synthetic map whose edited tile touches the right and bottom
    // edges.
    {
        let (dir, project_path) = sealed("delta_blob_ac1_synthetic", 37, 23, 6);
        let base = zmap_noisy(37, 23, 6);
        let info = ZMapInfo::new(37, 23, 6).unwrap();
        let ops = vec![Op::BrushStroke {
            source_id: 5,
            path: vec![Point { x: 36, y: 22 }],
            radius: 5.0,
            softness: 0.0,
        }];
        let tile = replay::replay_tile(&ops, info).unwrap();
        assert_eq!(
            (tile.x + tile.width, tile.y + tile.height),
            (37, 23),
            "the edited tile must actually touch the right and bottom edges"
        );
        let expected = expected_after(&base, info, &ops);
        project::append_ops(&project_path, ops, None).expect("append_ops must succeed");
        assert_all_paths_bit_identical(&dir, &project_path, &expected);
    }
}

#[test]
fn kind_field_gains_a_value_without_changing_full() {
    let (_dir, project_path) = sealed("delta_blob_ac2", 64, 48, 8);
    let project = Project::open(&project_path).unwrap();
    let full_record = project
        .manifest()
        .blobs
        .iter()
        .find(|b| b.kind == BlobKind::Full)
        .unwrap();
    let json = serde_json::to_value(full_record).unwrap();
    assert_eq!(json["kind"], serde_json::json!("full"));
    assert!(
        json.get("base").is_none(),
        "a full record must not serialize a base key: {json}"
    );
    assert!(
        json.get("target").is_none(),
        "a full record must not serialize a target key: {json}"
    );

    // A record with no base/target keys at all still deserializes as
    // `Full` with both `None`.
    let bare_full_json = serde_json::json!({
        "hash": "aa", "kind": "full", "compression": "zstd",
        "uncompressed_len": 1, "compressed_len": 1,
    });
    let record: BlobRecord = serde_json::from_value(bare_full_json).unwrap();
    assert_eq!(record.kind, BlobKind::Full);
    assert_eq!(record.base, None);
    assert_eq!(record.target, None);

    // An edit that lands as a delta carries `base` and `target`, serialized
    // under the kebab-case spelling.
    let sealed_ops = project::append_ops(&project_path, vec![small_stroke(2, 10, 10)], None)
        .expect("append_ops must succeed");
    assert_eq!(
        sealed_ops.zmap.kind,
        BlobKind::SparseDelta,
        "test edit must land as a delta"
    );
    let delta_json = serde_json::to_value(&sealed_ops.zmap).unwrap();
    assert_eq!(delta_json["kind"], serde_json::json!("sparse-delta"));
    assert!(delta_json.get("base").is_some());
    assert!(delta_json.get("target").is_some());
    assert_eq!(
        sealed_ops.zmap.base.as_deref(),
        Some(project.manifest().zmap.blob.as_str())
    );
}

#[test]
fn delta_hash_is_over_uncompressed_content() {
    let (_dir, project_path) = sealed("delta_blob_ac3", 64, 48, 8);
    let sealed_ops = project::append_ops(&project_path, vec![small_stroke(2, 20, 15)], None)
        .expect("append_ops must succeed");
    let record = &sealed_ops.zmap;
    assert_eq!(
        record.kind,
        BlobKind::SparseDelta,
        "test edit must land as a delta"
    );

    // The record's hash is the entry name, re-derived independently from
    // the archived, zstd-decompressed bytes.
    let entry_bytes = read_entry_bytes(&project_path, &format!("blobs/{}.zst", record.hash));
    let mut decoder = zstd::stream::read::Decoder::new(&entry_bytes[..]).unwrap();
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes).unwrap();
    assert_eq!(blake3::hash(&bytes).to_hex().to_string(), record.hash);

    // Two zstd levels of the same patch give one hash.
    let decoded = blob::decode_delta(&bytes, &project_path).unwrap();
    let staging = tempfile::tempdir().unwrap();
    let low = blob::stage_delta(
        decoded.map_width,
        decoded.map_height,
        decoded.tile,
        &decoded.indices,
        1,
        staging.path(),
    )
    .unwrap();
    let high = blob::stage_delta(
        decoded.map_width,
        decoded.map_height,
        decoded.tile,
        &decoded.indices,
        19,
        staging.path(),
    )
    .unwrap();
    assert_eq!(low.hash, high.hash);
    assert_ne!(
        low.compressed_len, high.compressed_len,
        "the two levels must differ in compressed size"
    );

    // The checkpoint's zmap_blob is the decoded map's hash -- the record's
    // target -- never the delta blob's own hash.
    let project = Project::open(&project_path).unwrap();
    assert_eq!(
        project.checkpoint().zmap_blob,
        record.target.clone().unwrap()
    );
    assert_ne!(project.checkpoint().zmap_blob, record.hash);
}

#[test]
fn delta_is_smaller_than_the_full_blob() {
    let ops = vec![Op::BrushStroke {
        source_id: 3,
        path: vec![
            Point { x: 200, y: 220 },
            Point { x: 230, y: 205 },
            Point { x: 260, y: 225 },
            Point { x: 290, y: 210 },
            Point { x: 320, y: 220 },
        ],
        radius: 12.0,
        softness: 0.3,
    }];

    let (_dir, full_project) = sealed_mantis("delta_blob_ac4_full");
    let full_only =
        project::append_ops_at(&full_project, ops.clone(), None, 1_000, ZMapStorage::Full)
            .expect("full-storage append must succeed");
    assert_eq!(full_only.zmap.kind, BlobKind::Full);
    let full_len = full_only.zmap.compressed_len;

    let (_dir2, smallest_project) = sealed_mantis("delta_blob_ac4_smallest");
    let smallest =
        project::append_ops_at(&smallest_project, ops, None, 1_000, ZMapStorage::Smallest)
            .expect("smallest-storage append must succeed");
    assert_eq!(
        smallest.zmap.kind,
        BlobKind::SparseDelta,
        "a realistic brush edit must compress smaller as a delta"
    );
    let delta_len = smallest.zmap.compressed_len;

    println!(
        "mantis-512 brush edit: full {full_len} B, delta {delta_len} B, {:.1}x smaller",
        full_len as f64 / delta_len as f64
    );
    assert!(
        delta_len * 10 <= full_len,
        "delta {delta_len} B is not at least 10x smaller than full {full_len} B"
    );
}

#[test]
fn missing_parent_blob_is_refused() {
    let (dir, project_path) = sealed("delta_blob_ac5", 64, 48, 8);
    let target_hash = blake3::hash(b"ac5-target").to_hex().to_string();
    let base_hash = blake3::hash(b"ac5-missing-base").to_hex().to_string();
    let delta_hash = blake3::hash(b"ac5-delta").to_hex().to_string();
    let corrupt = corrupted_delta_head(
        &dir,
        "ac5-missing-parent",
        &project_path,
        &target_hash,
        serde_json::json!({
            "hash": delta_hash, "kind": "sparse-delta", "compression": "zstd",
            "uncompressed_len": 1, "compressed_len": 1,
            "base": base_hash, "target": target_hash,
        }),
        Some((delta_hash.as_str(), b"filler".to_vec())),
    );

    let err = match Project::open(&corrupt) {
        Ok(_) => panic!("a corrupted head chain must be refused"),
        Err(err) => err,
    };
    match err {
        ProjectError::PatchBaseMissing { blob, base, .. } => {
            assert_eq!(blob, delta_hash);
            assert_eq!(base, base_hash);
        }
        other => panic!("expected PatchBaseMissing, got {other:?}"),
    }
}

#[test]
fn geometry_mismatch_is_refused() {
    let (dir, project_path) = sealed("delta_blob_ac6", 64, 48, 8);
    let manifest = read_manifest_value(&project_path);
    let base_hash = manifest["zmap"]["blob"].as_str().unwrap().to_string();

    // A delta whose own header declares the wrong map geometry, with a
    // correctly computed hash -- so only the geometry check can refuse it,
    // never the hash check.
    let staging = tempfile::tempdir().unwrap();
    let wrong = blob::stage_delta(
        32,
        24,
        Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        },
        &[0u16; 16],
        blob::ZSTD_LEVEL,
        staging.path(),
    )
    .unwrap();
    let delta_bytes = std::fs::read(&wrong.path).unwrap();
    let target_hash = blake3::hash(b"ac6-target").to_hex().to_string();

    let corrupt = corrupted_delta_head(
        &dir,
        "ac6-geometry-mismatch",
        &project_path,
        &target_hash,
        serde_json::json!({
            "hash": wrong.hash, "kind": "sparse-delta", "compression": "zstd",
            "uncompressed_len": wrong.uncompressed_len, "compressed_len": wrong.compressed_len,
            "base": base_hash, "target": target_hash,
        }),
        Some((&wrong.hash, delta_bytes)),
    );

    let mut project = Project::open(&corrupt).unwrap();
    let out = dir.join("geometry-mismatch-out.tif");
    let err = project::materialize_plane(&mut project, Plane::ZMap, &out).unwrap_err();
    match err {
        ProjectError::PatchGeometryMismatch {
            found_width,
            found_height,
            expected_width,
            expected_height,
            ..
        } => {
            assert_eq!((found_width, found_height), (32, 24));
            assert_eq!((expected_width, expected_height), (64, 48));
        }
        other => panic!("expected PatchGeometryMismatch, got {other:?}"),
    }
    assert!(!out.exists(), "no output file may be left");
    assert!(!partial_path(&out).exists(), "no .partial may be left");
}

#[test]
fn delta_is_against_the_materialized_parent() {
    let (dir_a, project_a) = sealed("delta_blob_ac7_full_first", 64, 48, 8);
    let dir_b = tmp_dir("delta_blob_ac7_delta_first");
    let project_b = dir_b.join("copy.ffp");
    std::fs::copy(&project_a, &project_b).unwrap();

    let ops_a = vec![small_stroke(2, 10, 10)];
    let ops_b = vec![small_stroke(5, 50, 5)];
    let created_middle = 1_700_000_000;
    let created_last = 1_700_000_100;

    let middle_full = project::append_ops_at(
        &project_a,
        ops_a.clone(),
        None,
        created_middle,
        ZMapStorage::Full,
    )
    .expect("full-storage middle append must succeed");
    let last_after_full = project::append_ops_at(
        &project_a,
        ops_b.clone(),
        None,
        created_last,
        ZMapStorage::Smallest,
    )
    .expect("delta-storage last append must succeed");

    let middle_delta = project::append_ops_at(
        &project_b,
        ops_a,
        None,
        created_middle,
        ZMapStorage::Smallest,
    )
    .expect("delta-storage middle append must succeed");
    let last_after_delta =
        project::append_ops_at(&project_b, ops_b, None, created_last, ZMapStorage::Smallest)
            .expect("delta-storage last append must succeed");

    assert_eq!(
        middle_full.checkpoint, middle_delta.checkpoint,
        "the same edit must derive the same checkpoint id regardless of how its parent is stored"
    );
    assert_eq!(last_after_full.checkpoint, last_after_delta.checkpoint);
    assert_eq!(
        last_after_full.zmap.kind,
        BlobKind::SparseDelta,
        "test edit must land as a delta to exercise this decision"
    );
    assert_eq!(last_after_full.zmap, last_after_delta.zmap);

    let mut pa = Project::open(&project_a).unwrap();
    let mut pb = Project::open(&project_b).unwrap();
    let out_a = dir_a.join("last-a.tif");
    let out_b = dir_b.join("last-b.tif");
    project::materialize_plane(&mut pa, Plane::ZMap, &out_a).unwrap();
    project::materialize_plane(&mut pb, Plane::ZMap, &out_b).unwrap();
    assert_eq!(read_zmap_pixels(&out_a), read_zmap_pixels(&out_b));
}

#[test]
fn whole_map_change_falls_back_to_full() {
    let (dir, project_path) = sealed("delta_blob_ac8", 64, 48, 8);
    let ops = vec![Op::BrushStroke {
        source_id: 7,
        path: vec![Point { x: 32, y: 24 }],
        radius: 100.0,
        softness: 0.0,
    }];
    let sealed_ops =
        project::append_ops(&project_path, ops, None).expect("append_ops must succeed");
    assert_eq!(
        sealed_ops.zmap.kind,
        BlobKind::Full,
        "a whole-map repaint's delta cannot beat the full blob"
    );

    let mut project = Project::open(&project_path).unwrap();
    let out = dir.join("whole-map-out.tif");
    project::materialize_plane(&mut project, Plane::ZMap, &out).unwrap();
    let pixels = read_zmap_pixels(&out);
    assert!(
        pixels.iter().all(|&v| v == 7),
        "every pixel must have been repainted"
    );
}
