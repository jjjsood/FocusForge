// SPDX-License-Identifier: GPL-3.0-or-later

//! The stage cache: intermediate **stage** results keyed on the input
//! fingerprint plus only the parameters that stage reads, so re-running a
//! subset stays cheap. Two stages are cached: alignment, which is expensive
//! to compute, parameter-poor and tiny to store, and the brightness
//! equalization estimate, which reads every sample of every frame.
//!
//! This is the **second** consumer of the one cheap header-only fingerprint
//! [`super::fingerprint::StackFingerprint`] -- the resume ledger
//! (`super::ledger`) is the first. It finds a candidate entry with no pixel
//! read; equal headers do not mean equal pixels, so an entry also stores each
//! frame's whole-file blake3 ([`crate::project::source_hash`]) and is reused
//! only when every current hash equals the stored one. Moving, renaming or
//! touching a frame keeps the hit; rewriting it, or a frame that cannot be
//! hashed, misses.
//!
//! # The key
//!
//! [`StageKey`] is the **path-free projection** of a [`StackFingerprint`] (every
//! [`super::fingerprint::FrameFingerprint`] field except `path`, so a moved or renamed input
//! directory still hits) plus the align stage's own [`super::record::AlignIdentity`] --
//! `AlignAlgorithm::params()` plus the named driver fields, and nothing else.
//! [`EqualizeKey`] is the same projection plus the equalization settings and the estimator
//! revision ([`crate::equalize::EQUALIZE_ESTIMATE_VERSION`]); it digests its own mirror, so an
//! align key's bytes never depend on it.
//!
//! No `f64` ever reaches the key's bytes as a JSON float: `KeyParamValue::FloatBits` carries
//! `f64::to_bits()` instead, since `serde_json` 1.0.151's float formatter does not guarantee a
//! 17-significant-digit `f64` round-trips without the non-default `float_roundtrip` feature.
//! `KeyParamValue` is externally tagged (serde's default for an enum with no `#[serde(...)]`
//! attribute), never `#[serde(untagged)]` like [`crate::params::ParamValue`]: `Int(2)` and
//! `FloatBits(2)` must never collide on the wire.
//!
//! # Every read failure is a miss
//!
//! Absent file, unparsable or truncated JSON (an entry written without
//! content hashes included), a `key_digest` that does not equal the requested
//! key's, a `frame_count` that disagrees with the key's frame list, or a
//! frame whose content hash differs or cannot be computed: every one of
//! these is [`Option::None`] from
//! [`StageCache::get_alignment`] or [`StageCache::get_equalization`], never an
//! `Err`. A cache read that could not be trusted must behave exactly like a
//! cache that never had the entry -- the caller falls back to recomputing,
//! nothing more.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::algorithm::AlignStatus;
use crate::align::{FrameAlignment, ReferencePolicy, StackAlignment, Transform};
use crate::equalize::{
    EQUALIZE_ESTIMATE_VERSION, EqualizationReport, EqualizeSettings, FrameEstimate,
};
use crate::params::ParamValue;

use super::fingerprint::{FrameFingerprint, StackFingerprint};
use super::ledger::{ResumeError, write_atomic};
use super::record::AlignIdentity;
use crate::io::RawIdentity;

/// One variant per cached stage. `Fuse` is deliberately absent: whether a
/// fusion intermediate is worth caching is a measurement, not an assumption.
/// `Equalize` is the brightness estimate Setup computes before alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CachedStage {
    Align,
    Equalize,
}

impl CachedStage {
    /// The directory name this stage's entries live under.
    fn dir_name(self) -> &'static str {
        match self {
            CachedStage::Align => "align",
            CachedStage::Equalize => "equalize",
        }
    }
}

/// Everything that can go wrong opening a [`StageCache`], or writing or
/// clearing an entry. A read that fails to produce a trustworthy entry is
/// never one of these -- see this module's own docs, "Every read failure is
/// a miss".
#[derive(Debug, thiserror::Error)]
pub enum StageCacheError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{}: {source}", path.display())]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

impl From<ResumeError> for StageCacheError {
    /// `write_atomic` only ever constructs `ResumeError::Io` or
    /// `ResumeError::Json`; `Mismatch` is
    /// `ResumeStore::find` outcomes this conversion never sees.
    fn from(err: ResumeError) -> Self {
        match err {
            ResumeError::Io { path, source } => StageCacheError::Io { path, source },
            ResumeError::Json { path, source } => StageCacheError::Json { path, source },
            ResumeError::Mismatch(_) => {
                unreachable!("write_atomic never returns Mismatch")
            }
        }
    }
}

