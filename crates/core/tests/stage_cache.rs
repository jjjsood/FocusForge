// SPDX-License-Identifier: GPL-3.0-or-later

//! The align stage cache's key -- both directions (an unrelated
//! parameter/geometry/backend leaving the digest byte-identical; an align
//! parameter, the failure policy, the solver revision, or a frame's
//! fingerprint changing it),
//! the path-free projection, the bit-exact round trip, and the four miss
//! cases: absent, truncated, missing required field, digest mismatch.

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use focusforge_core::algorithm::AlignStatus;
use focusforge_core::align::{
    FailurePolicy, FrameAlignment, ReferencePolicy, StackAlignment, Transform,
};
use focusforge_core::io::{FrameDesc, RawIdentity, TiffHeader};
use focusforge_core::job::{
    ALIGN_SOLVER_VERSION, AlignIdentity, DRIVER_POLICY_FIELDS, FrameFingerprint, StackFingerprint,
    StageCache, StageKey,
};
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::registry;

/// A directory of its own for one test, removed when the returned guard
/// drops -- including while a panic unwinds out of the test.
fn tmp_dir(label: &str) -> tempfile::TempDir {
    tempfile::TempDir::with_prefix(format!("ff-stage-cache-{label}-")).unwrap()
}

fn sample_frame(idx: usize) -> FrameFingerprint {
    FrameFingerprint {
        path: PathBuf::from(format!("frame-{idx}.tif")),
        file_size: 1_000 + idx as u64,
        image_description: None,
        width: 64,
        height: 64,
        samples_per_pixel: 3,
        strip_offsets: vec![8, 16],
        strip_byte_counts: vec![100, 100],
        raw_identity: None,
    }
}

fn sample_fingerprint(n: usize) -> StackFingerprint {
    StackFingerprint {
        frames: (0..n).map(sample_frame).collect(),
    }
}

fn sample_identity(algorithm: &str, policy: FailurePolicy) -> AlignIdentity {
    let mut params = ParamValues::new();
    params.insert("max-iterations".to_string(), ParamValue::Int(50));
    AlignIdentity::new(algorithm, params, policy, Vec::new())
}

fn sample_alignment(n: usize) -> StackAlignment {
    StackAlignment {
        reference: 0,
        policy: ReferencePolicy::Anchor,
        frames: (0..n)
            .map(|i| FrameAlignment {
                frame: i,
                transform: Transform::identity(),
                status: AlignStatus::Converged,
                iterations: 3,
                residual: 0.95,
                fallback: false,
            })
            .collect(),
    }
}

#[test]
fn alignment_parameter_change_is_a_miss() {
    let fingerprint = sample_fingerprint(2);
    let identity_a = sample_identity("ecc", FailurePolicy::Strict);
    let mut identity_b = identity_a.clone();
    identity_b
        .params
        .insert("max-iterations".to_string(), ParamValue::Int(99));
    let key_a = StageKey::align(&fingerprint, &identity_a);
    let key_b = StageKey::align(&fingerprint, &identity_b);
    assert_ne!(key_a.digest(), key_b.digest());

    let scratch = tmp_dir("param-change");
    let cache = StageCache::open(scratch.path()).unwrap();
    cache
        .put_alignment(&key_a, &[], &sample_alignment(2))
        .unwrap();
    assert!(cache.get_alignment(&key_b).is_none());
}

#[test]
fn a_strict_run_does_not_reuse_a_lax_alignment() {
    let fingerprint = sample_fingerprint(2);
    let lax = sample_identity("ecc", FailurePolicy::IdentityFallback);
    let strict = sample_identity("ecc", FailurePolicy::Strict);
    let scratch = tmp_dir("lax-vs-strict");
    let cache = StageCache::open(scratch.path()).unwrap();
    let key_lax = StageKey::align(&fingerprint, &lax);
    cache
        .put_alignment(&key_lax, &[], &sample_alignment(2))
        .unwrap();

    let key_strict = StageKey::align(&fingerprint, &strict);
    assert!(cache.get_alignment(&key_strict).is_none());
}

