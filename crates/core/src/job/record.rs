// SPDX-License-Identifier: GPL-3.0-or-later

//! [`JobRecord`]: what a resume must agree with, written once by
//! [`super::ledger::ResumeStore::begin`] and compared field by field by
//! [`super::ledger::ResumeStore::find`].
//!
//! `format` is a version constant, never a value a caller infers or
//! chooses: any value this build does not recognise means "no resumable
//! state, start over" -- returned as `None`, never treated as an error and
//! never read optimistically.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::job::fingerprint::StackFingerprint;
use crate::params::ParamValues;
use crate::tile::Rect;

/// The current on-disk record format. Bump this, never the field shapes
/// below, when the record's meaning changes in a way an older reader must
/// not interpret optimistically.
pub const JOB_RECORD_FORMAT: u32 = 1;

/// Everything a resume must agree with before it is allowed to continue: the
/// stack's identity, the geometry that gives a `PlannedTile::index()` its
/// meaning, and the resolved algorithm/parameter values that shaped the
/// units already recorded complete.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRecord {
    pub format: u32,
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
    pub composite_output: PathBuf,
    pub zmap_output: Option<PathBuf>,
}

/// The alignment algorithm's identity and resolved parameters, recorded so a
/// resume can tell "the same alignment was requested" from "a different
/// aligner or a different parameter now governs this stack" without
/// re-running `align_stack`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlignIdentity {
    pub algorithm: String,
    pub params: ParamValues,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record() -> JobRecord {
        JobRecord {
            format: JOB_RECORD_FORMAT,
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
            composite_output: PathBuf::from("out.tif"),
            zmap_output: None,
        }
    }

    #[test]
    fn job_record_round_trips_through_json() {
        let record = sample_record();
        let json = serde_json::to_string(&record).unwrap();
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