/// One frame's identity in a [`StageKey`] -- the **path-free projection** of
/// a [`FrameFingerprint`]: every field except `path`, so a moved or renamed
/// input directory still hits. `mtime` is already absent upstream
/// (`FrameFingerprint`'s own field set); nothing here re-adds it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FrameKey {
    pub file_size: u64,
    pub image_description: Option<String>,
    pub orientation: u16,
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    pub strip_offsets: Vec<u64>,
    pub strip_byte_counts: Vec<u64>,
    pub raw_identity: Option<RawIdentity>,
}

impl From<&FrameFingerprint> for FrameKey {
    fn from(fp: &FrameFingerprint) -> Self {
        FrameKey {
            file_size: fp.file_size,
            image_description: fp.image_description.clone(),
            orientation: fp.orientation,
            width: fp.width,
            height: fp.height,
            samples_per_pixel: fp.samples_per_pixel,
            strip_offsets: fp.strip_offsets.clone(),
            strip_byte_counts: fp.strip_byte_counts.clone(),
            raw_identity: fp.raw_identity.clone(),
        }
    }
}

/// One resolved parameter value, encoded for the key's canonical bytes.
/// Externally tagged (serde's default here -- no `#[serde(untagged)]`, the
/// opposite of [`crate::params::ParamValue`]): `Int(2)` and `FloatBits(...)`
/// can never collide on the wire. See this module's own docs, "The key".
#[derive(Debug, Clone, PartialEq, Serialize)]
enum KeyParamValue {
    Int(i64),
    FloatBits(u64),
    Bool(bool),
    Enum(String),
}

fn key_param_value(value: &ParamValue) -> KeyParamValue {
    match value {
        ParamValue::Int(i) => KeyParamValue::Int(*i),
        ParamValue::Float(f) => KeyParamValue::FloatBits(f.to_bits()),
        ParamValue::Bool(b) => KeyParamValue::Bool(*b),
        ParamValue::Enum(s) => KeyParamValue::Enum(s.clone()),
    }
}

/// The mirror [`StageKey::canonical_bytes`] actually serializes: every
/// `f64` already widened to a [`KeyParamValue::FloatBits`], so
/// `serde_json::to_vec` can never emit a JSON float.
#[derive(Serialize)]
struct KeyMirror<'a> {
    stage: CachedStage,
    frames: &'a [FrameKey],
    algorithm: &'a str,
    params: std::collections::BTreeMap<String, KeyParamValue>,
    failure_policy: crate::align::FailurePolicy,
    solver_version: u32,
    gains: &'a [u32],
}

/// A stage's cache key: the path-free projection of a [`StackFingerprint`]
/// plus that stage's own declared identity. Built by [`StageKey::align`] for
/// the align stage; the equalization stage has its own [`EqualizeKey`].
///
/// **In the key:** every [`FrameFingerprint`] field except `path` (via
/// [`FrameKey`]), [`AlignIdentity::algorithm`], `AlignIdentity::params`
/// (`AlignAlgorithm::params()`'s resolved values, nothing else), and the
/// named driver fields, `AlignIdentity::failure_policy`,
/// `AlignIdentity::solver_version` and `AlignIdentity::gains`
/// ([`DRIVER_POLICY_FIELDS`](super::record::DRIVER_POLICY_FIELDS)). An entry
/// stores only the key's digest, so the solver revision lets an alignment
/// computed by another revision miss, and other brightness gains let an
/// alignment of differently prepared sources miss.
///
/// **Deliberately not in the key**, by name: `path` and `mtime` (a moved, renamed or touched
/// input frame still hits); the frames' content hashes (the entry holds them and
/// [`StageCache::get_alignment`] compares them, so equal headers over other pixels miss);
/// tile geometry (`tile_size`, `levels` -- the align stage reads
/// neither); `backend` (an alignment is one backend's whole-stage result, and the CPU and GPU
/// solves agree within the geometric parity budget, so a hit survives switching the GPU on or
/// off; unlike [`super::record::JobRecord::backend`], which guards already-written fuse tiles;
/// a run without the cache recomputes); the stacking algorithm and its parameters (a
/// fusion-only parameter change stays cheap this way); and the output path (not part of what a
/// result is computed from).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StageKey {
    pub stage: CachedStage,
    pub frames: Vec<FrameKey>,
    pub align: AlignIdentity,
}

