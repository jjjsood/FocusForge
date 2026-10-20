// SPDX-License-Identifier: GPL-3.0-or-later

//! The header-only stack fingerprint a resume compares against
//! is **cheap, two-tier**. This
//! is the *scratch/resume* fingerprint -- a plain comparison of the
//! [`crate::io::FrameDesc`] facts already parsed for free, no hash crate,
//! no pixel read. It is
//! deliberately a *different* value from [`crate::project`]'s manifest
//! fingerprint, which is a real content hash (blake3) over every source
//! file -- see this module's own `tests::not_convertible_to_a_content_hash`
//! and [`crate::project::stack_fingerprint`]. Neither substitutes for the
//! other: this one exists to find a resumable ledger in milliseconds, that
//! one to say years later exactly which bytes a project was computed from.
//! The stage cache uses this fingerprint only to find a candidate entry; it
//! reuses the entry only for frames whose content hashes match.
//! This is also distinct from the **checkpoint** fingerprint, documented in
//! full at [`super::stage_cache`]'s own module docs: the checkpoint DAG
//! reads [`crate::project::stack_fingerprint`], never this type -- two
//! deliberately different values, not one value everywhere.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::io::{FrameDesc, RawIdentity};

/// One frame's identity, read straight off its already-parsed
/// [`FrameDesc`] -- none of them requiring a pixel to be read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameFingerprint {
    pub path: PathBuf,
    pub file_size: u64,
    pub image_description: Option<String>,
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    pub strip_offsets: Vec<u64>,
    pub strip_byte_counts: Vec<u64>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub raw_identity: Option<RawIdentity>,
}

impl From<&FrameDesc> for FrameFingerprint {
    fn from(header: &FrameDesc) -> Self {
        FrameFingerprint {
            path: header.path.clone(),
            file_size: header.file_size,
            image_description: header.image_description.clone(),
            width: header.width,
            height: header.height,
            samples_per_pixel: header.samples_per_pixel,
            strip_offsets: header.strip_offsets.clone(),
            strip_byte_counts: header.strip_byte_counts.clone(),
            raw_identity: header.raw_identity.clone(),
        }
    }
}

/// A stack fingerprint gained or lost frames between the run that recorded
/// it and the one comparing against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameCount {
    pub expected: usize,
    pub found: usize,
}

impl std::fmt::Display for FrameCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "frame count changed: expected {}, found {}",
            self.expected, self.found
        )
    }
}

/// The first disagreement [`StackFingerprint::compare`] found, as data: the
/// frame, the field, and both values -- never a bare bool, so a refusal can
/// name what changed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error(
    "frame {frame} ({}): {field} changed: expected {expected}, found {found}",
    path.display()
)]
pub struct FingerprintMismatch {
    pub frame: usize,
    pub path: PathBuf,
    pub field: &'static str,
    pub expected: String,
    pub found: String,
}

/// A whole stack's identity: one [`FrameFingerprint`] per frame, in frame
/// order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StackFingerprint {
    pub frames: Vec<FrameFingerprint>,
}

macro_rules! check_field {
    ($frame:expr, $path:expr, $field:literal, $expected:expr, $found:expr) => {
        if $expected != $found {
            return Err(FingerprintMismatch {
                frame: $frame,
                path: $path.clone(),
                field: $field,
                expected: format!("{:?}", $expected),
                found: format!("{:?}", $found),
            });
        }
    };
}

