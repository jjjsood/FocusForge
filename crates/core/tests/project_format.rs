// SPDX-License-Identifier: GPL-3.0-or-later

//! The behavioural suite over a real project container: the exact entry
//! layout, the payload round-trip against the bytes fusion produced, the
//! failed-seal cleanup, a tampered blob refused on read, sources referenced
//! rather than embedded, container reproducibility and the Z-Map blob's
//! measured compression.
//!
//! Every one of these pins an on-disk rule of the format: each of the four
//! format commitments gets a test here.

mod common;

use std::path::{Path, PathBuf};

use focusforge_core::io::{Rect, TiffSpec, TiffWriter};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::project::container::{ContainerWriter, Index};
use focusforge_core::project::{
    self, BlobKind, Checkpoint, HASH_ALGORITHM, Metrics, Op, Plane, Point, Project, SealRequest,
    SourceResolution, blob,
};
use focusforge_core::zmap::{ZMapInfo, ZMapReader, ZMapWriter};

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

/// Deterministic composite samples: a smooth horizontal ramp, repeated on
/// every row. Photographic planes are not this compressible, but a plane
/// that compresses to nothing at all would make
/// `sources_are_referenced_not_embedded`'s size comparison meaningless, and
/// a random plane would make the zstd level unobservable.
fn composite_samples() -> Vec<u16> {
    (0..(W as usize * H as usize * SPP as usize))
        .map(|i| {
            let x = (i / SPP as usize) % W as usize;
            let channel = i % SPP as usize;
            ((x * 65_535 / W as usize) as u16).wrapping_add(channel as u16 * 97)
        })
        .collect()
}

/// A piecewise-constant Z-Map -- horizontal bands of one source index --
/// the shape zstd is expected to compress by orders of magnitude.
fn zmap_samples() -> Vec<u16> {
    (0..(W as usize * H as usize))
        .map(|i| ((i / (W as usize * 4)) % FRAMES as usize) as u16)
        .collect()
}

/// Writes one composite TIFF and one Z-Map TIFF into `dir`, exactly as
/// `FusionConsumer` stages them.
fn stage_planes(dir: &Path) -> (PathBuf, PathBuf) {
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
    writer.write_tile(full(W, H), &composite_samples()).unwrap();
    writer.finish().unwrap();

    let mut map = ZMapWriter::create(&zmap, ZMapInfo::new(W, H, FRAMES).unwrap()).unwrap();
    map.write_tile(full(W, H), &zmap_samples()).unwrap();
    map.finish().unwrap();
    (composite, zmap)
}

/// `count` distinct source frames inside `dir`, so a project written into
/// `dir` records them as descendants.
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
        registration: focusforge_core::align::Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    }
}

/// One sealed project in a fresh directory: the sources, the two staged
/// planes and the container all live inside it, so the source paths are
/// descendants of the project's own directory.
fn sealed(name: &str) -> (PathBuf, PathBuf) {
    let dir = tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging);
    let sources = write_sources(&dir, 3);
    let values = params();
    let project_path = dir.join(format!("demo.{}", project::PROJECT_EXTENSION));
    project::seal(request(&project_path, &composite, &zmap, &sources, &values))
        .expect("seal must succeed");
    (dir, project_path)
}

fn entry_names(path: &Path) -> Vec<String> {
    Index::read(path)
        .unwrap()
        .names()
        .map(str::to_string)
        .collect()
}

/// Copies `src` to `dst`, replacing one entry's bytes -- an independent
/// rewrite of a real container, so a test can produce the corrupt or
/// future-dated project the read path is supposed to refuse without the
/// writer growing a hook that exists only for tests.
fn rewrite_entry(src: &Path, dst: &Path, entry: &str, replace: impl Fn(&[u8]) -> Vec<u8>) {
    let index = Index::read(src).unwrap();
    let mut writer = ContainerWriter::create(dst).unwrap();
    for name in index.names() {
        let mut bytes = index.read_entry(src, name).unwrap();
        if name == entry {
            bytes = replace(&bytes);
        }
        writer.put_bytes(name, &bytes).unwrap();
    }
    writer.finish().unwrap();
}