impl StageKey {
    /// Builds the align stage's key: `fingerprint`'s path-free projection
    /// plus `identity` (`AlignAlgorithm::params()` plus the named driver
    /// fields).
    pub fn align(fingerprint: &StackFingerprint, identity: &AlignIdentity) -> Self {
        StageKey {
            stage: CachedStage::Align,
            frames: fingerprint.frames.iter().map(FrameKey::from).collect(),
            align: identity.clone(),
        }
    }

    /// The canonical byte representation this key hashes to
    /// [`Self::digest`]: every `f64` widened to a `u64` via
    /// `key_param_value`, so the digest is bit-exact and independent of
    /// `serde_json`'s own float formatter. See this module's own docs, "The
    /// key".
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let params = self
            .align
            .params
            .iter()
            .map(|(id, value)| (id.clone(), key_param_value(value)))
            .collect();
        let mirror = KeyMirror {
            stage: self.stage,
            frames: &self.frames,
            algorithm: &self.align.algorithm,
            params,
            failure_policy: self.align.failure_policy,
            solver_version: self.align.solver_version,
            gains: &self.align.gains,
        };
        serde_json::to_vec(&mirror).expect("KeyMirror serialization cannot fail")
    }

    /// The key's digest: `blake3::hash(canonical_bytes)`, 64 hex characters.
    pub fn digest(&self) -> String {
        blake3::hash(&self.canonical_bytes()).to_hex().to_string()
    }
}

/// The mirror [`EqualizeKey::canonical_bytes`] serializes: both settings as
/// `f64::to_bits()`, never as JSON floats.
#[derive(Serialize)]
struct EqualizeKeyMirror<'a> {
    stage: CachedStage,
    frames: &'a [FrameKey],
    max_gain_bits: u64,
    target_stops_bits: u64,
    version: u32,
}

/// The equalization estimate's cache key: the path-free projection of a
/// [`StackFingerprint`] (see [`StageKey`]), the settings the estimate was
/// asked for and the estimator revision. The frames' content hashes are not
/// in the key; the entry holds them and [`StageCache::get_equalization`]
/// compares them.
#[derive(Debug, Clone, PartialEq)]
pub struct EqualizeKey {
    pub frames: Vec<FrameKey>,
    pub max_gain: f64,
    pub target_stops: f64,
    pub version: u32,
}

impl EqualizeKey {
    /// `fingerprint`'s projection with `settings` and the current
    /// [`EQUALIZE_ESTIMATE_VERSION`].
    pub fn new(fingerprint: &StackFingerprint, settings: EqualizeSettings) -> Self {
        EqualizeKey {
            frames: fingerprint.frames.iter().map(FrameKey::from).collect(),
            max_gain: settings.max_gain,
            target_stops: settings.target_stops,
            version: EQUALIZE_ESTIMATE_VERSION,
        }
    }

    /// The bytes [`Self::digest`] hashes; every `f64` enters as its bits.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mirror = EqualizeKeyMirror {
            stage: CachedStage::Equalize,
            frames: &self.frames,
            max_gain_bits: self.max_gain.to_bits(),
            target_stops_bits: self.target_stops.to_bits(),
            version: self.version,
        };
        serde_json::to_vec(&mirror).expect("EqualizeKeyMirror serialization cannot fail")
    }

    /// `blake3::hash(canonical_bytes)`, 64 hex characters.
    pub fn digest(&self) -> String {
        blake3::hash(&self.canonical_bytes()).to_hex().to_string()
    }
}

/// Converts one `f64` matrix to its per-entry `f64::to_bits()` projection,
/// and back -- the bit-exact encoding [`CachedFrameAlignment`] and the
/// resume ledger's `super::ledger::LedgerAlignment` both use.
pub(crate) fn transform_to_bits(t: &Transform) -> [[u64; 3]; 3] {
    let mut out = [[0u64; 3]; 3];
    for (row_out, row_in) in out.iter_mut().zip(t.m.iter()) {
        for (bits, value) in row_out.iter_mut().zip(row_in.iter()) {
            *bits = value.to_bits();
        }
    }
    out
}

pub(crate) fn transform_from_bits(bits: [[u64; 3]; 3]) -> Transform {
    let mut m = [[0.0f64; 3]; 3];
    for (row_out, row_in) in m.iter_mut().zip(bits.iter()) {
        for (value, bits) in row_out.iter_mut().zip(row_in.iter()) {
            *value = f64::from_bits(*bits);
        }
    }
    Transform { m }
}