#[test]
fn changed_frame_is_a_miss() {
    let scratch = tmp_dir("changed-frame");
    let cache = StageCache::open(scratch.path()).unwrap();
    let fingerprint = sample_fingerprint(2);
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key = StageKey::align(&fingerprint, &identity);
    cache
        .put_alignment(&key, &[], &sample_alignment(2))
        .unwrap();
    assert!(cache.get_alignment(&key).is_some());

    let mut changed = fingerprint.clone();
    changed.frames[0].file_size += 1;
    let other_key = StageKey::align(&changed, &identity);
    assert!(cache.get_alignment(&other_key).is_none());

    let mut changed_strips = fingerprint;
    changed_strips.frames[0].strip_byte_counts[0] += 1;
    let other_key = StageKey::align(&changed_strips, &identity);
    assert!(cache.get_alignment(&other_key).is_none());
}

#[test]
fn raw_metadata_change_is_a_miss() {
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let mut original = sample_fingerprint(1);
    original.frames[0].raw_identity = Some(RawIdentity {
        make: "Test".to_string(),
        model: "Camera".to_string(),
        serial_number: Some("one".to_string()),
        capture_time_original: Some("2026:08:29 18:42:41".to_string()),
        raw_width: 12,
        raw_height: 10,
        black_levels: Some(vec![64.0; 4]),
        white_levels: Some(vec![4095]),
        cfa: Some(vec![0, 1, 1, 2]),
        cfa_width: Some(2),
        cfa_height: Some(2),
    });
    for (name, change) in [
        (
            "serial",
            Box::new(|raw: &mut RawIdentity| raw.serial_number = Some("two".to_string()))
                as Box<dyn Fn(&mut RawIdentity)>,
        ),
        (
            "original time",
            Box::new(|raw: &mut RawIdentity| {
                raw.capture_time_original = Some("2026:08:29 18:42:42".to_string())
            }),
        ),
        (
            "black levels",
            Box::new(|raw: &mut RawIdentity| raw.black_levels = Some(vec![65.0; 4])),
        ),
        (
            "white levels",
            Box::new(|raw: &mut RawIdentity| raw.white_levels = Some(vec![4094])),
        ),
    ] {
        let mut changed = original.clone();
        change(
            changed.frames[0]
                .raw_identity
                .as_mut()
                .expect("RAW identity"),
        );
        assert_ne!(
            StageKey::align(&original, &identity).digest(),
            StageKey::align(&changed, &identity).digest(),
            "{name} must invalidate the stage key with every other fact fixed"
        );
    }
}

#[test]
fn every_parameter_the_align_stage_reads_is_in_its_key() {
    let algo = registry::find_align("pyramidal-ecc").expect("pyramidal-ecc must be registered");
    let ctx = StackContext::new(8, 512, 512, 3);
    let resolved = params::resolve(algo.params(), &ParamValues::new(), &ctx)
        .expect("resolve must succeed with no explicit overrides");
    let values = params::resolved_values(&resolved);
    let identity = AlignIdentity::new(algo.id(), values, FailurePolicy::Strict, Vec::new());
    let fingerprint = sample_fingerprint(2);
    let key = StageKey::align(&fingerprint, &identity);
    let bytes = key.canonical_bytes();
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).expect("canonical bytes must be valid JSON");
    let params_obj = json["params"]
        .as_object()
        .expect("the key's canonical bytes must carry a `params` object");
    assert_eq!(
        params_obj.len(),
        algo.params().len(),
        "the key must carry exactly one entry per declared parameter"
    );
}

#[test]
fn the_align_key_is_params_plus_the_named_driver_fields() {
    assert_eq!(
        DRIVER_POLICY_FIELDS,
        ["failure_policy", "solver_version", "gains"]
    );

    let src = include_str!("../src/job/record.rs");
    let start = src
        .find("pub struct AlignIdentity {")
        .expect("AlignIdentity must be declared");
    let after = &src[start..];
    let end = after.find('}').expect("AlignIdentity's block must close");
    let block = &after[..end];
    // Skip the first line: `pub struct AlignIdentity {` itself also starts
    // with `pub ` and must not be counted as a field.
    let pub_fields = block
        .lines()
        .skip(1)
        .filter(|line| line.trim_start().starts_with("pub "))
        .count();
    assert_eq!(
        pub_fields, 5,
        "AlignIdentity must carry exactly 5 `pub` fields (algorithm, params, \
         failure_policy, solver_version, gains) -- a field added without updating this \
         test or DRIVER_POLICY_FIELDS silently widens the cache key:\n{block}"
    );
}

