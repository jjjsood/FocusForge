// SPDX-License-Identifier: GPL-3.0-or-later

//! Packing a [`focusforge_core::tile::Tile`]'s `u16` samples two-per-`u32`
//! for a GPU storage buffer, and back. Not feature-gated: no `wgpu` type
//! appears here, so this arithmetic is testable without the `gpu` feature
//! and without a GPU.
//!
//! Explicit `to_le_bytes` / `from_le_bytes`, never a `bytemuck`-style
//! transmute: the workspace denies `unsafe_code`, and
//! `Tile::samples` is native-endian -- an explicit byte order is the only
//! spelling that stays honest about that.
//!
//! `transfer.rs` documents the exact `map_async` / `get_mapped_range`
//! alignment rule this module's output satisfies by construction: packing to
//! `u32` words means every packed byte length is a multiple of 4, including
//! the odd-sample-count case (5 samples -> 3 words -> 12 bytes: a multiple
//! of 4, not of 8, which is also legal).

/// [`pack_samples_into`] into a freshly allocated `Vec`.
pub fn pack_samples(samples: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(packed_len(samples.len()));
    pack_samples_into(samples, &mut out);
    out
}

/// Packs `samples` two-per-word, low half first, into little-endian bytes,
/// written into `out`. An odd sample count pads the final word's high half
/// with `0`. `out` is cleared first and its capacity kept, so a caller
/// reusing the same `Vec` across calls reallocates only when a pack grows
/// past its current capacity.
pub fn pack_samples_into(samples: &[u16], out: &mut Vec<u8>) {
    out.clear();
    let mut pairs = samples.chunks_exact(2);
    for pair in &mut pairs {
        let word = u32::from(pair[0]) | (u32::from(pair[1]) << 16);
        out.extend_from_slice(&word.to_le_bytes());
    }
    if let [last] = pairs.remainder() {
        let word = u32::from(*last);
        out.extend_from_slice(&word.to_le_bytes());
    }
}

/// The inverse of [`pack_samples`]: unpacks exactly `sample_count` samples
/// from `bytes`, dropping the odd tail's zero pad rather than returning it.
pub fn unpack_samples(bytes: &[u8], sample_count: usize) -> Vec<u16> {
    let mut out = Vec::with_capacity(sample_count);
    for word_bytes in bytes.chunks_exact(4) {
        if out.len() == sample_count {
            break;
        }
        let word = u32::from_le_bytes(
            word_bytes
                .try_into()
                .expect("chunks_exact(4) always yields 4 bytes"),
        );
        out.push((word & 0xffff) as u16);
        if out.len() == sample_count {
            break;
        }
        out.push((word >> 16) as u16);
    }
    out
}

/// The byte length [`pack_samples`] produces for `sample_count` samples:
/// `ceil(sample_count / 2) * 4`, always a multiple of 4.
pub fn packed_len(sample_count: usize) -> usize {
    sample_count.div_ceil(2) * 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_is_exact_for_an_even_sample_count() {
        let samples: Vec<u16> = vec![0, 1, 65535, 12345];

        let bytes = pack_samples(&samples);

        assert_eq!(bytes.len(), packed_len(samples.len()));
        assert_eq!(unpack_samples(&bytes, samples.len()), samples);
    }

    #[test]
    fn round_trip_is_exact_for_an_odd_sample_count_with_zero_padded_tail() {
        let samples: Vec<u16> = vec![10, 20, 30, 40, 50];

        let bytes = pack_samples(&samples);

        assert_eq!(bytes.len(), 12, "5 samples pack into 3 words = 12 bytes");
        let last_word = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        assert_eq!(last_word >> 16, 0, "odd tail's high half is zero-padded");
        assert_eq!(unpack_samples(&bytes, samples.len()), samples);
    }

    #[test]
    fn packed_len_of_an_odd_word_count_is_a_multiple_of_four_never_eight() {
        // 5 samples -> 3 words -> 12 bytes: legal for `map_async` (a
        // multiple of 4) without needing to be a multiple of 8.
        assert_eq!(packed_len(5), 12);
        assert_eq!(12 % 4, 0);
        assert_ne!(12 % 8, 0);
    }

    #[test]
    fn packed_len_of_an_even_word_count() {
        assert_eq!(packed_len(4), 8);
        assert_eq!(packed_len(0), 0);
    }

    #[test]
    fn pack_samples_into_matches_pack_samples_for_even_and_odd_counts() {
        for samples in [&[0u16, 1, 65535, 12345][..], &[10, 20, 30, 40, 50][..]] {
            let mut out = Vec::new();
            pack_samples_into(samples, &mut out);
            assert_eq!(out, pack_samples(samples));
        }
    }

    #[test]
    fn pack_samples_into_a_shorter_refill_leaves_no_stale_tail() {
        let long: Vec<u16> = (0..20).collect();
        let short: Vec<u16> = vec![1, 2, 3];

        let mut out = Vec::new();
        pack_samples_into(&long, &mut out);
        let capacity_after_long = out.capacity();

        pack_samples_into(&short, &mut out);

        assert_eq!(out, pack_samples(&short));
        assert_eq!(
            out.capacity(),
            capacity_after_long,
            "refilling shorter must keep the capacity from the longer pack"
        );
    }
}
