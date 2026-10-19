// SPDX-License-Identifier: GPL-3.0-or-later

//! The Z-Map spread measures `focusforge-cli review` reports: how many
//! source frames are actually used, and how spatially coherent the
//! selection is.
//!
//! [`all_eight_identical_fraction`] is the whole-buffer definition; the
//! measured number is pinned by a test in
//! `crates/core/tests/laplacian_fusion.rs`. [`SpreadScan`] is the
//! streaming form `review` needs: a 44.8 MP Z-Map never fits as one buffer,
//! so it is scored band by band with a one-row carry, and
//! [`streaming and whole-buffer agree bit for bit`](crate) is asserted in
//! `crates/core/tests/review_metrics.rs`.
//!
//! The **majority-agreement** coherence measure (`spatial_coherence_fraction`
//! in `crates/core/tests/laplacian_fusion.rs`) is not part of this module:
//! it passes a map that is, by the all-8-identical measure, noise, so
//! `review` never reports it.

use super::NO_SOURCE;
use crate::tile::Rect;

/// The fraction of pixels whose **entire** (present) 8-neighbourhood shares
/// the pixel's own winner -- the discriminating coherence measure: a random
/// 8-way map scores ~12.5% here. Border convention: an out-of-range neighbour is
/// skipped, never counted as a mismatch, so an edge or corner pixel needs
/// only its *present* neighbours to agree, never all 8 -- a different
/// border rule shifts this fraction by ~0.1 points, so the convention is
/// pinned here.
pub fn all_eight_identical_fraction(indices: &[u16], width: u32, height: u32) -> f64 {
    let mut identical = 0usize;
    let total = (width * height) as usize;
    for y in 0..height as i64 {
        for x in 0..width as i64 {
            let center = indices[(y as u32 * width + x as u32) as usize];
            let mut all_match = true;
            'neighbours: for dy in -1i64..=1 {
                for dx in -1i64..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || ny < 0 || nx >= width as i64 || ny >= height as i64 {
                        continue;
                    }
                    let idx = ny as u32 * width + nx as u32;
                    if indices[idx as usize] != center {
                        all_match = false;
                        break 'neighbours;
                    }
                }
            }
            if all_match {
                identical += 1;
            }
        }
    }
    identical as f64 / total as f64
}

/// The distinct-source count and the all-8-identical coherence fraction of
/// one Z-Map, plus the pixel count they were measured over.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZMapSpread {
    /// How many distinct source indices were seen. [`NO_SOURCE`] is never
    /// counted (it means "no single source", not a source of its own).
    pub distinct_sources: u32,
    /// The all-8-identical fraction, [`all_eight_identical_fraction`]'s
    /// definition, computed identically whether the plane arrived as one
    /// buffer or as many bands.
    pub coherence: f64,
    pub pixels: u64,
}

/// The streaming form of [`all_eight_identical_fraction`] plus the
/// distinct-source count, fed one full-width band at a time in top-to-bottom
/// order -- the shape [`crate::project::Project::read_plane_into`] hands
/// out.
///
/// Two internals the size of a full-resolution Z-Map forces: distinct
/// sources counted in a `[u64; 1024]` bitset (8 KB, one bit per `u16` index
/// -- never a `HashSet` over 44.8 M entries), and a **one-row carry**: a
/// row's 8-neighbourhood spans the band boundary above and below, so a row
/// is scored only once the row after it has arrived, and the very last row
/// is scored at [`Self::finish`] from its present neighbours -- the same
/// border rule the whole-buffer form uses.
pub struct SpreadScan {
    width: u32,
    height: u32,
    /// One bit per possible `u16` index (`NO_SOURCE` is never set).
    sources: Box<[u64; 1024]>,
    /// The row directly above `held`, if any has arrived yet.
    prev: Option<Vec<u16>>,
    /// The row waiting for the row after it before it can be scored.
    held: Option<Vec<u16>>,
    held_y: u32,
    identical: u64,
    scored_pixels: u64,
}

impl SpreadScan {
    pub fn new(width: u32, height: u32) -> Self {
        SpreadScan {
            width,
            height,
            sources: Box::new([0u64; 1024]),
            prev: None,
            held: None,
            held_y: 0,
            identical: 0,
            scored_pixels: 0,
        }
    }

