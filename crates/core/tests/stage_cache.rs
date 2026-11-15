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
use focusforge_core::equalize::{EqualizationReport, EqualizeSettings, FrameEstimate};
use focusforge_core::io::{FrameDesc, RawIdentity, TiffHeader};
use focusforge_core::job::{
    ALIGN_SOLVER_VERSION, AlignIdentity, DRIVER_POLICY_FIELDS, EqualizeKey, FrameFingerprint,
    StackFingerprint, StageCache, StageKey,
};
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::project::source_hash_change_checked;
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
        orientation: 1,
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

/// Frame `i`'s content hash in the synthetic stacks.
fn sample_hash(i: usize) -> Option<String> {
    Some(format!("{i:064x}"))
}

fn sample_content(n: usize) -> Vec<String> {
    (0..n).map(|i| sample_hash(i).unwrap()).collect()
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
        .put_alignment(&key_a, &[], &sample_content(2), &sample_alignment(2))
        .unwrap();
    assert!(cache.get_alignment(&key_b, sample_hash).is_none());
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
        .put_alignment(&key_lax, &[], &sample_content(2), &sample_alignment(2))
        .unwrap();

    let key_strict = StageKey::align(&fingerprint, &strict);
    assert!(cache.get_alignment(&key_strict, sample_hash).is_none());
}

#[test]
fn changed_frame_is_a_miss() {
    let scratch = tmp_dir("changed-frame");
    let cache = StageCache::open(scratch.path()).unwrap();
    let fingerprint = sample_fingerprint(2);
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key = StageKey::align(&fingerprint, &identity);
    cache
        .put_alignment(&key, &[], &sample_content(2), &sample_alignment(2))
        .unwrap();
    assert!(cache.get_alignment(&key, sample_hash).is_some());

    let mut changed = fingerprint.clone();
    changed.frames[0].file_size += 1;
    let other_key = StageKey::align(&changed, &identity);
    assert!(cache.get_alignment(&other_key, sample_hash).is_none());

    let mut changed_strips = fingerprint;
    changed_strips.frames[0].strip_byte_counts[0] += 1;
    let other_key = StageKey::align(&changed_strips, &identity);
    assert!(cache.get_alignment(&other_key, sample_hash).is_none());
}