#[test]
fn container_layout_is_exactly_the_named_entries() {
    let (_dir, project_path) = sealed("project_format_layout");
    let names = entry_names(&project_path);
    assert!(
        names.contains(&"manifest.json".to_string()),
        "no manifest.json in {names:?}"
    );
    let blobs: Vec<&String> = names.iter().filter(|n| n.starts_with("blobs/")).collect();
    assert!(!blobs.is_empty(), "no blob entries in {names:?}");
    assert!(
        blobs.iter().all(|n| n.ends_with(".zst")),
        "a blob entry is not .zst: {blobs:?}"
    );
    let checkpoints: Vec<&String> = names
        .iter()
        .filter(|n| n.starts_with("checkpoints/"))
        .collect();
    assert_eq!(
        checkpoints.len(),
        1,
        "exactly one checkpoint expected, got {checkpoints:?}"
    );
    assert!(checkpoints[0].ends_with(".json"));
    for name in &names {
        assert!(
            name == "manifest.json"
                || name.starts_with("blobs/")
                || name.starts_with("checkpoints/"),
            "entry outside the three named paths: {name}"
        );
    }
}

#[test]
fn manifest_carries_no_format_version() {
    let (_dir, project_path) = sealed("project_format_no_version");
    let bytes = Index::read(&project_path)
        .unwrap()
        .read_entry(&project_path, "manifest.json")
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(
        value.get("format_version").is_none(),
        "the manifest must carry no version key: {text}"
    );
    assert_eq!(value["hash_algorithm"], serde_json::json!(HASH_ALGORITHM));
}

#[test]
fn blob_kind_is_recorded_and_required() {
    let (_dir, project_path) = sealed("project_format_blob_kind");
    let project = Project::open(&project_path).unwrap();
    assert!(!project.manifest().blobs.is_empty());
    for blob in &project.manifest().blobs {
        assert_eq!(blob.kind, BlobKind::Full);
        assert_eq!(blob.compression, "zstd");
    }
    let json = serde_json::to_value(&project.manifest().blobs[0]).unwrap();
    assert_eq!(json["kind"], serde_json::json!("full"));

    // A record missing the field is refused, never defaulted.
    let mut without = json.clone();
    without.as_object_mut().unwrap().remove("kind");
    let err = serde_json::from_value::<project::BlobRecord>(without).unwrap_err();
    assert!(
        err.to_string().contains("kind"),
        "refusal must name the missing field: {err}"
    );
}

#[test]
fn hash_is_over_uncompressed_content() {
    let dir = tmp_dir("project_format_hash_levels");
    let (composite, _zmap) = stage_planes(&dir);
    let low = blob::stage_plane(&composite, Plane::Composite, 1, &dir).unwrap();
    let high = blob::stage_plane(&composite, Plane::Composite, 22, &dir).unwrap();
    assert_eq!(
        low.hash, high.hash,
        "a different zstd level must not change the content hash"
    );
    assert_ne!(
        low.compressed_len, high.compressed_len,
        "the two levels must genuinely have produced different compressed sizes"
    );
    assert_eq!(low.uncompressed_len, high.uncompressed_len);

    // The named algorithm, asserted rather than implied.
    assert_eq!(HASH_ALGORITHM, "blake3");
    let bytes: Vec<u8> = composite_samples()
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect();
    assert_eq!(low.hash, blake3::hash(&bytes).to_hex().to_string());
}

#[test]
fn checkpoint_serializes_every_field_with_no_parent_and_no_ops() {
    let (_dir, project_path) = sealed("project_format_checkpoint");
    let project = Project::open(&project_path).unwrap();
    let checkpoint = project.checkpoint();
    assert_eq!(checkpoint.parent, None);
    assert!(checkpoint.ops.is_empty());
    assert_eq!(checkpoint.id, project.manifest().checkpoint);
    assert_eq!(checkpoint.zmap_blob, project.manifest().zmap.blob);

    let json = serde_json::to_value(checkpoint).unwrap();
    for field in [
        "id",
        "parent",
        "ops",
        "zmap_blob",
        "stack_fingerprint",
        "algorithm",
        "params",
        "created_at",
        "label",
        "fuse",
        "hidden",
    ] {
        assert!(
            json.get(field).is_some(),
            "checkpoint is missing {field}: {json}"
        );
    }
    assert_eq!(json["parent"], serde_json::Value::Null);
    assert_eq!(json["ops"], serde_json::json!([]));
}