    /// Feeds one band, `rect.height` rows of `rect.width * rect.height`
    /// indices, in top-to-bottom order. `rect.width` must equal the width
    /// this scan was built with and `rect.x` must be `0` -- the same
    /// full-width-band contract [`crate::project::Project::read_plane_into`]
    /// guarantees.
    pub fn push_band(&mut self, indices: &[u16], rect: Rect) {
        debug_assert_eq!(rect.width, self.width, "a band must span the full width");
        debug_assert_eq!(rect.x, 0, "a band must start at the left edge");
        debug_assert!(
            rect.y + rect.height <= self.height,
            "a band must not extend past the scan's own height"
        );
        let width = self.width as usize;
        for row_index in 0..rect.height as usize {
            let y = rect.y + row_index as u32;
            let row = &indices[row_index * width..(row_index + 1) * width];
            for &index in row {
                if index != NO_SOURCE {
                    self.sources[(index as usize) >> 6] |= 1 << (index & 63);
                }
            }
            if let Some(held) = self.held.take() {
                let identical = score_row(width, self.prev.as_deref(), &held, Some(row));
                self.identical += identical;
                self.scored_pixels += width as u64;
                self.prev = Some(held);
            }
            self.held = Some(row.to_vec());
            self.held_y = y;
        }
    }

    /// Scores the final held row (which never received a "next" row) and
    /// returns the totals.
    pub fn finish(mut self) -> ZMapSpread {
        if let Some(held) = self.held.take() {
            let width = self.width as usize;
            let identical = score_row(width, self.prev.as_deref(), &held, None);
            self.identical += identical;
            self.scored_pixels += width as u64;
        }
        let distinct_sources = self.sources.iter().map(|word| word.count_ones()).sum();
        let coherence = if self.scored_pixels == 0 {
            0.0
        } else {
            self.identical as f64 / self.scored_pixels as f64
        };
        ZMapSpread {
            distinct_sources,
            coherence,
            pixels: self.scored_pixels,
        }
    }
}

/// Scores one row against its (possibly absent) neighbour rows, using
/// exactly [`all_eight_identical_fraction`]'s border convention: an absent
/// row (`prev`/`next` is `None`, the top/bottom edge) or an out-of-range
/// column is skipped, never counted as a mismatch.
fn score_row(width: usize, prev: Option<&[u16]>, row: &[u16], next: Option<&[u16]>) -> u64 {
    let mut identical = 0u64;
    for x in 0..width {
        let center = row[x];
        let mut all_match = true;
        'neighbours: for dy in -1i64..=1 {
            let candidate_row = match dy {
                -1 => prev,
                0 => Some(row),
                1 => next,
                _ => unreachable!(),
            };
            let Some(candidate_row) = candidate_row else {
                continue;
            };
            for dx in -1i64..=1 {
                if dx == 0 && dy == 0 {
                    continue;
                }
                let nx = x as i64 + dx;
                if nx < 0 || nx >= width as i64 {
                    continue;
                }
                if candidate_row[nx as usize] != center {
                    all_match = false;
                    break 'neighbours;
                }
            }
        }
        if all_match {
            identical += 1;
        }
    }
    identical
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_eight_identical_fraction_of_a_uniform_map_is_one() {
        let (w, h) = (4u32, 4u32);
        let indices = vec![3u16; (w * h) as usize];
        assert_eq!(all_eight_identical_fraction(&indices, w, h), 1.0);
    }

    #[test]
    fn spread_scan_matches_whole_buffer_on_a_small_checkerboard() {
        let (w, h) = (6u32, 5u32);
        let indices: Vec<u16> = (0..(w * h)).map(|i| (i % 3) as u16).collect();
        let expected = all_eight_identical_fraction(&indices, w, h);

        let mut scan = SpreadScan::new(w, h);
        scan.push_band(
            &indices,
            Rect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            },
        );
        let spread = scan.finish();
        assert_eq!(spread.coherence, expected);
        assert_eq!(spread.pixels, (w * h) as u64);
    }
}