#[test]
fn orientation_change_is_a_miss() {
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let original = sample_fingerprint(2);
    let key = StageKey::align(&original, &identity).digest();
    assert_eq!(
        key,
        StageKey::align(&original.clone(), &identity).digest(),
        "one stack keys identically"
    );
    for orientation in [3, 2] {
        let mut changed = original.clone();
        changed.frames[1].orientation = orientation;
        assert_ne!(
            key,
            StageKey::align(&changed, &identity).digest(),
            "orientation {orientation} must separate the stage key"
        );
    }
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
        .put_alignment(&key, &[], &sample_content(2), &sample_alignment(2))
        .unwrap();
    assert!(cache.get_alignment(&key, sample_hash).is_some());
    identity.gains = vec![65_536, 65_536];
    assert!(
        cache
            .get_alignment(&StageKey::align(&fingerprint, &identity), sample_hash)
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
        .put_alignment(&key_a, &[], &sample_content(3), &sample_alignment(3))
        .unwrap();
    assert!(cache.get_alignment(&key_a, sample_hash).is_some());
    assert!(cache.get_alignment(&key_b, sample_hash).is_none());
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
        .put_alignment(&key_a, &[], &sample_content(3), &sample_alignment(3))
        .unwrap();
    assert!(cache.get_alignment(&key_b, sample_hash).is_some());
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
    cache
        .put_alignment(&key, &[], &sample_content(2), &alignment)
        .unwrap();
    let back = cache.get_alignment(&key, sample_hash).expect("must hit");

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

/// Copies the mantis-64 frames into `<scratch>/frames`, sorted.
fn copy_mantis_frames(scratch: &tempfile::TempDir) -> Vec<PathBuf> {
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
    paths
}

fn header_fingerprint(paths: &[PathBuf]) -> StackFingerprint {
    StackFingerprint {
        frames: paths
            .iter()
            .map(|p| FrameFingerprint::from(&FrameDesc::from(&TiffHeader::open(p).unwrap())))
            .collect(),
    }
}

/// The whole-file hash of frame `i` of `paths`, as the pipeline asks for it.
fn file_hash(paths: &[PathBuf], i: usize) -> Option<String> {
    source_hash_change_checked(&paths[i])
        .ok()
        .map(|(_, hash)| hash)
}

#[test]
fn mtime_is_not_in_the_key() {
    let scratch = tmp_dir("mtime");
    let paths = copy_mantis_frames(&scratch);
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key_before = StageKey::align(&header_fingerprint(&paths), &identity);
    let content: Vec<String> = (0..paths.len())
        .map(|i| file_hash(&paths, i).unwrap())
        .collect();

    let cache = StageCache::open(scratch.path()).unwrap();
    cache
        .put_alignment(
            &key_before,
            &paths,
            &content,
            &sample_alignment(paths.len()),
        )
        .unwrap();

    let future = SystemTime::now() + Duration::from_secs(3600);
    for p in &paths {
        // Windows refuses to set a time through a handle opened for reading only.
        fs::OpenOptions::new()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(future)
            .unwrap();
    }

    let key_after = StageKey::align(&header_fingerprint(&paths), &identity);
    assert_eq!(key_before.digest(), key_after.digest());
    assert!(
        cache
            .get_alignment(&key_after, |i| file_hash(&paths, i))
            .is_some(),
        "an entry keyed before the mtime touch must still hit after it"
    );
}

#[cfg(unix)]
#[test]
fn in_place_rewrite_with_restored_mtime_is_a_miss() {
    let scratch = tmp_dir("rewrite");
    let paths = copy_mantis_frames(&scratch);
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key = StageKey::align(&header_fingerprint(&paths), &identity);
    let content: Vec<String> = (0..paths.len())
        .map(|i| file_hash(&paths, i).unwrap())
        .collect();
    let cache = StageCache::open(scratch.path()).unwrap();
    cache
        .put_alignment(&key, &paths, &content, &sample_alignment(paths.len()))
        .unwrap();
    assert!(
        cache
            .get_alignment(&key, |i| file_hash(&paths, i))
            .is_some()
    );

    // Same length, same header, same inode, same modification time: only the
    // pixels differ, and only the change time moves.
    let last = paths.last().unwrap();
    let modified = fs::metadata(last).unwrap().modified().unwrap();
    let mut bytes = fs::read(last).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0xff;
    use std::os::unix::fs::FileExt;
    let file = fs::OpenOptions::new().write(true).open(last).unwrap();
    file.write_all_at(&bytes, 0).unwrap();
    file.set_modified(modified).unwrap();
    drop(file);

    let key_after = StageKey::align(&header_fingerprint(&paths), &identity);
    assert_eq!(
        key.digest(),
        key_after.digest(),
        "the headers are unchanged"
    );
    assert!(
        cache
            .get_alignment(&key_after, |i| file_hash(&paths, i))
            .is_none(),
        "a rewritten frame must not reuse the entry, whatever its mtime says"
    );
}

#[test]
fn a_header_match_with_other_content_is_a_miss() {
    let scratch = tmp_dir("other-content");
    let cache = StageCache::open(scratch.path()).unwrap();
    let fingerprint = sample_fingerprint(3);
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key = StageKey::align(&fingerprint, &identity);
    cache
        .put_alignment(&key, &[], &sample_content(3), &sample_alignment(3))
        .unwrap();

    let mut asked = Vec::new();
    let differs_at_the_last_frame = |i: usize| {
        asked.push(i);
        if i == 2 {
            Some("f".repeat(64))
        } else {
            sample_hash(i)
        }
    };
    assert!(
        cache
            .get_alignment(&key, differs_at_the_last_frame)
            .is_none()
    );
    assert_eq!(asked, [0, 1, 2], "every frame up to the differing one");

    let mut asked = Vec::new();
    let differs_at_the_first_frame = |i: usize| {
        asked.push(i);
        Some("f".repeat(64))
    };
    assert!(
        cache
            .get_alignment(&key, differs_at_the_first_frame)
            .is_none()
    );
    assert_eq!(asked, [0], "the lookup stops at the first differing frame");

    assert!(
        cache.get_alignment(&key, |_| None).is_none(),
        "a frame that cannot be hashed is a miss"
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
        .put_alignment(&key, &[], &sample_content(2), &sample_alignment(2))
        .unwrap();

    let path = cache.entry_path(&key);
    let mut text = fs::read_to_string(&path).unwrap();
    text.truncate(text.len() / 2);
    fs::write(&path, text).unwrap();

    assert!(cache.get_alignment(&key, sample_hash).is_none());
}

#[test]
fn entry_missing_a_required_field_is_a_miss() {
    let scratch = tmp_dir("missing-required-field");
    let cache = StageCache::open(scratch.path()).unwrap();
    let fingerprint = sample_fingerprint(2);
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key = StageKey::align(&fingerprint, &identity);
    cache
        .put_alignment(&key, &[], &sample_content(2), &sample_alignment(2))
        .unwrap();
    assert!(cache.get_alignment(&key, sample_hash).is_some());

    let path = cache.entry_path(&key);
    let text = fs::read_to_string(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(value.get("format").is_none());
    value
        .as_object_mut()
        .expect("cached entry is an object")
        .remove("reference");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

    assert!(cache.get_alignment(&key, sample_hash).is_none());
}

#[test]
fn entry_with_changed_frame_count_is_a_miss() {
    let scratch = tmp_dir("changed-frame-count");
    let cache = StageCache::open(scratch.path()).unwrap();
    let fingerprint = sample_fingerprint(2);
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key = StageKey::align(&fingerprint, &identity);
    cache
        .put_alignment(&key, &[], &sample_content(2), &sample_alignment(2))
        .unwrap();
    assert!(cache.get_alignment(&key, sample_hash).is_some());

    let path = cache.entry_path(&key);
    let text = fs::read_to_string(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value["frame_count"] = serde_json::json!(3);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

    assert!(cache.get_alignment(&key, sample_hash).is_none());
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
        .put_alignment(&key_b, &[], &sample_content(2), &sample_alignment(2))
        .unwrap();
    let path_b = cache.entry_path(&key_b);
    let path_a = cache.entry_path(&key_a);
    fs::create_dir_all(path_a.parent().unwrap()).unwrap();
    fs::rename(&path_b, &path_a).unwrap();

    assert!(cache.get_alignment(&key_a, sample_hash).is_none());
}

#[test]
fn alignment_entry_size_is_bounded() {
    let scratch = tmp_dir("entry-size");
    let cache = StageCache::open(scratch.path()).unwrap();
    let fingerprint = sample_fingerprint(61);
    let identity = sample_identity("ecc", FailurePolicy::Strict);
    let key = StageKey::align(&fingerprint, &identity);
    cache
        .put_alignment(&key, &[], &sample_content(61), &sample_alignment(61))
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

/// The digest of a fixed align key. Changing the equalization stage's key
/// must leave every existing align entry findable under this value.
#[test]
fn equalization_stage_leaves_the_align_digest_unchanged() {
    let key = StageKey::align(
        &sample_fingerprint(2),
        &sample_identity("ecc", FailurePolicy::Strict),
    );
    assert_eq!(
        key.digest(),
        "965031ea1e456299362cb5196694a6850e06fb70f9447bdbe4e1c652052e7eb6"
    );
}

const EQ_SETTINGS: EqualizeSettings = EqualizeSettings {
    max_gain: 4.0,
    target_stops: 0.0,
};

fn next_up(x: f64) -> f64 {
    f64::from_bits(x.to_bits() + 1)
}

fn eq_key(fingerprint: &StackFingerprint) -> EqualizeKey {
    EqualizeKey::new(fingerprint, EQ_SETTINGS)
}

/// Three frames: one ordinary with a thirds-valued mean and a `-0.0`
/// residual, one clamped with clipped samples, one black.
fn eq_report() -> EqualizationReport {
    EqualizationReport {
        max_gain: EQ_SETTINGS.max_gain,
        target_stops: EQ_SETTINGS.target_stops,
        neutral: 1.0 / 3.0,
        target: 2.0 / 3.0,
        frames: vec![
            FrameEstimate {
                mean: 1.0 / 3.0,
                estimated: 1.0 / 7.0,
                applied: 1.0 / 7.0,
                gain: 9362,
                clamped: false,
                black: false,
                clipped_samples: 0,
                output_mean: 2.0 / 3.0,
                residual: -0.0,
            },
            FrameEstimate {
                mean: 0.1,
                estimated: 6.5,
                applied: 4.0,
                gain: 4 << 16,
                clamped: true,
                black: false,
                clipped_samples: u64::MAX - 1,
                output_mean: f64::MIN_POSITIVE,
                residual: -123.456_789_012_345_67,
            },
            FrameEstimate {
                mean: 0.0,
                estimated: f64::INFINITY,
                applied: 1.0,
                gain: 1 << 16,
                clamped: false,
                black: true,
                clipped_samples: 0,
                output_mean: 0.0,
                residual: -2.0 / 3.0,
            },
        ],
    }
}

#[test]
fn equalization_key_digest_moves_with_each_input() {
    let fingerprint = sample_fingerprint(3);
    let base = eq_key(&fingerprint).digest();

    let mut moved = Vec::new();
    let mut header = fingerprint.clone();
    header.frames[1].width += 1;
    moved.push(("a frame header", eq_key(&header).digest()));
    let mut reordered = fingerprint.clone();
    reordered.frames.swap(0, 2);
    moved.push(("the frame order", eq_key(&reordered).digest()));
    moved.push(("a dropped frame", eq_key(&sample_fingerprint(2)).digest()));
    moved.push(("an added frame", eq_key(&sample_fingerprint(4)).digest()));
    moved.push((
        "the last bit of max_gain",
        EqualizeKey::new(
            &fingerprint,
            EqualizeSettings {
                max_gain: next_up(EQ_SETTINGS.max_gain),
                ..EQ_SETTINGS
            },
        )
        .digest(),
    ));
    moved.push((
        "the last bit of target_stops",
        EqualizeKey::new(
            &fingerprint,
            EqualizeSettings {
                target_stops: next_up(EQ_SETTINGS.target_stops),
                ..EQ_SETTINGS
            },
        )
        .digest(),
    ));
    let mut other_version = eq_key(&fingerprint);
    other_version.version += 1;
    moved.push(("the estimator version", other_version.digest()));
    for (what, digest) in &moved {
        assert_ne!(*digest, base, "{what} must change the digest");
    }

    let mut renamed = fingerprint.clone();
    renamed.frames[0].path = PathBuf::from("elsewhere/other-name.tif");
    assert_eq!(
        eq_key(&renamed).digest(),
        base,
        "a frame path is not part of the key"
    );
    assert_ne!(
        base,
        StageKey::align(&fingerprint, &sample_identity("ecc", FailurePolicy::Strict)).digest(),
        "the two stages never share a digest"
    );
}

#[test]
fn equalization_report_round_trips_every_bit() {
    let scratch = tmp_dir("eq-round-trip");
    let cache = StageCache::open(scratch.path()).unwrap();
    let key = eq_key(&sample_fingerprint(3));
    let report = eq_report();
    cache
        .put_equalization(&key, &[], &sample_content(3), &report)
        .unwrap();

    let back = cache
        .get_equalization(&key, sample_hash)
        .expect("a freshly written entry hits");
    assert_eq!(
        common::equalization_bits(&back),
        common::equalization_bits(&report)
    );
    assert!(back.frames[0].residual.is_sign_negative());
    assert_eq!(back.frames[2].estimated, f64::INFINITY);
}

#[test]
fn equalization_entry_that_cannot_be_trusted_is_a_miss() {
    let fingerprint = sample_fingerprint(3);
    let key = eq_key(&fingerprint);
    let scratch = tmp_dir("eq-misses");
    let cache = StageCache::open(scratch.path()).unwrap();
    let path = cache.equalization_entry_path(&key);

    assert!(
        cache
            .get_equalization(&key, |_| panic!("a hash was asked for without an entry"))
            .is_none(),
        "absent entry"
    );

    cache
        .put_equalization(&key, &[], &sample_content(3), &eq_report())
        .unwrap();
    assert!(cache.get_equalization(&key, sample_hash).is_some());
    let intact = fs::read_to_string(&path).unwrap();
    let edited = |edit: &dyn Fn(&mut serde_json::Value)| {
        let mut value: serde_json::Value = serde_json::from_str(&intact).unwrap();
        edit(&mut value);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    };

    fs::write(&path, &intact[..intact.len() / 2]).unwrap();
    assert!(
        cache.get_equalization(&key, sample_hash).is_none(),
        "truncated entry"
    );

    let align_key = StageKey::align(&fingerprint, &sample_identity("ecc", FailurePolicy::Strict));
    cache
        .put_alignment(&align_key, &[], &sample_content(3), &sample_alignment(3))
        .unwrap();
    fs::copy(cache.entry_path(&align_key), &path).unwrap();
    assert!(
        cache.get_equalization(&key, sample_hash).is_none(),
        "an align entry's JSON"
    );

    edited(&|v| v["key_digest"] = serde_json::json!("0".repeat(64)));
    assert!(
        cache.get_equalization(&key, sample_hash).is_none(),
        "a stored digest that is another key's"
    );

    edited(&|v| v["frame_count"] = serde_json::json!(2));
    assert!(
        cache.get_equalization(&key, sample_hash).is_none(),
        "a stored frame count that disagrees"
    );

    edited(&|v| {
        v["frames"].as_array_mut().unwrap().pop();
    });
    assert!(
        cache.get_equalization(&key, sample_hash).is_none(),
        "fewer stored frames than the key's"
    );

    fs::write(&path, &intact).unwrap();
    assert!(
        cache
            .get_equalization(&key, |i| {
                if i == 2 {
                    Some("f".repeat(64))
                } else {
                    sample_hash(i)
                }
            })
            .is_none(),
        "one frame with other content"
    );
    assert!(
        cache
            .get_equalization(&key, |i| if i == 1 { None } else { sample_hash(i) })
            .is_none(),
        "one frame that cannot be hashed"
    );
    assert!(
        cache.get_equalization(&key, sample_hash).is_some(),
        "the intact entry still hits"
    );
}

#[test]
fn equalization_entry_of_other_settings_or_frames_is_a_miss() {
    let scratch = tmp_dir("eq-other-key");
    let cache = StageCache::open(scratch.path()).unwrap();
    let fingerprint = sample_fingerprint(3);
    cache
        .put_equalization(&eq_key(&fingerprint), &[], &sample_content(3), &eq_report())
        .unwrap();

    let other_gain = EqualizeKey::new(
        &fingerprint,
        EqualizeSettings {
            max_gain: next_up(EQ_SETTINGS.max_gain),
            ..EQ_SETTINGS
        },
    );
    let other_stops = EqualizeKey::new(
        &fingerprint,
        EqualizeSettings {
            target_stops: next_up(EQ_SETTINGS.target_stops),
            ..EQ_SETTINGS
        },
    );
    let mut other_version = eq_key(&fingerprint);
    other_version.version += 1;
    let mut reordered = fingerprint.clone();
    reordered.frames.swap(0, 1);
    let mut grown = fingerprint.clone();
    grown.frames.push(sample_frame(3));
    for (what, key) in [
        ("max_gain", other_gain),
        ("target_stops", other_stops),
        ("version", other_version),
        ("frame order", eq_key(&reordered)),
        ("an added frame", eq_key(&grown)),
    ] {
        assert!(
            cache.get_equalization(&key, sample_hash).is_none(),
            "{what} differs"
        );
    }
}