#[test]
fn the_brush_stroke_round_trips() {
    let ops = vec![Op::BrushStroke {
        source_id: 7,
        path: vec![Point { x: -3, y: 4 }, Point { x: 10, y: 20 }],
        radius: 12.5,
        softness: 0.25,
    }];
    let text = serde_json::to_string(&ops).unwrap();
    let back: Vec<Op> = serde_json::from_str(&text).unwrap();
    assert_eq!(back, ops);
    assert!(
        text.contains("brush-stroke"),
        "externally-tagged brush-stroke missing: {text}"
    );
}

#[test]
fn op_with_a_missing_field_is_refused() {
    let err =
        serde_json::from_str::<Op>(r#"{"brush-stroke":{"source_id":1,"path":[],"radius":2.0}}"#)
            .unwrap_err();
    assert!(err.to_string().contains("softness"), "{err}");
    let err =
        serde_json::from_str::<Op>(r#"{"brush-stroke":{"path":[],"radius":2.0,"softness":0.5}}"#)
            .unwrap_err();
    assert!(err.to_string().contains("source_id"), "{err}");
}

#[test]
fn a_dropped_op_tag_is_an_unknown_variant() {
    for tag in ["flood-fill", "reset-region", "refuse"] {
        let err = serde_json::from_str::<Op>(&format!(r#"{{"{tag}":{{}}}}"#)).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("unknown variant") && message.contains(tag),
            "the refusal of `{tag}` must name it as an unknown variant: {message}"
        );
    }
}

#[test]
fn payloads_round_trip_bit_identically() {
    let (_dir, project_path) = sealed("project_format_round_trip");
    let mut project = Project::open(&project_path).unwrap();

    let mut composite = vec![0u16; W as usize * H as usize * SPP as usize];
    project
        .read_plane_into(Plane::Composite, &mut |band, rect| {
            let start = rect.y as usize * W as usize * SPP as usize;
            composite[start..start + band.len()].copy_from_slice(band);
        })
        .unwrap();
    assert_eq!(composite, composite_samples());

    let mut zmap = vec![0u16; W as usize * H as usize];
    project
        .read_plane_into(Plane::ZMap, &mut |band, rect| {
            let start = rect.y as usize * W as usize;
            zmap[start..start + band.len()].copy_from_slice(band);
        })
        .unwrap();
    assert_eq!(zmap, zmap_samples());

    let plane = &project.manifest().composite;
    assert_eq!((plane.width, plane.height), (W, H));
    assert_eq!(plane.samples_per_pixel, SPP);
    assert_eq!(plane.description, "staged composite");
    assert_eq!(project.manifest().zmap.frame_count, FRAMES);
}

#[test]
fn failed_write_leaves_neither_project_nor_temp() {
    let dir = tmp_dir("project_format_failed_seal");
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging);
    let sources = write_sources(&dir, 2);
    let values = params();
    let project_path = dir.join(format!("demo.{}", project::PROJECT_EXTENSION));

    // A staged Z-Map that is not there is the cheapest genuine mid-seal
    // failure: the composite blob is already written when it is hit.
    std::fs::remove_file(&zmap).unwrap();
    let err = project::seal(request(&project_path, &composite, &zmap, &sources, &values))
        .expect_err("seal must fail when a staged plane is missing");
    assert!(
        err.to_string().contains("zmap.tif"),
        "the refusal must name the missing plane: {err}"
    );
    assert!(!project_path.exists(), "a project was left behind");
    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("partial") || n.ends_with(project::PROJECT_EXTENSION))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

#[test]
fn sources_are_referenced_not_embedded() {
    let (dir, project_path) = sealed("project_format_sources");
    let project = Project::open(&project_path).unwrap();
    let sources = &project.manifest().sources;
    assert_eq!(sources.len(), 3);
    for source in sources {
        assert_eq!(source.resolution, SourceResolution::Relative);
        assert!(
            !Path::new(&source.path).is_absolute(),
            "a descendant source must be recorded relative: {}",
            source.path
        );
        let on_disk = dir.join(&source.path);
        let bytes = std::fs::read(&on_disk).unwrap();
        assert_eq!(source.bytes, bytes.len() as u64);
        assert_eq!(source.hash, blake3::hash(&bytes).to_hex().to_string());
    }

    // Nothing of the frames themselves is in the container.
    let frame_len = std::fs::metadata(dir.join(&sources[0].path)).unwrap().len();
    let container_len = std::fs::metadata(&project_path).unwrap().len();
    assert!(
        container_len < frame_len,
        "container ({container_len} bytes) is not smaller than one frame ({frame_len} bytes)"
    );

    // The fingerprint is a real content hash over the frame list, not the
    // resume ledger's cheap header-only one.
    assert_eq!(project.manifest().stack_fingerprint.len(), 64);
    assert_eq!(
        project.manifest().stack_fingerprint,
        project.checkpoint().stack_fingerprint
    );
}

#[test]
fn unresolvable_source_is_recorded_as_given_and_reported_on_open() {
    let dir = tmp_dir("project_format_as_given");
    let frames = dir.join("frames");
    let out = dir.join("out");
    std::fs::create_dir_all(&frames).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging);
    let sources = write_sources(&frames, 2);
    let values = params();
    let project_path = out.join(format!("demo.{}", project::PROJECT_EXTENSION));
    project::seal(request(&project_path, &composite, &zmap, &sources, &values)).unwrap();

    let project = Project::open(&project_path).unwrap();
    let unresolved: Vec<&str> = project
        .unresolved_sources()
        .map(|s| s.path.as_str())
        .collect();
    assert_eq!(
        unresolved.len(),
        2,
        "every source outside the project's own directory is unresolved"
    );
    for path in &unresolved {
        assert!(
            Path::new(path).is_absolute(),
            "an as-given source is recorded absolute: {path}"
        );
    }
    for source in &project.manifest().sources {
        assert_eq!(source.resolution, SourceResolution::AsGiven);
    }
    assert_eq!(
        serde_json::to_value(&project.manifest().sources[0]).unwrap()["resolution"],
        serde_json::json!("as-given")
    );
}

/// The other half of the unresolved-source state, and the one a user hits
/// first: a project copied away from its frames records **relative** paths
/// that simply are not there any more.
#[test]
fn a_moved_project_reports_its_relative_sources_as_unresolved() {
    let (dir, project_path) = sealed("project_format_moved");

    // In place, every source resolves and nothing is reported.
    let project = Project::open(&project_path).unwrap();
    assert_eq!(project.unresolved_sources().count(), 0);
    assert!(
        project
            .source_path(&project.manifest().sources[0])
            .is_file()
    );
    drop(project);

    // Copied one directory away -- the frames stay behind.
    let moved_dir = dir.join("moved");
    std::fs::create_dir_all(&moved_dir).unwrap();
    let moved = moved_dir.join(project_path.file_name().unwrap());
    std::fs::copy(&project_path, &moved).unwrap();

    let project = Project::open(&moved).unwrap();
    let unresolved: Vec<&str> = project
        .unresolved_sources()
        .map(|s| s.path.as_str())
        .collect();
    assert_eq!(
        unresolved.len(),
        3,
        "every source must be reported by name: {unresolved:?}"
    );
    // Still recorded relative -- what changed is that the path does not
    // resolve from here, not how it was written.
    for source in &project.manifest().sources {
        assert_eq!(source.resolution, SourceResolution::Relative);
    }
}

#[test]
fn a_tampered_blob_is_refused_on_read() {
    let (_dir, project_path) = sealed("project_format_tampered");
    let tampered = project_path.with_extension("tampered.ffp");
    let composite_blob = Project::open(&project_path)
        .unwrap()
        .plane(Plane::Composite)
        .blob
        .clone();
    rewrite_entry(
        &project_path,
        &tampered,
        &format!("blobs/{composite_blob}.zst"),
        |bytes| {
            // Truncating the zstd frame would fail the read for the wrong
            // reason; re-encoding different pixels is what a bit-rotted or
            // maliciously edited blob actually looks like.
            let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
            use std::io::Write;
            let corrupt: Vec<u8> = composite_samples()
                .iter()
                .enumerate()
                .flat_map(|(i, s)| (s ^ if i == 0 { 0xffff } else { 0 }).to_le_bytes())
                .collect();
            encoder.write_all(&corrupt).unwrap();
            let _ = bytes;
            encoder.finish().unwrap()
        },
    );

    let mut project = Project::open(&tampered).unwrap();
    let err = project
        .read_plane_into(Plane::Composite, &mut |_, _| {})
        .expect_err("a tampered blob must be refused");
    let text = err.to_string();
    assert!(
        text.contains("hash") || text.contains("blake3"),
        "the refusal must name the hash mismatch: {text}"
    );
}

#[test]
fn zmap_blob_is_compressed() {
    let (_dir, project_path) = sealed("project_format_zmap_compression");
    let project = Project::open(&project_path).unwrap();
    let zmap_hash = &project.manifest().zmap.blob;
    let record = project
        .manifest()
        .blobs
        .iter()
        .find(|b| &b.hash == zmap_hash)
        .expect("the Z-Map blob is recorded");
    let raw = W as u64 * H as u64 * 2;
    assert_eq!(record.uncompressed_len, raw);
    assert!(
        record.compressed_len < raw,
        "the Z-Map blob ({} bytes) is not smaller than the raw map ({raw} bytes)",
        record.compressed_len
    );
    println!(
        "zmap blob: {} -> {} bytes ({:.1}x)",
        raw,
        record.compressed_len,
        raw as f64 / record.compressed_len as f64
    );
}

#[test]
fn the_container_is_reproducible() {
    let dir = tmp_dir("project_format_reproducible");
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging);
    let sources = write_sources(&dir, 2);
    let values = params();

    let seal_one = |name: &str| {
        let path = dir.join(name);
        project::seal(request(&path, &composite, &zmap, &sources, &values)).unwrap();
        std::fs::read(&path).unwrap()
    };
    let a = seal_one("a.ffp");
    let b = seal_one("b.ffp");
    assert_eq!(
        a.len(),
        b.len(),
        "two seals of the same inputs differ in length"
    );

    // `created_at` is the one field that legitimately differs, and both
    // copies of it are the only bytes allowed to. Re-sealing with a pinned
    // clock is what makes the comparison exact.
    let c = dir.join("c.ffp");
    let d = dir.join("d.ffp");
    project::seal_at(
        request(&c, &composite, &zmap, &sources, &values),
        1_700_000_000,
    )
    .unwrap();
    project::seal_at(
        request(&d, &composite, &zmap, &sources, &values),
        1_700_000_000,
    )
    .unwrap();
    assert_eq!(
        std::fs::read(&c).unwrap(),
        std::fs::read(&d).unwrap(),
        "two seals at the same instant are not byte-identical"
    );
}

