// SPDX-License-Identifier: GPL-3.0-or-later

//! [`JobRecord`]: the input and run facts a resume must agree with.
//! [`super::ledger::ResumeStore::begin`] writes it and
//! [`super::ledger::ResumeStore::find`] compares each record field by field.
//! A record that cannot deserialize is skipped as non-resumable state.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::align::FailurePolicy;
use crate::equalize::Equalization;
use crate::job::fingerprint::StackFingerprint;
use crate::params::ParamValues;
use crate::tile::Rect;

/// Everything a resume must agree with before it is allowed to continue: the
/// stack's identity, the geometry that gives a `PlannedTile::index()` its
/// meaning, and the resolved algorithm/parameter values that shaped the
/// units already recorded complete.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRecord {
    pub fingerprint: StackFingerprint,
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    pub frame_count: usize,
    /// The outer read-rect size `TilePlan::new` was built with -- a
    /// `PlannedTile::index()` is only meaningful against the `TilePlan` it
    /// came from, and `tile_size` plus `levels` below is what lets
    /// `TilePlan::new(width, height, tile_size, levels)` reproduce that plan
    /// in a fresh process.
    pub tile_size: u32,
    pub levels: u32,
    pub stack_algorithm: String,
    pub stack_params: ParamValues,
    /// Which compute path produced the already-written tiles: `"cpu"`, or
    /// the selected adapter's `crate::gpu::BackendInfo` display -- the same
    /// string `stack --json` reports as `backend`. Part of the identity, not
    /// merely informational: a run cancelled on the GPU path and resumed on
    /// the CPU one would finish one output file out of two different
    /// implementations of the same algorithm, which is precisely what the
    /// `<= 1 LSB` parity budget permits and a single file must not contain.
    pub backend: String,
    pub align: Option<AlignIdentity>,
    pub crop: Option<Rect>,
    /// The bound and per-frame gains the fused tiles were read through;
    /// `None` when equalization was off.
    pub equalization: Option<Equalization>,
    pub composite_output: PathBuf,
    pub zmap_output: Option<PathBuf>,
}

/// The alignment algorithm's identity and resolved parameters, recorded so a
/// resume can tell "the same alignment was requested" from "a different
/// aligner or a different parameter now governs this stack" without
/// re-running `align_stack`.
///
/// This is also the align stage cache key's algorithm half:
/// **`AlignAlgorithm::params()` plus the named driver fields, and nothing
/// else** -- `failure_policy`, `solver_version` and `gains`, named in
/// [`DRIVER_POLICY_FIELDS`]. A field added here without a matching addition
/// to `DRIVER_POLICY_FIELDS` widens the cache key silently;
/// `crates/core/tests/stage_cache.rs`'s
/// `the_align_key_is_params_plus_the_named_driver_fields` is the test that
/// catches it.
///
/// `solver_version` has no serde default: a record written without it does
/// not parse, so it is never resumed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlignIdentity {
    pub algorithm: String,
    pub params: ParamValues,
    pub failure_policy: FailurePolicy,
    pub solver_version: u32,
    /// The Q16 brightness gains the alignment read its sources through,
    /// index == frame index; empty when the sources were read as delivered.
    pub gains: Vec<u32>,
}

impl AlignIdentity {
    /// The identity of an alignment this build computes: `solver_version` is
    /// [`ALIGN_SOLVER_VERSION`].
    pub fn new(
        algorithm: &str,
        params: ParamValues,
        failure_policy: FailurePolicy,
        gains: Vec<u32>,
    ) -> Self {
        AlignIdentity {
            algorithm: algorithm.to_string(),
            params,
            failure_policy,
            solver_version: ALIGN_SOLVER_VERSION,
            gains,
        }
    }
}

/// The alignment solver's revision. Bump it on any change that can move an
/// aligned transform or a frame's status, so alignments computed before the
/// change are neither reused from the stage cache nor resumed.
pub const ALIGN_SOLVER_VERSION: u32 = 5;

/// The field names on [`AlignIdentity`] that are **not** one of
/// `AlignAlgorithm::params()`'s declared parameters -- the named exceptions
/// the stage cache key's completeness test checks against.
pub const DRIVER_POLICY_FIELDS: &[&str] = &["failure_policy", "solver_version", "gains"];

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record() -> JobRecord {
        JobRecord {
            fingerprint: StackFingerprint { frames: Vec::new() },
            width: 512,
            height: 512,
            samples_per_pixel: 3,
            frame_count: 8,
            tile_size: 200,
            levels: 3,
            stack_algorithm: "laplacian".to_string(),
            stack_params: ParamValues::new(),
            backend: "cpu".to_string(),
            align: None,
            crop: None,
            equalization: None,
            composite_output: PathBuf::from("out.tif"),
            zmap_output: None,
        }
    }

    #[test]
    fn job_record_round_trips_through_json() {
        let record = sample_record();
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains("\"format\""));
        let back: JobRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
    }

    #[test]
    fn stack_params_equality_is_a_plain_btreemap_eq() {
        // `ParamValues` is a `BTreeMap`, so ordering is deterministic and
        // equality is exactly `==` -- pinned here so a future change of
        // `ParamValues`'s backing type is caught by this test, not silently.
        let mut a = sample_record();
        a.stack_params
            .insert("levels".to_string(), crate::params::ParamValue::Int(3));
        let mut b = sample_record();
        b.stack_params
            .insert("levels".to_string(), crate::params::ParamValue::Int(3));
        assert_eq!(a, b);

        b.stack_params
            .insert("levels".to_string(), crate::params::ParamValue::Int(4));
        assert_ne!(a, b);
    }
}