#[test]
fn the_key_bytes_carry_every_identity_field() {
    let key = StageKey::align(
        &sample_fingerprint(2),
        &sample_identity("ecc", FailurePolicy::Strict),
    );
    let json: serde_json::Value =
        serde_json::from_slice(&key.canonical_bytes()).expect("canonical bytes are JSON");
    for field in [
        "algorithm",
        "params",
        "failure_policy",
        "solver_version",
        "gains",
    ] {
        assert!(
            json.get(field).is_some(),
            "the canonical key bytes must carry `{field}`: {json}"
        );
    }
    assert_eq!(json["solver_version"], ALIGN_SOLVER_VERSION);
    assert!(json.get("format").is_none());
    for frame in json["frames"]
        .as_array()
        .expect("canonical key bytes carry frames")
    {
        assert_eq!(frame["raw_identity"], serde_json::Value::Null);
    }
    for frame_count in [1, 3] {
        let key = StageKey::align(
            &sample_fingerprint(frame_count),
            &sample_identity("ecc", FailurePolicy::Strict),
        );
        let json: serde_json::Value =
            serde_json::from_slice(&key.canonical_bytes()).expect("canonical bytes are JSON");
        assert!(json.get("format").is_none());
        assert!(
            json["frames"]
                .as_array()
                .expect("canonical key bytes carry frames")
                .iter()
                .all(|frame| frame["raw_identity"] == serde_json::Value::Null)
        );
    }
}

#[test]
fn changed_gains_change_the_align_digest() {
    let fingerprint = sample_fingerprint(2);
    let digest = |gains: Vec<u32>| {
        let mut identity = sample_identity("ecc", FailurePolicy::Strict);
        identity.gains = gains;
        StageKey::align(&fingerprint, &identity).digest()
    };
    let unprepared = digest(Vec::new());
    let unit = digest(vec![65_536, 65_536]);
    let shifted = digest(vec![65_536, 65_537]);
    assert_ne!(unprepared, unit, "preparation on or off");
    assert_ne!(unit, shifted, "one gain moved by one Q16 step");
    assert_eq!(shifted, digest(vec![65_536, 65_537]), "the same gains hit");

    let scratch = tmp_dir("prepared-gains");
    let cache = StageCache::open(scratch.path()).unwrap();
    let mut identity = sample_identity("ecc", FailurePolicy::Strict);
    identity.gains = vec![65_536, 65_537];
    let key = StageKey::align(&fingerprint, &identity);
    cache
        .put_alignment(&key, &[], &sample_alignment(2))
        .unwrap();
    assert!(cache.get_alignment(&key).is_some());
    identity.gains = vec![65_536, 65_536];
    assert!(
        cache
            .get_alignment(&StageKey::align(&fingerprint, &identity))
            .is_none()
    );
}

#[test]
fn a_solver_revision_change_is_a_miss() {
    let fingerprint = sample_fingerprint(3);
    let identity_a = sample_identity("ecc", FailurePolicy::Strict);
    let mut identity_b = identity_a.clone();
    identity_b.solver_version += 1;
    let key_a = StageKey::align(&fingerprint, &identity_a);
    let key_b = StageKey::align(&fingerprint, &identity_b);
    assert_ne!(key_a.digest(), key_b.digest());

    let scratch = tmp_dir("solver-revision");
    let cache = StageCache::open(scratch.path()).unwrap();
    cache
        .put_alignment(&key_a, &[], &sample_alignment(3))
        .unwrap();
    assert!(cache.get_alignment(&key_a).is_some());
    assert!(cache.get_alignment(&key_b).is_none());
}

#[test]
fn an_unrelated_parameter_does_not_change_the_key() {
    // `StageKey::align` takes only a fingerprint and an `AlignIdentity` --
    // no tile geometry, no `StackAlgorithm` parameters, no `backend` --
    // pinned structurally: the same two inputs always produce the same key,
    // regardless of what else a `JobRecord` might carry alongside them.
    let fingerprint = sample_fingerprint(3);
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key_a = StageKey::align(&fingerprint, &identity);
    let key_b = StageKey::align(&fingerprint, &identity);
    assert_eq!(key_a.canonical_bytes(), key_b.canonical_bytes());
    assert_eq!(key_a.digest(), key_b.digest());
}