#[test]
fn registration_missing_from_the_manifest_is_refused() {
    let (_dir, project_path) = sealed("project_format_registration_required");
    let without = project_path.with_extension("no-registration.ffp");
    rewrite_entry(&project_path, &without, "manifest.json", |bytes| {
        let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("registration")
            .expect("the sealed manifest must carry a registration key to remove");
        serde_json::to_vec_pretty(&value).unwrap()
    });
    let err = match Project::open(&without) {
        Ok(_) => panic!("a manifest without registration must be refused"),
        Err(err) => err,
    };
    assert!(
        matches!(&err, project::ProjectError::Container { .. }),
        "expected a container error: {err:?}"
    );
    let text = err.to_string();
    assert!(
        text.contains("manifest.json") && text.contains("registration"),
        "the refusal must name the manifest and the field: {text}"
    );
}

/// zstd compressing a piecewise-constant Z-Map by one to two orders of
/// magnitude, measured at the reference scale (44.8 MP, 89.6 MB raw)
/// rather than at 64x48.
#[test]
#[ignore = "builds an 89.6 MB Z-Map; minutes, not seconds"]
fn zmap_blob_compression_over_the_reference_scale() {
    let dir = tmp_dir("project_format_reference_zmap");
    let (width, height, frames) = (8192u32, 5464u32, 61u32);
    let path = dir.join("zmap.tif");
    let mut writer = ZMapWriter::create(&path, ZMapInfo::new(width, height, frames).unwrap())
        .expect("z-map writer");
    // Coherent, piecewise-constant bands -- a de-speckled map, not a
    // per-pixel random one.
    let mut row = vec![0u16; width as usize];
    for y in 0..height {
        let index = ((y / 64) % frames) as u16;
        row.fill(index);
        writer
            .write_tile(
                Rect {
                    x: 0,
                    y,
                    width,
                    height: 1,
                },
                &row,
            )
            .unwrap();
    }
    writer.finish().unwrap();

    let staged = blob::stage_plane(&path, Plane::ZMap, 3, &dir).unwrap();
    let raw = width as u64 * height as u64 * 2;
    println!(
        "reference-scale Z-Map: {} -> {} bytes ({:.1}x)",
        raw,
        staged.compressed_len,
        raw as f64 / staged.compressed_len as f64
    );
    assert_eq!(staged.uncompressed_len, raw);
    assert!(
        staged.compressed_len * 10 < raw,
        "less than one order of magnitude: {} vs {raw}",
        staged.compressed_len
    );
}

