// SPDX-License-Identifier: GPL-3.0-or-later

//! Whether two fused states share one pixel grid: the same source frames, the
//! same alignment, the same crop and the same dimensions.
//! [`PixelGrid::mismatch`] names the first attribute that differs, in that
//! order. Brightness gains are not part of the grid.

use std::fmt;
use std::path::PathBuf;

use crate::align::Registration;
use crate::job::stage_cache::transform_to_bits;
use crate::tile::Rect;

use super::seal::HeldCheckpoint;

/// The attribute that keeps a candidate checkpoint from sharing the edited
/// state's pixel grid. When several differ, the first in declaration order
/// is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridMismatch {
    SourceFrames,
    Alignment,
    Crop,
    Dimensions,
}

impl fmt::Display for GridMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SourceFrames => "different source frames",
            Self::Alignment => "different alignment",
            Self::Crop => "different crop",
            Self::Dimensions => "different dimensions",
        })
    }
}

/// What a fuse contributes to its pixel grid, borrowed from a saved fuse
/// record ([`super::Project::pixel_grid`]) or a held one ([`Self::held`]).
#[derive(Debug, Clone)]
pub struct PixelGrid<'a> {
    /// Content hash per source frame in frame order; `None` for a held fuse,
    /// which has not hashed its files.
    pub hashes: Option<Vec<&'a str>>,
    /// Where each source frame is read from, in frame order.
    pub paths: Vec<PathBuf>,
    pub registration: &'a Registration,
    /// The composite's extent in pixels, after any crop.
    pub width: u32,
    pub height: u32,
}

impl<'a> PixelGrid<'a> {
    /// The grid of a fuse held outside any container.
    pub fn held(held: &'a HeldCheckpoint) -> Self {
        Self {
            hashes: None,
            paths: held.sources.clone(),
            registration: &held.registration,
            width: held.width,
            height: held.height,
        }
    }