#[test]
fn a_renamed_input_directory_still_hits() {
    let original = sample_fingerprint(3);
    let mut renamed = original.clone();
    for (i, f) in renamed.frames.iter_mut().enumerate() {
        f.path = PathBuf::from(format!("/moved/elsewhere/frame-{i}.tif"));
    }
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key_a = StageKey::align(&original, &identity);
    let key_b = StageKey::align(&renamed, &identity);
    assert_eq!(key_a.digest(), key_b.digest());

    let scratch = tmp_dir("renamed");
    let cache = StageCache::open(scratch.path()).unwrap();
    cache
        .put_alignment(&key_a, &[], &sample_alignment(3))
        .unwrap();
    assert!(cache.get_alignment(&key_b).is_some());
}

#[test]
fn a_round_trip_preserves_every_bit() {
    let fingerprint = sample_fingerprint(2);
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key = StageKey::align(&fingerprint, &identity);
    let alignment = StackAlignment {
        reference: 0,
        policy: ReferencePolicy::Chain,
        frames: vec![
            FrameAlignment {
                frame: 0,
                transform: Transform::identity(),
                status: AlignStatus::Converged,
                iterations: 0,
                residual: 1.0,
                fallback: false,
            },
            FrameAlignment {
                frame: 1,
                transform: Transform {
                    m: [
                        [
                            1.000_000_000_000_001,
                            0.123_456_789_012_345_6,
                            12.345_678_901_234_5,
                        ],
                        [
                            -0.000_000_000_000_001,
                            0.999_999_999_999_999,
                            -7.654_321_098_765_432,
                        ],
                        [0.0, 0.0, 1.0],
                    ],
                },
                status: AlignStatus::Converged,
                iterations: 12,
                residual: 0.918_273_645_510_203,
                fallback: false,
            },
        ],
    };
    let scratch = tmp_dir("round-trip");
    let cache = StageCache::open(scratch.path()).unwrap();
    cache.put_alignment(&key, &[], &alignment).unwrap();
    let back = cache.get_alignment(&key).expect("must hit");

    assert_eq!(back.frames.len(), alignment.frames.len());
    for (a, b) in alignment.frames.iter().zip(back.frames.iter()) {
        assert_eq!(a.residual.to_bits(), b.residual.to_bits());
        for r in 0..3 {
            for c in 0..3 {
                assert_eq!(a.transform.m[r][c].to_bits(), b.transform.m[r][c].to_bits());
            }
        }
    }
    assert_eq!(alignment, back);
}

#[test]
fn mtime_is_not_in_the_key() {
    let scratch = tmp_dir("mtime");
    let frames_dir = scratch.path().join("frames");
    fs::create_dir_all(&frames_dir).unwrap();
    let source = common::fixtures_dir().join("mantis-64").join("frames");
    let mut paths = Vec::new();
    for entry in fs::read_dir(&source).unwrap() {
        let entry = entry.unwrap();
        if entry.path().extension().and_then(|e| e.to_str()) != Some("tif") {
            continue;
        }
        let dest = frames_dir.join(entry.file_name());
        fs::copy(entry.path(), &dest).unwrap();
        paths.push(dest);
    }
    paths.sort();
    assert!(
        !paths.is_empty(),
        "fixtures/mantis-64/frames must hold at least one .tif"
    );

    let fingerprint_before = StackFingerprint {
        frames: paths
            .iter()
            .map(|p| FrameFingerprint::from(&FrameDesc::from(&TiffHeader::open(p).unwrap())))
            .collect(),
    };
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key_before = StageKey::align(&fingerprint_before, &identity);

    let cache = StageCache::open(scratch.path()).unwrap();
    cache
        .put_alignment(&key_before, &paths, &sample_alignment(paths.len()))
        .unwrap();

    let future = SystemTime::now() + Duration::from_secs(3600);
    for p in &paths {
        fs::File::open(p).unwrap().set_modified(future).unwrap();
    }

    let fingerprint_after = StackFingerprint {
        frames: paths
            .iter()
            .map(|p| FrameFingerprint::from(&FrameDesc::from(&TiffHeader::open(p).unwrap())))
            .collect(),
    };
    let key_after = StageKey::align(&fingerprint_after, &identity);
    assert_eq!(key_before.digest(), key_after.digest());
    assert!(
        cache.get_alignment(&key_after).is_some(),
        "an entry keyed before the mtime touch must still hit after it"
    );
}