#[test]
fn checkpoint_id_is_deterministic() {
    let id = |created_at: i64| Checkpoint::derive_id("fingerprint", "zmapblob", created_at);
    assert_eq!(id(10), id(10));
    assert_ne!(id(10), id(11));
    assert_eq!(id(10).len(), 32);
}

#[test]
fn multithreaded_blobs_round_trip() {
    let dir = tmp_dir("project_format_multithreaded_blobs");
    let mantis = common::mantis_512_dir();
    let golden = mantis.join("golden");
    // Copied so `seal` stages its blobs beside private files, not the fixture.
    let composite_path = dir.join("golden-composite.tif");
    std::fs::copy(golden.join("laplacian.tif"), &composite_path).unwrap();
    let zmap_path = dir.join("golden-zmap.tif");
    std::fs::copy(golden.join("laplacian-zmap.tif"), &zmap_path).unwrap();
    let sources: Vec<PathBuf> = (0..8)
        .map(|i| mantis.join("frames").join(format!("{i:02}.tif")))
        .collect();
    let values = params();
    let project_path = dir.join(format!("demo.{}", project::PROJECT_EXTENSION));
    project::seal(request(
        &project_path,
        &composite_path,
        &zmap_path,
        &sources,
        &values,
    ))
    .expect("seal must succeed");

    // Every blob the project holds decodes to bytes that hash to its own
    // entry name.
    let project = Project::open(&project_path).unwrap();
    let records = &project.manifest().blobs;
    assert!(!records.is_empty(), "the project must hold blobs");
    for record in records {
        let stored = Index::read(&project_path)
            .unwrap()
            .read_entry(&project_path, &format!("blobs/{}.zst", record.hash))
            .unwrap();
        let bytes = zstd::stream::decode_all(&stored[..]).unwrap();
        assert_eq!(bytes.len() as u64, record.uncompressed_len);
        assert_eq!(blake3::hash(&bytes).to_hex().to_string(), record.hash);
    }

    // The same plane staged at two levels: one entry name, and the plane's
    // own little-endian bytes back from either file.
    let le =
        |samples: &[u16]| -> Vec<u8> { samples.iter().flat_map(|s| s.to_le_bytes()).collect() };
    let stack =
        focusforge_core::io::TiffStack::open_files(std::slice::from_ref(&composite_path)).unwrap();
    let (w, h) = stack.dimensions();
    let mut composite = vec![0u16; w as usize * h as usize * SPP as usize];
    stack.read_tile_into(0, full(w, h), &mut composite).unwrap();
    let reader = ZMapReader::open(&zmap_path).unwrap();
    let (zw, zh) = reader.dimensions();
    let mut zmap = vec![0u16; zw as usize * zh as usize];
    reader.read_tile_into(full(zw, zh), &mut zmap).unwrap();
    let planes = [
        (Plane::Composite, composite_path, le(&composite)),
        (Plane::ZMap, zmap_path, le(&zmap)),
    ];
    for (plane, source, expected) in planes {
        let mut hashes = Vec::new();
        for level in [1, blob::ZSTD_LEVEL] {
            let out = dir.join(format!("staged-{plane:?}-{level}"));
            std::fs::create_dir_all(&out).unwrap();
            let staged = blob::stage_plane(&source, plane, level, &out).unwrap();
            let stored = std::fs::read(&staged.path).unwrap();
            assert_eq!(zstd::stream::decode_all(&stored[..]).unwrap(), expected);
            hashes.push(staged.hash);
        }
        assert_eq!(
            hashes[0], hashes[1],
            "{plane:?}: the level moved the entry name"
        );
        assert_eq!(hashes[0], blake3::hash(&expected).to_hex().to_string());
    }
}