/// One frame's alignment, bit-exact encoded -- the same value
/// [`FrameAlignment`] carries, with `transform` and `residual` widened to
/// `u64` via `f64::to_bits()`. Shared by the stage cache and the resume
/// ledger (`super::ledger`) rather than each encoding `f64` its own way.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFrameAlignment {
    pub frame: usize,
    #[serde(flatten)]
    pub status: AlignStatus,
    pub iterations: u32,
    pub fallback: bool,
    pub transform_bits: [[u64; 3]; 3],
    pub residual_bits: u64,
}

impl From<&FrameAlignment> for CachedFrameAlignment {
    fn from(f: &FrameAlignment) -> Self {
        CachedFrameAlignment {
            frame: f.frame,
            status: f.status,
            iterations: f.iterations,
            fallback: f.fallback,
            transform_bits: transform_to_bits(&f.transform),
            residual_bits: f.residual.to_bits(),
        }
    }
}

impl CachedFrameAlignment {
    /// `pub(super)`: read back by the resume ledger
    /// (`super::ledger::read_alignment`) as well as by this module's own
    /// [`CachedAlignment::into_stack_alignment`].
    pub(super) fn into_frame_alignment(self) -> FrameAlignment {
        FrameAlignment {
            frame: self.frame,
            transform: transform_from_bits(self.transform_bits),
            status: self.status,
            iterations: self.iterations,
            residual: f64::from_bits(self.residual_bits),
            fallback: self.fallback,
        }
    }
}

/// One stored entry: the alignment stage's result, plus enough metadata for
/// [`StageCache::get_alignment`] to validate it before trusting it. The key
/// itself is **not** stored -- only its digest: the projection carries
/// `strip_offsets`/`strip_byte_counts` per frame, which at reference scale is
/// megabytes of JSON. `paths` is stored for diagnostics only and is never
/// compared. `content` holds each frame's whole-file blake3 (hex, in frame
/// order): the key finds this entry from the headers, and only frames with
/// these same bytes may reuse it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedAlignment {
    pub key_digest: String,
    pub frame_count: usize,
    pub paths: Vec<PathBuf>,
    pub content: Vec<String>,
    pub reference: usize,
    pub policy: ReferencePolicy,
    pub frames: Vec<CachedFrameAlignment>,
}

impl CachedAlignment {
    fn new(
        key: &StageKey,
        paths: &[PathBuf],
        content: &[String],
        alignment: &StackAlignment,
    ) -> Self {
        CachedAlignment {
            key_digest: key.digest(),
            frame_count: key.frames.len(),
            paths: paths.to_vec(),
            content: content.to_vec(),
            reference: alignment.reference,
            policy: alignment.policy,
            frames: alignment
                .frames
                .iter()
                .map(CachedFrameAlignment::from)
                .collect(),
        }
    }

    fn into_stack_alignment(self) -> StackAlignment {
        StackAlignment {
            reference: self.reference,
            policy: self.policy,
            frames: self
                .frames
                .into_iter()
                .map(CachedFrameAlignment::into_frame_alignment)
                .collect(),
        }
    }
}

/// One frame's [`FrameEstimate`], bit-exact encoded: every `f64` widened to
/// `u64` via `f64::to_bits()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedFrameEstimate {
    mean_bits: u64,
    estimated_bits: u64,
    applied_bits: u64,
    gain: u32,
    clamped: bool,
    black: bool,
    clipped_samples: u64,
    output_mean_bits: u64,
    residual_bits: u64,
}

impl From<&FrameEstimate> for CachedFrameEstimate {
    fn from(f: &FrameEstimate) -> Self {
        CachedFrameEstimate {
            mean_bits: f.mean.to_bits(),
            estimated_bits: f.estimated.to_bits(),
            applied_bits: f.applied.to_bits(),
            gain: f.gain,
            clamped: f.clamped,
            black: f.black,
            clipped_samples: f.clipped_samples,
            output_mean_bits: f.output_mean.to_bits(),
            residual_bits: f.residual.to_bits(),
        }
    }
}

