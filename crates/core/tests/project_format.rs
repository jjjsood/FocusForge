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
    self, BlobKind, BrushMode, Checkpoint, HASH_ALGORITHM, Metrics, Op, Plane, Point, Project,
    SealRequest, SourceResolution, StrokeSource, blob,
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

fn manifest_value(path: &Path) -> serde_json::Value {
    let bytes = Index::read(path)
        .unwrap()
        .read_entry(path, "manifest.json")
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn stored_format_version(path: &Path) -> Option<serde_json::Value> {
    manifest_value(path).get("format_version").cloned()
}

#[test]
fn manifest_carries_no_format_version() {
    let (_dir, project_path) = sealed("project_format_no_version");
    let value = manifest_value(&project_path);
    assert!(
        value.get("format_version").is_none(),
        "the sealed manifest must carry no version key: {value}"
    );
    assert_eq!(value["hash_algorithm"], serde_json::json!(HASH_ALGORITHM));

    project::append_ops(
        &project_path,
        vec![Op::frame_stroke(0, vec![Point { x: 1, y: 1 }], 2.0, 0.5)],
        None,
    )
    .unwrap();
    let saved = manifest_value(&project_path);
    assert!(
        saved.get("format_version").is_none(),
        "a saved manifest must carry no version key: {saved}"
    );
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
    let ops = vec![Op::frame_stroke(
        7,
        vec![Point { x: -3, y: 4 }, Point { x: 10, y: 20 }],
        12.5,
        0.25,
    )];
    let text = serde_json::to_string(&ops).unwrap();
    let back: Vec<Op> = serde_json::from_str(&text).unwrap();
    assert_eq!(back, ops);
    assert!(
        text.contains("brush-stroke"),
        "externally-tagged brush-stroke missing: {text}"
    );
}

/// The stroke list in `fixtures/retouch/stroke-list.json`, whose strokes carry
/// neither a mode nor an opacity.
fn stroke_list_fixture() -> String {
    std::fs::read_to_string(common::fixtures_dir().join("retouch/stroke-list.json"))
        .expect("the stroke list fixture must read")
}

fn stroke(mode: BrushMode, opacity: f32) -> Op {
    Op::BrushStroke {
        source: StrokeSource::Frame(3),
        path: vec![Point { x: -4, y: 9 }, Point { x: 30, y: -2 }],
        radius: 6.5,
        softness: 0.0,
        mode,
        opacity,
    }
}

/// Every stroke shape that needs format 2.
fn non_default_strokes() -> [Op; 5] {
    [
        stroke(BrushMode::Lighten, 1.0),
        stroke(BrushMode::Darken, 1.0),
        stroke(BrushMode::Detail, 1.0),
        stroke(BrushMode::Normal, 0.5),
        stroke(BrushMode::Normal, 1.0 - f32::EPSILON / 2.0),
    ]
}

fn required_version(path: &Path) -> u32 {
    project::required_format_version(Project::open(path).unwrap().history())
}

#[test]
fn required_format_version_is_two_for_a_listed_non_default_stroke() {
    let (_dir, path) = sealed("project_format_required_fold");
    assert_eq!(required_version(&path), 1, "a sealed project");
    project::append_ops(&path, vec![stroke(BrushMode::Normal, 1.0)], None).unwrap();
    assert_eq!(required_version(&path), 1, "a default stroke");
    for (i, op) in non_default_strokes().into_iter().enumerate() {
        let (_dir, other) = sealed(&format!("project_format_required_fold_{i}"));
        project::append_ops(&other, vec![op.clone()], None).unwrap();
        assert_eq!(required_version(&other), 2, "{op:?}");
        // A default stroke after it leaves the earlier checkpoint listed.
        project::append_ops(&other, vec![stroke(BrushMode::Normal, 1.0)], None).unwrap();
        assert_eq!(required_version(&other), 2, "{op:?} then a default stroke");
    }
}

#[test]
fn required_format_version_never_lowers_a_stored_version() {
    let (_dir, path) = sealed("project_format_required_monotone");
    let default = || vec![stroke(BrushMode::Normal, 1.0)];

    let at_two = with_manifest(&path, "stored-two", |m| {
        m.insert("format_version".into(), serde_json::json!(2));
    });
    project::append_ops(&at_two, default(), None).unwrap();
    assert_eq!(stored_format_version(&at_two), Some(serde_json::json!(2)));

    for (name, stored) in [("stored-zero", 0), ("stored-one", 1)] {
        let copy = with_manifest(&path, name, |m| {
            m.insert("format_version".into(), serde_json::json!(stored));
        });
        project::append_ops(&copy, default(), None).unwrap();
        assert_eq!(
            stored_format_version(&copy),
            Some(serde_json::json!(stored)),
            "{name}: a default-only save rewrites nothing"
        );
        project::append_ops(&copy, vec![stroke(BrushMode::Detail, 1.0)], None).unwrap();
        assert_eq!(
            stored_format_version(&copy),
            Some(serde_json::json!(2)),
            "{name}: a non-default stroke raises it"
        );
    }
}

#[test]
fn required_format_version_two_opens_and_three_is_refused() {
    let (_dir, path) = sealed("project_format_required_supported");
    project::append_ops(&path, vec![stroke(BrushMode::Lighten, 0.5)], None).unwrap();
    let head = Project::open(&path).unwrap().manifest().checkpoint.clone();
    assert_eq!(stored_format_version(&path), Some(serde_json::json!(2)));
    assert_eq!(Project::open(&path).unwrap().manifest().checkpoint, head);

    let newer = with_manifest(&path, "v3", |m| {
        m.insert("format_version".into(), serde_json::json!(3));
    });
    assert!(matches!(
        open_err(&newer),
        project::ProjectError::NewerFormat {
            found: 3,
            supported: 2,
            ..
        }
    ));
}

#[test]
fn format_version_stamp_marks_only_non_default_strokes() {
    let (_dir, path) = sealed("project_format_stamp_default");
    let keys = |p: &Path| -> Vec<String> {
        manifest_value(p)
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    };
    let before = keys(&path);
    project::append_ops(&path, vec![stroke(BrushMode::Normal, 1.0)], None).unwrap();
    assert_eq!(
        stored_format_version(&path),
        None,
        "default strokes: no field"
    );
    assert_eq!(keys(&path), before, "the default manifest keeps its keys");

    for (i, op) in non_default_strokes().into_iter().enumerate() {
        let (_dir, other) = sealed(&format!("project_format_stamp_{i}"));
        project::append_ops(&other, vec![op.clone()], None).unwrap();
        assert_eq!(
            stored_format_version(&other),
            Some(serde_json::json!(2)),
            "{op:?}"
        );
    }
}

#[test]
fn brush_stroke_mode_opacity_absent_fields_read_as_defaults() {
    let default = stroke(BrushMode::Normal, 1.0);

    let literal: Vec<Op> = serde_json::from_str(
        r#"[{"brush-stroke":{"source_id":3,"path":[{"x":-4,"y":9},{"x":30,"y":-2}],"radius":6.5,"softness":0}}]"#,
    )
    .unwrap();
    assert_eq!(literal, vec![default.clone()]);

    let fixture: Vec<Op> = serde_json::from_str(&stroke_list_fixture()).unwrap();
    assert_eq!(fixture.len(), 3);
    for op in &fixture {
        assert!(
            matches!(op, Op::BrushStroke { mode: BrushMode::Normal, opacity, .. } if *opacity == 1.0),
            "a fixture stroke without the fields must read normal / 1.0: {op:?}"
        );
    }

    let (_dir, project_path) = sealed("project_format_stroke_defaults");
    let appended = project::append_ops(&project_path, vec![default.clone()], None).unwrap();
    let entry = format!("checkpoints/{}.json", appended.checkpoint);
    let raw: serde_json::Value = serde_json::from_slice(
        &Index::read(&project_path)
            .unwrap()
            .read_entry(&project_path, &entry)
            .unwrap(),
    )
    .unwrap();
    let written = raw["ops"][0]["brush-stroke"].as_object().unwrap();
    assert!(
        !written.contains_key("mode") && !written.contains_key("opacity"),
        "a default stroke's checkpoint entry carries neither key: {raw}"
    );
    let reopened = Project::open(&project_path).unwrap();
    assert_eq!(reopened.checkpoint().ops, vec![default]);
}

#[test]
fn brush_stroke_mode_opacity_default_keeps_its_bytes_and_id() {
    let without_fields = r#"[{"brush-stroke":{"source_id":2,"path":[{"x":120,"y":256},{"x":380,"y":256}],"radius":18.0,"softness":0.3}}]"#;
    let ops: Vec<Op> = serde_json::from_str(without_fields).unwrap();
    let bytes = serde_json::to_vec(&ops).unwrap();
    assert_eq!(String::from_utf8(bytes.clone()).unwrap(), without_fields);

    let id = |ops_json: &[u8]| {
        Checkpoint::derive_child_id("parent", "fingerprint", &blake3::hash(ops_json).to_hex(), 7)
    };
    assert_eq!(id(&bytes), id(without_fields.as_bytes()));
}

fn sourced_stroke(source: StrokeSource, points: usize, mode: BrushMode, opacity: f32) -> Op {
    Op::BrushStroke {
        source,
        path: (0..points as i32)
            .map(|i| Point { x: i * 3, y: -i })
            .collect(),
        radius: 4.0,
        softness: 0.75,
        mode,
        opacity,
    }
}

#[test]
fn stroke_source_round_trips_both_variants() {
    let sources = [
        StrokeSource::Frame(0),
        StrokeSource::Frame(u16::MAX),
        StrokeSource::Checkpoint("0123456789abcdef0123456789abcdef".into()),
        StrokeSource::Checkpoint(String::new()),
    ];
    for source in sources {
        for points in [0, 1, 1000] {
            for (mode, opacity) in [(BrushMode::Normal, 1.0), (BrushMode::Darken, 0.3)] {
                let op = sourced_stroke(source.clone(), points, mode, opacity);
                let text = serde_json::to_string(&op).unwrap();
                assert_eq!(serde_json::from_str::<Op>(&text).unwrap(), op);

                let value = serde_json::to_value(&op).unwrap();
                let fields = value["brush-stroke"].as_object().unwrap();
                match &source {
                    StrokeSource::Frame(n) => {
                        assert_eq!(fields["source_id"], serde_json::json!(n), "{op:?}");
                        assert!(!fields.contains_key("source"), "{op:?}");
                    }
                    StrokeSource::Checkpoint(id) => {
                        assert_eq!(
                            fields["source"],
                            serde_json::json!({ "checkpoint": id }),
                            "{op:?}"
                        );
                        assert!(!fields.contains_key("source_id"), "{op:?}");
                    }
                }
            }
        }
    }
}

#[test]
fn stroke_source_frame_keeps_its_bytes_and_id() {
    let literal = r#"[{"brush-stroke":{"source_id":65535,"path":[{"x":-7,"y":0},{"x":0,"y":41},{"x":9,"y":9}],"radius":2.5,"softness":1.0}}]"#;
    let ops: Vec<Op> = serde_json::from_str(literal).unwrap();
    let bytes = serde_json::to_vec(&ops).unwrap();
    assert_eq!(bytes, literal.as_bytes());
    let id = |ops_json: &[u8]| {
        Checkpoint::derive_child_id("parent", "fingerprint", &blake3::hash(ops_json).to_hex(), 7)
    };
    assert_eq!(id(&bytes), id(literal.as_bytes()));
    assert_eq!(
        serde_json::to_string(&Op::frame_stroke(2, vec![Point { x: 0, y: 0 }], 4.5, 0.25)).unwrap(),
        r#"{"brush-stroke":{"source_id":2,"path":[{"x":0,"y":0}],"radius":4.5,"softness":0.25}}"#
    );
    assert_eq!(
        ops,
        vec![Op::frame_stroke(
            u16::MAX,
            vec![
                Point { x: -7, y: 0 },
                Point { x: 0, y: 41 },
                Point { x: 9, y: 9 }
            ],
            2.5,
            1.0
        )]
    );

    let (_dir, path) = sealed("project_format_stroke_source_frame");
    let frames = vec![
        Op::frame_stroke(0, vec![Point { x: 1, y: 2 }], 3.0, 0.5),
        sourced_stroke(StrokeSource::Frame(2), 2, BrushMode::Lighten, 0.5),
    ];
    let appended = project::append_ops(&path, frames.clone(), None).unwrap();
    let entry = format!("checkpoints/{}.json", appended.checkpoint);
    let raw: serde_json::Value = serde_json::from_slice(
        &Index::read(&path)
            .unwrap()
            .read_entry(&path, &entry)
            .unwrap(),
    )
    .unwrap();
    for (i, expected) in [0, 2].into_iter().enumerate() {
        let written = raw["ops"][i]["brush-stroke"].as_object().unwrap();
        assert_eq!(written["source_id"], serde_json::json!(expected), "{raw}");
        assert!(!written.contains_key("source"), "{raw}");
    }
    assert_eq!(Project::open(&path).unwrap().checkpoint().ops, frames);
}

fn stroke_with(source_fields: &str) -> String {
    let comma = if source_fields.is_empty() { "" } else { "," };
    format!(
        r#"{{"brush-stroke":{{{source_fields}{comma}"path":[{{"x":2,"y":3}}],"radius":1.5,"softness":0.25}}}}"#
    )
}

#[test]
fn legacy_source_id_reads_as_a_frame() {
    for n in [0u16, 1, 255, 256, 65535] {
        for fields in [
            format!(r#""source_id":{n}"#),
            format!(r#""source":{{"frame":{n}}}"#),
        ] {
            let op: Op = serde_json::from_str(&stroke_with(&fields)).unwrap();
            let Op::BrushStroke { source, .. } = op else {
                panic!("not a brush stroke: {op:?}");
            };
            assert_eq!(source, StrokeSource::Frame(n), "{fields}");
        }
    }
}

#[test]
fn legacy_source_id_with_source_is_refused() {
    let both = stroke_with(r#""source_id":4,"source":{"frame":4}"#);
    let message = serde_json::from_str::<Op>(&both).unwrap_err().to_string();
    assert!(
        message.contains("source_id") && message.contains("source"),
        "{message}"
    );
    assert!(message.contains("both"), "{message}");

    let neither = serde_json::from_str::<Op>(&stroke_with("")).unwrap_err();
    assert!(
        neither.to_string().contains("missing field `source_id`"),
        "{neither}"
    );

    let unknown = serde_json::from_str::<Op>(&stroke_with(r#""source":{"layer":1}"#)).unwrap_err();
    assert!(unknown.to_string().contains("unknown variant"), "{unknown}");
}

#[test]
fn brush_stroke_mode_opacity_non_default_round_trips() {
    let modes = [
        (BrushMode::Normal, "normal"),
        (BrushMode::Lighten, "lighten"),
        (BrushMode::Darken, "darken"),
        (BrushMode::Detail, "detail"),
    ];
    for (mode, name) in modes {
        for opacity in [1.0_f32, 0.25, 1e-6] {
            let op = stroke(mode, opacity);
            let text = serde_json::to_string(&op).unwrap();
            assert_eq!(serde_json::from_str::<Op>(&text).unwrap(), op, "{text}");

            let value = serde_json::to_value(&op).unwrap();
            let fields = value["brush-stroke"].as_object().unwrap();
            let written_mode = (mode != BrushMode::Normal).then(|| serde_json::json!(name));
            assert_eq!(fields.get("mode"), written_mode.as_ref(), "{text}");
            assert_eq!(fields.contains_key("opacity"), opacity != 1.0, "{text}");
        }
    }

    let lighten = serde_json::to_value(stroke(BrushMode::Lighten, 1.0)).unwrap();
    assert!(
        lighten["brush-stroke"].get("opacity").is_none(),
        "{lighten}"
    );
    let half = serde_json::to_value(stroke(BrushMode::Normal, 0.5)).unwrap();
    assert_eq!(half["brush-stroke"]["opacity"], serde_json::json!(0.5));
    assert!(half["brush-stroke"].get("mode").is_none(), "{half}");
}

#[test]
fn brush_stroke_mode_opacity_refuses_an_unknown_mode() {
    for bad in ["screen", "Normal"] {
        let text = format!(
            r#"{{"brush-stroke":{{"source_id":1,"path":[],"radius":2.0,"softness":0.5,"mode":"{bad}"}}}}"#
        );
        let err = serde_json::from_str::<Op>(&text).unwrap_err();
        assert!(err.to_string().contains("unknown variant"), "{bad}: {err}");
    }
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

/// A copy of a sealed project whose manifest is edited as a JSON value.
fn with_manifest(
    src: &Path,
    name: &str,
    edit: impl Fn(&mut serde_json::Map<String, serde_json::Value>),
) -> PathBuf {
    let dst = src.with_extension(format!("{name}.ffp"));
    rewrite_entry(src, &dst, "manifest.json", |bytes| {
        let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        edit(value.as_object_mut().unwrap());
        serde_json::to_vec_pretty(&value).unwrap()
    });
    dst
}

fn open_err(path: &Path) -> project::ProjectError {
    match Project::open(path) {
        Ok(_) => panic!("{} must be refused", path.display()),
        Err(err) => err,
    }
}

#[test]
fn newer_format_version_is_refused() {
    let (_dir, project_path) = sealed("project_format_newer");
    for found in [3u32, 7] {
        let newer = with_manifest(&project_path, &format!("v{found}"), |m| {
            m.insert("format_version".into(), serde_json::json!(found));
        });
        let err = open_err(&newer);
        assert!(
            matches!(
                &err,
                project::ProjectError::NewerFormat { found: f, supported: 2, .. } if *f == found
            ),
            "expected NewerFormat for version {found}: {err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!(
                "{}: saved by a newer FocusForge (project format {found}; this build reads up to 2). Update FocusForge to open it.",
                newer.display()
            )
        );
    }
}

#[test]
fn newer_format_version_is_refused_before_the_manifest_is_decoded() {
    let (_dir, project_path) = sealed("project_format_newer_undecodable");
    let newer = with_manifest(&project_path, "v3-no-registration", |m| {
        m.insert("format_version".into(), serde_json::json!(3));
        m.remove("registration")
            .expect("the sealed manifest must carry a registration key to remove");
    });
    let err = open_err(&newer);
    assert!(
        matches!(&err, project::ProjectError::NewerFormat { found: 3, .. }),
        "expected NewerFormat, not a parse error: {err:?}"
    );
}

#[test]
fn format_version_absent_or_current_opens() {
    let (_dir, project_path) = sealed("project_format_version_current");
    let head = Project::open(&project_path)
        .unwrap()
        .manifest()
        .checkpoint
        .clone();
    let cases = [
        ("absent", None),
        ("one", Some(serde_json::json!(1))),
        ("two", Some(serde_json::json!(2))),
        ("zero", Some(serde_json::json!(0))),
        ("null", Some(serde_json::Value::Null)),
    ];
    for (name, version) in cases {
        let copy = with_manifest(&project_path, name, |m| {
            if let Some(v) = &version {
                m.insert("format_version".into(), v.clone());
            }
        });
        let opened = Project::open(&copy).unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(opened.manifest().checkpoint, head, "{name}: head differs");
    }
}

#[test]
fn malformed_format_version_is_refused() {
    let (_dir, project_path) = sealed("project_format_version_malformed");
    let bad = [
        ("string", serde_json::json!("2")),
        ("negative", serde_json::json!(-1)),
        ("fraction", serde_json::json!(2.5)),
    ];
    for (name, version) in bad {
        let copy = with_manifest(&project_path, name, |m| {
            m.insert("format_version".into(), version.clone());
        });
        let err = open_err(&copy);
        assert!(
            matches!(&err, project::ProjectError::Container { .. }),
            "{name}: expected a container error: {err:?}"
        );
        let text = err.to_string();
        assert!(
            text.contains("manifest.json") && text.contains("format_version"),
            "{name}: the refusal must name the manifest and the field: {text}"
        );
    }

    let truncated = project_path.with_extension("truncated.ffp");
    rewrite_entry(&project_path, &truncated, "manifest.json", |bytes| {
        bytes[..bytes.len() / 2].to_vec()
    });
    let err = open_err(&truncated);
    assert!(
        matches!(&err, project::ProjectError::Container { .. }),
        "expected a container error: {err:?}"
    );
    let text = err
        .to_string()
        .replace(&truncated.display().to_string(), "");
    assert!(
        text.contains("manifest.json") && !text.contains("format_version"),
        "a syntax error must not name the field: {text}"
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