    /// Why `other` does not share this grid, `None` when it does. Source
    /// frames compare by content hash when both sides carry one, otherwise
    /// by path; a crop of `None` equals a full-frame rectangle of its own
    /// extent; transforms compare bit for bit with missing entries read as
    /// identity.
    pub fn mismatch(&self, other: &PixelGrid<'_>) -> Option<GridMismatch> {
        let same_frames = match (&self.hashes, &other.hashes) {
            (Some(a), Some(b)) => a == b,
            _ => self.paths == other.paths,
        };
        if !same_frames {
            return Some(GridMismatch::SourceFrames);
        }
        let frames = self.paths.len();
        let bits = |g: &PixelGrid<'_>| -> Vec<_> {
            g.registration
                .transforms(frames)
                .iter()
                .map(transform_to_bits)
                .collect()
        };
        if bits(self) != bits(other) {
            return Some(GridMismatch::Alignment);
        }
        let both_uncropped = self.registration.crop.is_none() && other.registration.crop.is_none();
        if !both_uncropped && self.crop() != other.crop() {
            return Some(GridMismatch::Crop);
        }
        if (self.width, self.height) != (other.width, other.height) {
            return Some(GridMismatch::Dimensions);
        }
        None
    }

    fn crop(&self) -> Rect {
        self.registration.crop.unwrap_or(Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::align::Transform;

    fn reg(transforms: Vec<Transform>, crop: Option<Rect>) -> Registration {
        Registration {
            transforms,
            crop,
            equalization: None,
        }
    }

    fn grid<'a>(hashes: &[&'a str], reg: &'a Registration, w: u32, h: u32) -> PixelGrid<'a> {
        PixelGrid {
            hashes: Some(hashes.to_vec()),
            paths: (0..hashes.len())
                .map(|i| PathBuf::from(format!("/frames/f{i}.tif")))
                .collect(),
            registration: reg,
            width: w,
            height: h,
        }
    }

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn both_ways(a: &PixelGrid<'_>, b: &PixelGrid<'_>, want: Option<GridMismatch>) {
        assert_eq!(a.mismatch(b), want, "edited vs candidate");
        assert_eq!(b.mismatch(a), want, "candidate vs edited");
    }

    #[test]
    fn identical_grids_are_compatible() {
        let r = reg(vec![], None);
        both_ways(
            &grid(&["a", "b"], &r, 64, 48),
            &grid(&["a", "b"], &r, 64, 48),
            None,
        );
    }

    #[test]
    fn another_source_list_is_source_frames() {
        let r = reg(vec![], None);
        both_ways(
            &grid(&["a", "b"], &r, 64, 48),
            &grid(&["a", "c"], &r, 64, 48),
            Some(GridMismatch::SourceFrames),
        );
        both_ways(
            &grid(&["a", "b"], &r, 64, 48),
            &grid(&["a", "b", "c"], &r, 64, 48),
            Some(GridMismatch::SourceFrames),
        );
        both_ways(
            &grid(&["a", "b"], &r, 64, 48),
            &grid(&["b", "a"], &r, 64, 48),
            Some(GridMismatch::SourceFrames),
        );
    }

    #[test]
    fn one_shifted_transform_is_alignment() {
        let a = reg(vec![], None);
        let b = reg(
            vec![Transform::identity(), Transform::translation(0.5, 0.0)],
            None,
        );
        both_ways(
            &grid(&["a", "b"], &a, 64, 48),
            &grid(&["a", "b"], &b, 64, 48),
            Some(GridMismatch::Alignment),
        );
    }

    #[test]
    fn another_crop_is_crop() {
        let a = reg(vec![], Some(rect(0, 0, 32, 24)));
        let b = reg(vec![], Some(rect(0, 0, 40, 24)));
        let moved = reg(vec![], Some(rect(8, 4, 32, 24)));
        both_ways(
            &grid(&["a"], &a, 32, 24),
            &grid(&["a"], &b, 32, 24),
            Some(GridMismatch::Crop),
        );
        both_ways(
            &grid(&["a"], &a, 32, 24),
            &grid(&["a"], &moved, 32, 24),
            Some(GridMismatch::Crop),
        );
    }

    #[test]
    fn other_composite_extent_is_dimensions() {
        let r = reg(vec![], None);
        both_ways(
            &grid(&["a"], &r, 64, 48),
            &grid(&["a"], &r, 64, 49),
            Some(GridMismatch::Dimensions),
        );
        both_ways(
            &grid(&["a"], &r, 64, 48),
            &grid(&["a"], &r, 65, 48),
            Some(GridMismatch::Dimensions),
        );
    }

    #[test]
    fn several_differences_report_the_first_in_order() {
        let shifted = reg(
            vec![Transform::translation(1.0, 0.0)],
            Some(rect(1, 1, 30, 20)),
        );
        let plain = reg(vec![], None);
        // Sources and crop differ: sources win.
        both_ways(
            &grid(&["a"], &plain, 64, 48),
            &grid(&["z"], &shifted, 30, 20),
            Some(GridMismatch::SourceFrames),
        );
        // Alignment and crop differ: alignment wins.
        both_ways(
            &grid(&["a"], &plain, 64, 48),
            &grid(&["a"], &shifted, 64, 48),
            Some(GridMismatch::Alignment),
        );
        // Crop and dimensions differ: crop wins.
        let cropped = reg(vec![], Some(rect(2, 2, 30, 20)));
        both_ways(
            &grid(&["a"], &plain, 64, 48),
            &grid(&["a"], &cropped, 30, 20),
            Some(GridMismatch::Crop),
        );
    }

    #[test]
    fn saved_sources_match_by_hash_not_path() {
        let r = reg(vec![], None);
        let a = grid(&["a", "b"], &r, 64, 48);
        let mut b = grid(&["a", "b"], &r, 64, 48);
        b.paths = vec![PathBuf::from("/moved/x.tif"), PathBuf::from("/moved/y.tif")];
        both_ways(&a, &b, None);
    }

    #[test]
    fn empty_transforms_equal_explicit_identities_over_five_frames() {
        let empty = reg(vec![], None);
        let explicit = reg(vec![Transform::identity(); 5], None);
        let hashes = ["a", "b", "c", "d", "e"];
        both_ways(
            &grid(&hashes, &empty, 64, 48),
            &grid(&hashes, &explicit, 64, 48),
            None,
        );
        let mut last_moved = vec![Transform::identity(); 4];
        last_moved.push(Transform::translation(0.0, 2.0));
        let moved = reg(last_moved, None);
        both_ways(
            &grid(&hashes, &empty, 64, 48),
            &grid(&hashes, &moved, 64, 48),
            Some(GridMismatch::Alignment),
        );
    }

    #[test]
    fn full_frame_crop_equals_no_crop() {
        let none = reg(vec![], None);
        let full = reg(vec![], Some(rect(0, 0, 37, 29)));
        both_ways(
            &grid(&["a"], &none, 37, 29),
            &grid(&["a"], &full, 37, 29),
            None,
        );
    }

    #[test]
    fn same_size_crop_at_another_origin_is_crop() {
        let a = reg(vec![], Some(rect(0, 0, 20, 10)));
        let b = reg(vec![], Some(rect(3, 0, 20, 10)));
        both_ways(
            &grid(&["a"], &a, 20, 10),
            &grid(&["a"], &b, 20, 10),
            Some(GridMismatch::Crop),
        );
    }

    #[test]
    fn equalization_gains_are_not_part_of_the_grid() {
        let plain = reg(vec![], None);
        let mut gained = reg(vec![], None);
        gained.equalization = Some(crate::equalize::Equalization {
            max_gain: 2.0,
            target_stops: 0.0,
            gains: vec![70_000],
        });
        both_ways(
            &grid(&["a"], &plain, 8, 8),
            &grid(&["a"], &gained, 8, 8),
            None,
        );
    }

    #[test]
    fn held_grids_compare_by_path() {
        let r = reg(vec![], None);
        let held = |paths: &[&str], w: u32| PixelGrid {
            hashes: None,
            paths: paths.iter().map(PathBuf::from).collect(),
            registration: &r,
            width: w,
            height: 10,
        };
        both_ways(
            &held(&["/s/a", "/s/b"], 8),
            &held(&["/s/a", "/s/b"], 8),
            None,
        );
        both_ways(
            &held(&["/s/a", "/s/b"], 8),
            &held(&["/t/a", "/t/b"], 8),
            Some(GridMismatch::SourceFrames),
        );
        both_ways(
            &held(&["/s/a"], 8),
            &held(&["/s/a"], 9),
            Some(GridMismatch::Dimensions),
        );
    }

    #[test]
    fn held_against_saved_compares_the_saved_paths() {
        let r = reg(vec![], None);
        let saved = grid(&["a"], &r, 8, 8);
        let same_files = PixelGrid {
            hashes: None,
            paths: saved.paths.clone(),
            registration: &r,
            width: 8,
            height: 8,
        };
        let copies = PixelGrid {
            paths: vec![PathBuf::from("/copy/f0.tif")],
            ..same_files.clone()
        };
        both_ways(&saved, &same_files, None);
        both_ways(&saved, &copies, Some(GridMismatch::SourceFrames));
    }

    #[test]
    fn reasons_render_as_plain_language() {
        assert_eq!(
            GridMismatch::SourceFrames.to_string(),
            "different source frames"
        );
        assert_eq!(GridMismatch::Alignment.to_string(), "different alignment");
        assert_eq!(GridMismatch::Crop.to_string(), "different crop");
        assert_eq!(GridMismatch::Dimensions.to_string(), "different dimensions");
    }
}