impl From<CachedFrameEstimate> for FrameEstimate {
    fn from(c: CachedFrameEstimate) -> Self {
        FrameEstimate {
            mean: f64::from_bits(c.mean_bits),
            estimated: f64::from_bits(c.estimated_bits),
            applied: f64::from_bits(c.applied_bits),
            gain: c.gain,
            clamped: c.clamped,
            black: c.black,
            clipped_samples: c.clipped_samples,
            output_mean: f64::from_bits(c.output_mean_bits),
            residual: f64::from_bits(c.residual_bits),
        }
    }
}

/// One stored equalization estimate, laid out like [`CachedAlignment`]: the
/// key's digest, the frame count, diagnostic `paths`, each frame's content
/// hash, and the whole [`EqualizationReport`] bit-exact.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedEqualization {
    key_digest: String,
    frame_count: usize,
    paths: Vec<PathBuf>,
    content: Vec<String>,
    max_gain_bits: u64,
    target_stops_bits: u64,
    neutral_bits: u64,
    target_bits: u64,
    frames: Vec<CachedFrameEstimate>,
}

impl CachedEqualization {
    fn new(
        key: &EqualizeKey,
        paths: &[PathBuf],
        content: &[String],
        report: &EqualizationReport,
    ) -> Self {
        CachedEqualization {
            key_digest: key.digest(),
            frame_count: key.frames.len(),
            paths: paths.to_vec(),
            content: content.to_vec(),
            max_gain_bits: report.max_gain.to_bits(),
            target_stops_bits: report.target_stops.to_bits(),
            neutral_bits: report.neutral.to_bits(),
            target_bits: report.target.to_bits(),
            frames: report
                .frames
                .iter()
                .map(CachedFrameEstimate::from)
                .collect(),
        }
    }

    fn into_report(self) -> EqualizationReport {
        EqualizationReport {
            max_gain: f64::from_bits(self.max_gain_bits),
            target_stops: f64::from_bits(self.target_stops_bits),
            neutral: f64::from_bits(self.neutral_bits),
            target: f64::from_bits(self.target_bits),
            frames: self.frames.into_iter().map(FrameEstimate::from).collect(),
        }
    }
}

/// Whether an entry's stored frame count and content hashes agree with a key
/// of `key_frames` frames and the current files. `content(i)` is asked one
/// frame at a time and stops at the first frame that differs.
fn content_matches(
    frame_count: usize,
    stored: &[String],
    key_frames: usize,
    mut content: impl FnMut(usize) -> Option<String>,
) -> bool {
    frame_count == key_frames
        && stored.len() == frame_count
        && stored
            .iter()
            .enumerate()
            .all(|(i, hash)| content(i).as_ref() == Some(hash))
}

/// Writes `entry` to `path` atomically, creating its stage directory first.
fn write_entry<T: Serialize>(path: &Path, entry: &T) -> Result<(), StageCacheError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| StageCacheError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    write_atomic(path, entry).map_err(StageCacheError::from)
}

/// The stage cache under `<scratch_base>/stage-cache/`.
pub struct StageCache {
    root: PathBuf,
}

impl StageCache {
    /// Creates `<scratch_base>/stage-cache/` (and its ancestors) if it does
    /// not exist yet -- the same posture as `ResumeStore::open`.
    pub fn open(scratch_base: &Path) -> Result<Self, StageCacheError> {
        let root = scratch_base.join("stage-cache");
        fs::create_dir_all(&root).map_err(|source| StageCacheError::Io {
            path: root.clone(),
            source,
        })?;
        Ok(StageCache { root })
    }

    /// `<root>/<stage>/<first 16 hex chars of the digest>.json` -- the same
    /// 16-hex-char convention `crate::project::short_hash` uses. `pub` so a
    /// caller that needs to exercise an entry's on-disk shape directly (a
    /// test corrupting or renaming one) does not have to re-derive this
    /// rule by hand.
    pub fn entry_path(&self, key: &StageKey) -> PathBuf {
        self.path_of(key.stage, &key.digest())
    }

    /// [`Self::entry_path`] for an equalization entry.
    pub fn equalization_entry_path(&self, key: &EqualizeKey) -> PathBuf {
        self.path_of(CachedStage::Equalize, &key.digest())
    }

    fn path_of(&self, stage: CachedStage, digest: &str) -> PathBuf {
        self.root
            .join(stage.dir_name())
            .join(format!("{}.json", &digest[..16]))
    }