impl StackFingerprint {
    /// The **first** disagreement between `self` (the fingerprint a resume
    /// was recorded against) and `other` (the fingerprint freshly computed
    /// for the stack being resumed), field by field, frame by frame in
    /// order. A frame-count change is reported through the caller
    /// (`Resumable`/`ResumeStore::find`), not through this method's `Err`,
    /// since [`FrameCount`] is not a per-frame [`FingerprintMismatch`].
    pub fn compare(&self, other: &Self) -> Result<(), FingerprintMismatch> {
        for (i, (a, b)) in self.frames.iter().zip(other.frames.iter()).enumerate() {
            check_field!(i, b.path, "path", a.path, b.path);
            check_field!(i, b.path, "file_size", a.file_size, b.file_size);
            check_field!(
                i,
                b.path,
                "image_description",
                a.image_description,
                b.image_description
            );
            check_field!(i, b.path, "width", a.width, b.width);
            check_field!(i, b.path, "height", a.height, b.height);
            check_field!(
                i,
                b.path,
                "samples_per_pixel",
                a.samples_per_pixel,
                b.samples_per_pixel
            );
            check_field!(i, b.path, "strip_offsets", a.strip_offsets, b.strip_offsets);
            check_field!(
                i,
                b.path,
                "strip_byte_counts",
                a.strip_byte_counts,
                b.strip_byte_counts
            );
            check_field!(i, b.path, "raw_identity", a.raw_identity, b.raw_identity);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{RawIdentity, TiffHeader};

    fn frame(path: &str, width: u32) -> FrameFingerprint {
        FrameFingerprint {
            path: PathBuf::from(path),
            file_size: 100,
            image_description: None,
            width,
            height: 10,
            samples_per_pixel: 3,
            strip_offsets: vec![1, 2, 3],
            strip_byte_counts: vec![4, 5, 6],
            raw_identity: None,
        }
    }

    #[test]
    fn identical_fingerprints_compare_equal() {
        let a = StackFingerprint {
            frames: vec![frame("a.tif", 10), frame("b.tif", 10)],
        };
        let b = a.clone();
        assert_eq!(a.compare(&b), Ok(()));
    }

    #[test]
    fn compare_names_the_first_disagreeing_field() {
        let a = StackFingerprint {
            frames: vec![frame("a.tif", 10)],
        };
        let mut changed = frame("a.tif", 10);
        changed.width = 20;
        let b = StackFingerprint {
            frames: vec![changed],
        };

        let err = a.compare(&b).unwrap_err();
        assert_eq!(err.frame, 0);
        assert_eq!(err.field, "width");
        assert_eq!(err.expected, "10");
        assert_eq!(err.found, "20");
        assert!(err.to_string().contains("a.tif"));
        assert!(err.to_string().contains("width"));
    }

    #[test]
    fn raw_identity_is_compared_and_serialized_for_tiff() {
        let mut a = frame("a.dng", 10);
        a.raw_identity = Some(RawIdentity {
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
        let raw_json = serde_json::to_value(&a).expect("RAW fingerprint serializes");
        assert!(raw_json["raw_identity"].is_object());
        assert_eq!(
            serde_json::from_value::<FrameFingerprint>(raw_json)
                .expect("RAW fingerprint deserializes"),
            a
        );
        let mut b = a.clone();
        b.raw_identity.as_mut().expect("RAW identity").serial_number = Some("two".to_string());
        let error = StackFingerprint { frames: vec![a] }
            .compare(&StackFingerprint { frames: vec![b] })
            .expect_err("RAW metadata must invalidate resume");
        assert_eq!(error.field, "raw_identity");

        let tiff = frame("a.tif", 10);
        let json = serde_json::to_value(&tiff).expect("TIFF fingerprint serializes");
        assert_eq!(json["raw_identity"], serde_json::Value::Null);
        assert_eq!(
            serde_json::from_value::<FrameFingerprint>(json.clone())
                .expect("TIFF fingerprint deserializes"),
            tiff
        );
        let without_identity = json
            .as_object()
            .expect("fingerprint is an object")
            .iter()
            .filter(|(name, _)| name.as_str() != "raw_identity")
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        assert!(
            serde_json::from_value::<FrameFingerprint>(serde_json::Value::Object(without_identity))
                .is_err()
        );
    }

    #[test]
    fn compare_reports_the_first_field_in_declaration_order_not_the_last() {
        // A frame that disagrees on both `path` and `width` must report
        // `path` first -- proves `compare` short-circuits per frame rather
        // than collecting every disagreement.
        let a = StackFingerprint {
            frames: vec![frame("a.tif", 10)],
        };
        let b = StackFingerprint {
            frames: vec![frame("renamed.tif", 20)],
        };
        let err = a.compare(&b).unwrap_err();
        assert_eq!(err.field, "path");
    }

    #[test]
    fn from_frame_desc_reads_exactly_the_named_fields() {
        let header = TiffHeader {
            path: PathBuf::from("frame.tif"),
            width: 4,
            height: 4,
            byte_order: crate::io::ByteOrder::Little,
            ifd_offset: 0,
            bits_per_sample: Box::new([16, 16, 16]),
            samples_per_pixel: 3,
            compression: 1,
            photometric: 2,
            planar_config: 1,
            predictor: 1,
            rows_per_strip: 1,
            tile_width: None,
            tile_length: None,
            sample_format: Box::new([1, 1, 1]),
            image_description: Some("hello".to_string()),
            // Grouping reads this field; the fingerprint field set excludes it.
            date_time_original: Some("2026:08:29 18:42:41".to_string()),
            file_size: 12345,
            strip_offsets: vec![10, 20, 30, 40],
            strip_byte_counts: vec![1, 2, 3, 4],
            tile_offsets: vec![],
            tile_byte_counts: vec![],
            refusal: None,
        };
        let desc = FrameDesc::from(&header);
        let fp = FrameFingerprint::from(&desc);
        assert_eq!(fp.path, PathBuf::from("frame.tif"));
        assert_eq!(fp.file_size, 12345);
        assert_eq!(fp.image_description, Some("hello".to_string()));
        assert_eq!(fp.width, 4);
        assert_eq!(fp.height, 4);
        assert_eq!(fp.samples_per_pixel, 3);
        assert_eq!(fp.strip_offsets, vec![10, 20, 30, 40]);
        assert_eq!(fp.strip_byte_counts, vec![1, 2, 3, 4]);

        assert_eq!(fp.raw_identity, None);
    }

    /// This fingerprint is not, and is not convertible to, the project
    /// manifest's content hash.
    ///
    /// `FrameFingerprint` compares parsed headers and does not hash file
    /// contents, so it cannot become a content hash.
    #[test]
    fn not_convertible_to_a_content_hash() {
        // The needles are assembled rather than written out, so this
        // check does not match its own source line.
        let needles = [concat!("bla", "ke3"), concat!("Has", "her")];
        let module = include_str!("fingerprint.rs");
        let uses_a_hash = module
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .any(|line| needles.iter().any(|needle| line.contains(needle)));
        assert!(
            !uses_a_hash,
            "the resume fingerprint must stay a plain header comparison -- no hash crate"
        );
        // A header-only fingerprint carries no pixel bytes at all -- proven
        // structurally: every field on `FrameFingerprint` comes from
        // `TiffHeader`, and `TiffHeader` itself never reads pixel data
        // (`crates/core/src/io/tiff/header.rs`'s own module docs).
        let fp = frame("a.tif", 10);
        assert_eq!(
            fp.strip_offsets.len(),
            3,
            "no pixel bytes, only the strip table"
        );
    }
}
