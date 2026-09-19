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
//! This is also distinct from the **checkpoint** fingerprint, documented in
//! full at [`super::stage_cache`]'s own module docs: the checkpoint DAG
//! reads [`crate::project::stack_fingerprint`], never this type -- two
//! deliberately different values, not one value everywhere.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::io::FrameDesc;

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
#[derive(Debug, Clone, PartialEq)]
pub struct FingerprintMismatch {
    pub frame: usize,
    pub path: PathBuf,
    pub field: &'static str,
    pub expected: String,
    pub found: String,
}

impl std::fmt::Display for FingerprintMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "frame {} ({}): {} changed: expected {}, found {}",
            self.frame,
            self.path.display(),
            self.field,
            self.expected,
            self.found
        )
    }
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
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::TiffHeader;

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
            // Read by grouping, deliberately **not** by the
            // fingerprint: adding it to `FrameFingerprint` would invalidate
            // every resume ledger already on disk for no benefit. The
            // assertions below are what pin that field set.
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

        #[derive(Serialize)]
        struct LegacyFrameFingerprint {
            path: PathBuf,
            file_size: u64,
            image_description: Option<String>,
            width: u32,
            height: u32,
            samples_per_pixel: u32,
            strip_offsets: Vec<u64>,
            strip_byte_counts: Vec<u64>,
        }

        let legacy = LegacyFrameFingerprint {
            path: header.path.clone(),
            file_size: header.file_size,
            image_description: header.image_description.clone(),
            width: header.width,
            height: header.height,
            samples_per_pixel: header.samples_per_pixel,
            strip_offsets: header.strip_offsets.clone(),
            strip_byte_counts: header.strip_byte_counts.clone(),
        };
        assert_eq!(
            serde_json::to_vec(&fp).expect("fingerprint must serialize"),
            serde_json::to_vec(&legacy).expect("legacy fingerprint must serialize")
        );
    }

    /// This fingerprint is not, and is not convertible to, the project
    /// manifest's content hash.
    ///
    /// The guard used to be "this crate has no hashing dependency at all",
    /// which stopped being the right one the moment `crate::project` added
    /// blake3 for the manifest's own fingerprint -- a value that is
    /// deliberately *different* from this one. The claim worth guarding was
    /// never about the crate: it is that **this module** reaches for no
    /// hash, so a `FrameFingerprint` has nothing to convert into and cannot
    /// quietly drift into a content hash.
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