    /// Looks up `key`'s cached alignment. Every failure to produce a
    /// trustworthy entry is `None` -- see this module's own docs, "Every
    /// read failure is a miss".
    ///
    /// `content(i)` is frame `i`'s whole-file blake3 (hex), or `None` when it
    /// cannot be hashed; it is asked for one frame at a time, only once the
    /// entry's header key matches, and stops at the first frame that differs
    /// from the stored hash. An entry is reused only when every frame agrees.
    pub fn get_alignment(
        &self,
        key: &StageKey,
        content: impl FnMut(usize) -> Option<String>,
    ) -> Option<StackAlignment> {
        let path = self.entry_path(key);
        let text = fs::read_to_string(&path).ok()?;
        let cached: CachedAlignment = serde_json::from_str(&text).ok()?;
        if cached.key_digest != key.digest() {
            return None;
        }
        if !content_matches(
            cached.frame_count,
            &cached.content,
            key.frames.len(),
            content,
        ) {
            return None;
        }
        Some(cached.into_stack_alignment())
    }

    /// Looks up `key`'s cached equalization report, validated like
    /// [`Self::get_alignment`]: any failure to produce a trustworthy entry is
    /// `None`, and `content(i)` is asked for no frame before the entry file
    /// has parsed and matched the key.
    pub fn get_equalization(
        &self,
        key: &EqualizeKey,
        content: impl FnMut(usize) -> Option<String>,
    ) -> Option<EqualizationReport> {
        let text = fs::read_to_string(self.equalization_entry_path(key)).ok()?;
        let cached: CachedEqualization = serde_json::from_str(&text).ok()?;
        if cached.key_digest != key.digest()
            || cached.frames.len() != cached.frame_count
            || !content_matches(
                cached.frame_count,
                &cached.content,
                key.frames.len(),
                content,
            )
        {
            return None;
        }
        Some(cached.into_report())
    }

    /// Writes `alignment` under `key`, atomically. `paths` is stored for
    /// diagnostics only (never compared on read) -- the frame paths this
    /// alignment was computed from, as the caller already has them; passed
    /// separately because `key` is deliberately path-free. `content` is
    /// each frame's whole-file blake3, which [`Self::get_alignment`]
    /// compares before reusing the entry. A write
    /// failure is the caller's to decide how to handle -- the posture
    /// `crates/cli/src/stack.rs::write_manifest`'s own caller already takes
    /// is "warn, do not fail the run".
    pub fn put_alignment(
        &self,
        key: &StageKey,
        paths: &[PathBuf],
        content: &[String],
        alignment: &StackAlignment,
    ) -> Result<(), StageCacheError> {
        let cached = CachedAlignment::new(key, paths, content, alignment);
        write_entry(&self.entry_path(key), &cached)
    }

    /// Writes `report` under `key`, atomically; `paths` and `content` are as
    /// for [`Self::put_alignment`].
    pub fn put_equalization(
        &self,
        key: &EqualizeKey,
        paths: &[PathBuf],
        content: &[String],
        report: &EqualizationReport,
    ) -> Result<(), StageCacheError> {
        let cached = CachedEqualization::new(key, paths, content, report);
        write_entry(&self.equalization_entry_path(key), &cached)
    }

    /// Removes **only** this cache's entries -- never `resume/`, never
    /// `projects/`. Returns how many entry files were removed.
    pub fn clear(&self) -> Result<usize, StageCacheError> {
        let mut removed = 0usize;
        if self.root.is_dir() {
            for stage_entry in fs::read_dir(&self.root).map_err(|source| StageCacheError::Io {
                path: self.root.clone(),
                source,
            })? {
                let stage_entry = stage_entry.map_err(|source| StageCacheError::Io {
                    path: self.root.clone(),
                    source,
                })?;
                let stage_dir = stage_entry.path();
                if !stage_dir.is_dir() {
                    continue;
                }
                for file_entry in
                    fs::read_dir(&stage_dir).map_err(|source| StageCacheError::Io {
                        path: stage_dir.clone(),
                        source,
                    })?
                {
                    let file_entry = file_entry.map_err(|source| StageCacheError::Io {
                        path: stage_dir.clone(),
                        source,
                    })?;
                    if file_entry.path().is_file() {
                        removed += 1;
                    }
                }
            }
        }
        match fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(StageCacheError::Io {
                    path: self.root.clone(),
                    source,
                });
            }
        }
        fs::create_dir_all(&self.root).map_err(|source| StageCacheError::Io {
            path: self.root.clone(),
            source,
        })?;
        Ok(removed)
    }
}