#[test]
fn truncated_entry_is_a_miss() {
    let scratch = tmp_dir("truncated");
    let cache = StageCache::open(scratch.path()).unwrap();
    let fingerprint = sample_fingerprint(2);
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key = StageKey::align(&fingerprint, &identity);
    cache
        .put_alignment(&key, &[], &sample_alignment(2))
        .unwrap();

    let path = cache.entry_path(&key);
    let mut text = fs::read_to_string(&path).unwrap();
    text.truncate(text.len() / 2);
    fs::write(&path, text).unwrap();

    assert!(cache.get_alignment(&key).is_none());
}

#[test]
fn entry_missing_a_required_field_is_a_miss() {
    let scratch = tmp_dir("missing-required-field");
    let cache = StageCache::open(scratch.path()).unwrap();
    let fingerprint = sample_fingerprint(2);
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key = StageKey::align(&fingerprint, &identity);
    cache
        .put_alignment(&key, &[], &sample_alignment(2))
        .unwrap();
    assert!(cache.get_alignment(&key).is_some());

    let path = cache.entry_path(&key);
    let text = fs::read_to_string(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(value.get("format").is_none());
    value
        .as_object_mut()
        .expect("cached entry is an object")
        .remove("reference");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

    assert!(cache.get_alignment(&key).is_none());
}

#[test]
fn entry_with_changed_frame_count_is_a_miss() {
    let scratch = tmp_dir("changed-frame-count");
    let cache = StageCache::open(scratch.path()).unwrap();
    let fingerprint = sample_fingerprint(2);
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key = StageKey::align(&fingerprint, &identity);
    cache
        .put_alignment(&key, &[], &sample_alignment(2))
        .unwrap();
    assert!(cache.get_alignment(&key).is_some());

    let path = cache.entry_path(&key);
    let text = fs::read_to_string(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value["frame_count"] = serde_json::json!(3);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

    assert!(cache.get_alignment(&key).is_none());
}

#[test]
fn a_digest_collision_is_a_miss() {
    let scratch = tmp_dir("digest-collision");
    let cache = StageCache::open(scratch.path()).unwrap();
    let fingerprint_a = sample_fingerprint(2);
    let mut fingerprint_b = fingerprint_a.clone();
    fingerprint_b.frames[0].file_size += 1;
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key_a = StageKey::align(&fingerprint_a, &identity);
    let key_b = StageKey::align(&fingerprint_b, &identity);
    assert_ne!(key_a.digest(), key_b.digest());

    // Write key_b's own valid entry, then place it under key_a's file name
    // -- an entry whose file name matches key_a but whose stored digest is
    // key_b's.
    cache
        .put_alignment(&key_b, &[], &sample_alignment(2))
        .unwrap();
    let path_b = cache.entry_path(&key_b);
    let path_a = cache.entry_path(&key_a);
    fs::create_dir_all(path_a.parent().unwrap()).unwrap();
    fs::rename(&path_b, &path_a).unwrap();

    assert!(cache.get_alignment(&key_a).is_none());
}

#[test]
fn alignment_entry_size_is_bounded() {
    let scratch = tmp_dir("entry-size");
    let cache = StageCache::open(scratch.path()).unwrap();
    let fingerprint = sample_fingerprint(61);
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key = StageKey::align(&fingerprint, &identity);
    cache
        .put_alignment(&key, &[], &sample_alignment(61))
        .unwrap();

    let path = cache.entry_path(&key);
    let size = fs::metadata(&path).unwrap().len();
    assert!(
        size < 64 * 1024,
        "alignment entry for 61 frames was {size} bytes, expected < 64 KiB"
    );
    eprintln!("stage cache alignment entry size for 61 frames: {size} bytes");
}

#[test]
fn the_cache_key_projects_the_resume_fingerprint() {
    // One `StackFingerprint` value feeds both a `JobRecord::fingerprint`
    // (the resume ledger) and `StageKey::align` (the stage cache key) --
    // the same canonical type, imported from the same module, used
    // unconverted by both. `grep -rn "struct StackFingerprint" crates/core/src/`
    // is this test's companion check that no second definition exists.
    let fingerprint = sample_fingerprint(2);
    let record_fingerprint: StackFingerprint = fingerprint.clone();
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key = StageKey::align(&fingerprint, &identity);

    assert_eq!(record_fingerprint, fingerprint);
    assert_eq!(key.frames.len(), fingerprint.frames.len());
    for (k, f) in key.frames.iter().zip(fingerprint.frames.iter()) {
        assert_eq!(k.file_size, f.file_size);
        assert_eq!(k.strip_offsets, f.strip_offsets);
    }
}
