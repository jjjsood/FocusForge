// SPDX-License-Identifier: GPL-3.0-or-later

//! The set of sensor positions to repair, as one bit per pixel in sensor
//! coordinates: [`DefectMask`]. Automatic detection ([`crate::hot_pixels`])
//! and a user-painted mask file ([`load`]) both produce one.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use crate::equalize::strip_rows_for;
use crate::io::{JPEG_EXTENSIONS, PNG_EXTENSIONS, TIFF_EXTENSIONS, has_extension, open_frame_set};
use crate::tile::{HostBudget, Rect};

/// A set of sensor positions, one bit per pixel of a `width` × `height` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefectMask {
    width: u32,
    height: u32,
    bits: Vec<u64>,
}

impl DefectMask {
    /// An empty mask for a `width` × `height` frame.
    pub fn new(width: u32, height: u32) -> Self {
        let pixels = width as usize * height as usize;
        DefectMask {
            width,
            height,
            bits: vec![0; pixels.div_ceil(64)],
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// How many positions are set.
    pub fn count(&self) -> usize {
        self.bits
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    /// Whether `(x, y)` is set; a position outside the frame is not.
    pub fn contains(&self, x: u32, y: u32) -> bool {
        x < self.width && y < self.height && {
            let at = self.index(x, y);
            self.bits[at / 64] >> (at % 64) & 1 == 1
        }
    }

    /// Sets `(x, y)`.
    ///
    /// # Panics
    /// When the position is outside the frame.
    pub fn insert(&mut self, x: u32, y: u32) {
        assert!(
            x < self.width && y < self.height,
            "({x}, {y}) is outside the {}x{} defect mask",
            self.width,
            self.height
        );
        let at = self.index(x, y);
        self.bits[at / 64] |= 1 << (at % 64);
    }

    /// Sets every position set in `other`.
    ///
    /// # Panics
    /// When the masks are not the same size.
    pub fn union_with(&mut self, other: &DefectMask) {
        assert_eq!(
            (self.width, self.height),
            (other.width, other.height),
            "a union needs masks of the same size"
        );
        for (word, other) in self.bits.iter_mut().zip(&other.bits) {
            *word |= other;
        }
    }

    /// A blake3 digest, 64 hex characters, of the frame size and the set
    /// positions: equal for equal masks and different for any other.
    pub fn digest(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.width.to_le_bytes());
        hasher.update(&self.height.to_le_bytes());
        for word in &self.bits {
            hasher.update(&word.to_le_bytes());
        }
        hasher.finalize().to_hex().to_string()
    }

    /// The set positions as `(x, y)`, row by row.
    pub fn positions(&self) -> Vec<(u32, u32)> {
        let width = self.width as usize;
        let mut out = Vec::with_capacity(self.count());
        for (word_at, &word) in self.bits.iter().enumerate() {
            let mut rest = word;
            while rest != 0 {
                let at = word_at * 64 + rest.trailing_zeros() as usize;
                out.push(((at % width) as u32, (at / width) as u32));
                rest &= rest - 1;
            }
        }
        out
    }

    /// Every position within Euclidean distance `radius` of a set one
    /// (`dx² + dy² ≤ radius²`), clipped at the frame edge. A radius of 0 keeps
    /// the mask as it is. Each row is reduced to its horizontal distance to
    /// the nearest set pixel, kept for the `2 * radius + 1` rows that can reach
    /// the output row: cost is pixels × (2 × radius + 1), no per-defect work.
    pub fn grow(&self, radius: u32) -> DefectMask {
        if radius == 0 || self.count() == 0 {
            return self.clone();
        }
        let (width, height) = (self.width as usize, self.height as usize);
        let reach_rows = radius as usize;
        let ring = 2 * reach_rows + 1;
        let cap = (radius + 1) as u8;
        let mut distances = vec![cap; ring * width];
        let mut has_set = vec![false; ring];
        let mut out = DefectMask::new(self.width, self.height);
        let mut hit = vec![false; width];
        let mut computed = 0;
        for y in 0..height {
            while computed < height && computed <= y + reach_rows {
                let slot = computed % ring;
                has_set[slot] =
                    self.row_distances(computed, cap, &mut distances[slot * width..][..width]);
                computed += 1;
            }
            hit.fill(false);
            for source in y.saturating_sub(reach_rows)..=(y + reach_rows).min(height - 1) {
                let slot = source % ring;
                if !has_set[slot] {
                    continue;
                }
                let dy = y.abs_diff(source);
                let reach = (reach_rows * reach_rows - dy * dy).isqrt() as u8;
                for (hit, &distance) in hit.iter_mut().zip(&distances[slot * width..][..width]) {
                    *hit |= distance <= reach;
                }
            }
            for (x, _) in hit.iter().enumerate().filter(|&(_, &on)| on) {
                out.insert(x as u32, y as u32);
            }
        }
        out
    }

    /// Writes each pixel's horizontal distance to the nearest set pixel of
    /// `row` into `out`, saturating at `cap`; false when the row has none.
    fn row_distances(&self, row: usize, cap: u8, out: &mut [u8]) -> bool {
        let base = row * self.width as usize;
        let set = |x: usize| self.bits[(base + x) / 64] >> ((base + x) % 64) & 1 == 1;
        let mut any = false;
        let mut distance = cap;
        for (x, slot) in out.iter_mut().enumerate() {
            distance = if set(x) {
                any = true;
                0
            } else {
                (distance + 1).min(cap)
            };
            *slot = distance;
        }
        if any {
            let mut distance = cap;
            for (x, slot) in out.iter_mut().enumerate().rev() {
                distance = if set(x) { 0 } else { (distance + 1).min(cap) };
                *slot = (*slot).min(distance);
            }
        }
        any
    }

    fn index(&self, x: u32, y: u32) -> usize {
        y as usize * self.width as usize + x as usize
    }
}

/// Half of full scale on the 16-bit scale every source is read at (8-bit
/// samples arrive ×257): a colour sample below it is dark.
const HALF: u16 = 1 << 15;

/// Why a defect mask file could not become a [`DefectMask`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DefectMaskError {
    /// The mask is not exactly the frames' size; it is never cropped, scaled
    /// or rotated to fit.
    #[error(
        "{}: the defect mask is {}×{} pixels but the frames are {}×{}; the mask must be the frames' size",
        path.display(), mask.0, mask.1, frames.0, frames.1
    )]
    SizeMismatch {
        /// The file.
        path: PathBuf,
        /// The mask's width and height.
        mask: (u32, u32),
        /// The frames' width and height.
        frames: (u32, u32),
    },
    /// The file is not a TIFF, PNG or JPEG by its extension.
    #[error("{}: a defect mask must be a TIFF, PNG or JPEG file", path.display())]
    UnsupportedFormat {
        /// The file.
        path: PathBuf,
    },
    /// The file is missing or not a readable image of its kind.
    #[error("cannot read the defect mask {}: {source}", path.display())]
    Unreadable {
        /// The file.
        path: PathBuf,
        /// What the decoder reported.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

fn unreadable(
    path: &Path,
    source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> DefectMaskError {
    DefectMaskError::Unreadable {
        path: path.to_path_buf(),
        source: source.into(),
    }
}

/// Reads the defect mask `path` for frames of `frame` (width, height),
/// grown by `radius` pixels. A TIFF (8 or 16 bit), PNG or JPEG, grey or
/// colour: a pixel is a defect when every colour channel is below half of
/// full scale and, when there is an alpha channel, alpha is at or above half
/// (transparent means unpainted). A TIFF with 2 or 4 samples per pixel takes
/// its last sample as alpha. The size is checked before any pixel is read.
pub fn load(
    path: &Path,
    frame: (u32, u32),
    radius: u32,
    budget: HostBudget,
) -> Result<DefectMask, DefectMaskError> {
    let painted = if has_extension(path, PNG_EXTENSIONS) {
        decode_png(path, frame)?
    } else if has_extension(path, TIFF_EXTENSIONS) || has_extension(path, JPEG_EXTENSIONS) {
        decode_frames(path, frame, None, budget)?
    } else {
        return Err(DefectMaskError::UnsupportedFormat {
            path: path.to_path_buf(),
        });
    };
    Ok(painted.grow(radius))
}

/// Marks the defects of `row`, the interleaved samples of row `y`.
fn mark_row(mask: &mut DefectMask, y: u32, samples_per_pixel: usize, row: &[u16]) {
    let colours = if samples_per_pixel.is_multiple_of(2) {
        samples_per_pixel - 1
    } else {
        samples_per_pixel
    };
    for (x, pixel) in row.chunks_exact(samples_per_pixel).enumerate() {
        let dark = pixel[..colours].iter().all(|&sample| sample < HALF);
        if dark && (colours == samples_per_pixel || pixel[colours] >= HALF) {
            mask.insert(x as u32, y);
        }
    }
}

/// A TIFF or JPEG mask through the frame readers, `band_rows` full-width rows
/// at a time (`None`: the strip height the other readers use).
fn decode_frames(
    path: &Path,
    frame: (u32, u32),
    band_rows: Option<u32>,
    budget: HostBudget,
) -> Result<DefectMask, DefectMaskError> {
    let set = open_frame_set(&[path.to_path_buf()], budget).map_err(|e| unreadable(path, e))?;
    let reader = set.reader.as_ref();
    let size = reader.dimensions();
    if size != frame {
        return Err(DefectMaskError::SizeMismatch {
            path: path.to_path_buf(),
            mask: size,
            frames: frame,
        });
    }
    let samples_per_pixel = reader.frame(0).samples_per_pixel;
    let rows = band_rows
        .unwrap_or_else(|| strip_rows_for(frame, samples_per_pixel))
        .max(1);
    let row_samples = frame.0 as usize * samples_per_pixel as usize;
    let mut band = vec![0u16; rows as usize * row_samples];
    let mut mask = DefectMask::new(frame.0, frame.1);
    let mut top = 0;
    while top < frame.1 {
        let height = rows.min(frame.1 - top);
        let samples = &mut band[..height as usize * row_samples];
        let rect = Rect {
            x: 0,
            y: top,
            width: frame.0,
            height,
        };
        reader
            .read_tile_into(0, rect, samples)
            .map_err(|e| unreadable(path, e))?;
        for (row, samples) in samples.chunks_exact(row_samples).enumerate() {
            let y = top + row as u32;
            mark_row(&mut mask, y, samples_per_pixel as usize, samples);
        }
        top += height;
    }
    Ok(mask)
}

/// A PNG mask: palette and low-depth grey expand to 8-bit samples, 16-bit
/// samples stay as they are.
fn decode_png(path: &Path, frame: (u32, u32)) -> Result<DefectMask, DefectMaskError> {
    let file = File::open(path).map_err(|e| unreadable(path, e))?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().map_err(|e| unreadable(path, e))?;
    let size = (reader.info().width, reader.info().height);
    if size != frame {
        return Err(DefectMaskError::SizeMismatch {
            path: path.to_path_buf(),
            mask: size,
            frames: frame,
        });
    }
    let buffer_size = reader
        .output_buffer_size()
        .ok_or_else(|| unreadable(path, "the image is too large to decode"))?;
    let mut pixels = vec![0; buffer_size];
    let info = reader
        .next_frame(&mut pixels)
        .map_err(|e| unreadable(path, e))?;
    let samples_per_pixel = info.color_type.samples();
    let mut mask = DefectMask::new(frame.0, frame.1);
    let mut row = vec![0u16; frame.0 as usize * samples_per_pixel];
    for (y, line) in pixels.chunks_exact(info.line_size).enumerate() {
        if info.bit_depth == png::BitDepth::Sixteen {
            for (sample, bytes) in row.iter_mut().zip(line.chunks_exact(2)) {
                *sample = u16::from_be_bytes([bytes[0], bytes[1]]);
            }
        } else {
            for (sample, &byte) in row.iter_mut().zip(line) {
                *sample = u16::from(byte) * 257;
            }
        }
        mark_row(&mut mask, y as u32, samples_per_pixel, &row);
    }
    Ok(mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{MetadataTags, StripCompression, StripSpec, StripWriter};

    /// Every pixel within `radius` of a set pixel, by walking each disc.
    fn brute_grow(mask: &DefectMask, radius: u32) -> DefectMask {
        let r = i64::from(radius);
        let mut out = DefectMask::new(mask.width(), mask.height());
        for (x, y) in mask.positions() {
            for dy in -r..=r {
                for dx in -r..=r {
                    let (nx, ny) = (i64::from(x) + dx, i64::from(y) + dy);
                    let inside = (0..i64::from(mask.width())).contains(&nx)
                        && (0..i64::from(mask.height())).contains(&ny);
                    if inside && dx * dx + dy * dy <= r * r {
                        out.insert(nx as u32, ny as u32);
                    }
                }
            }
        }
        out
    }

    /// A deterministic mask where about `percent` of the pixels are set.
    fn random_mask(width: u32, height: u32, percent: u64, seed: u64) -> DefectMask {
        let mut state = seed;
        let mut mask = DefectMask::new(width, height);
        for y in 0..height {
            for x in 0..width {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                if (state >> 33) % 100 < percent {
                    mask.insert(x, y);
                }
            }
        }
        mask
    }

    #[test]
    fn grow_matches_a_brute_force_disc_for_every_radius() {
        for (percent, seed) in [(1, 7), (4, 11), (30, 3)] {
            let mask = random_mask(131, 47, percent, seed);
            for radius in 0..=16 {
                assert_eq!(
                    mask.grow(radius),
                    brute_grow(&mask, radius),
                    "{percent}% set, radius {radius}"
                );
            }
        }
    }

    #[test]
    fn union_sets_every_position_of_either_mask() {
        let a = random_mask(131, 47, 5, 1);
        let b = random_mask(131, 47, 5, 2);
        let mut union = a.clone();
        union.union_with(&b);
        let mut want = a.positions();
        want.extend(b.positions());
        want.sort_unstable_by_key(|&(x, y)| (y, x));
        want.dedup();
        assert_eq!(union.positions(), want);
    }

    #[test]
    #[should_panic(expected = "same size")]
    fn union_refuses_a_mask_of_another_size() {
        DefectMask::new(8, 8).union_with(&DefectMask::new(8, 9));
    }

    #[test]
    fn digest_tells_masks_and_sizes_apart() {
        let mut one = DefectMask::new(10, 6);
        one.insert(3, 2);
        let mut other = DefectMask::new(10, 6);
        other.insert(4, 2);
        // The same 60 bits read as another frame shape.
        let mut reshaped = DefectMask::new(6, 10);
        reshaped.insert(3, 2);
        let digests = [
            one.digest(),
            other.digest(),
            reshaped.digest(),
            DefectMask::new(10, 6).digest(),
        ];
        for (at, digest) in digests.iter().enumerate() {
            assert_eq!(digest.len(), 64);
            assert!(
                digests[..at].iter().all(|earlier| earlier != digest),
                "{at}"
            );
        }
        assert_eq!(one.digest(), one.clone().digest());
    }

    #[test]
    fn grow_is_a_disc_not_a_square() {
        let mut mask = DefectMask::new(9, 9);
        mask.insert(4, 4);
        assert_eq!(mask.grow(1).count(), 5);
        assert_eq!(mask.grow(2).count(), 13);
        assert!(!mask.grow(2).contains(6, 6));
    }

    #[test]
    fn grow_is_clipped_at_every_border() {
        let (width, height) = (70, 20);
        for (x, y) in [
            (0, 0),
            (width - 1, 0),
            (0, height - 1),
            (width - 1, height - 1),
            (30, 0),
            (30, height - 1),
            (0, 9),
            (width - 1, 9),
        ] {
            let mut mask = DefectMask::new(width, height);
            mask.insert(x, y);
            for radius in [3, 16] {
                assert_eq!(
                    mask.grow(radius),
                    brute_grow(&mask, radius),
                    "({x}, {y}) radius {radius}"
                );
            }
        }
    }

    #[test]
    fn grow_by_zero_keeps_the_mask_and_an_empty_mask_stays_empty() {
        let mask = random_mask(131, 47, 5, 1);
        assert_eq!(mask.grow(0), mask);
        let empty = DefectMask::new(131, 47);
        assert_eq!(empty.grow(16), empty);
    }

    fn budget() -> HostBudget {
        HostBudget::host_derived(&crate::host::snapshot())
    }

    /// Sample values that straddle "below half" at `bits` (8 or 16).
    fn boundary_values(bits: u16) -> [u16; 6] {
        if bits == 8 {
            [0, 126, 127, 128, 129, 255]
        } else {
            [0, 32766, 32767, 32768, 32769, 65535]
        }
    }

    /// `width * height * spp` interleaved samples drawn from the boundary values.
    fn boundary_samples(width: u32, height: u32, spp: usize, bits: u16) -> Vec<u16> {
        let values = boundary_values(bits);
        let mut state = 99u64 + spp as u64 + u64::from(bits);
        (0..width as usize * height as usize * spp)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                values[(state >> 33) as usize % values.len()]
            })
            .collect()
    }

    /// The pixels a user would call defects, from the samples at their own depth.
    fn painted(width: u32, samples: &[u16], spp: usize, bits: u16) -> Vec<(u32, u32)> {
        let half = 1u16 << (bits - 1);
        let colours = if spp.is_multiple_of(2) { spp - 1 } else { spp };
        samples
            .chunks_exact(spp)
            .enumerate()
            .filter(|(_, pixel)| {
                pixel[..colours].iter().all(|&v| v < half)
                    && (colours == spp || pixel[colours] >= half)
            })
            .map(|(at, _)| (at as u32 % width, at as u32 / width))
            .collect()
    }

    fn sample_bytes(samples: &[u16], bits: u16, big_endian: bool) -> Vec<u8> {
        if bits == 8 {
            samples.iter().map(|&v| v as u8).collect()
        } else {
            samples
                .iter()
                .flat_map(|&v| {
                    if big_endian {
                        v.to_be_bytes()
                    } else {
                        v.to_le_bytes()
                    }
                })
                .collect()
        }
    }

    fn write_tiff(
        dir: &Path,
        name: &str,
        size: (u32, u32),
        spp: u32,
        bits: u16,
        samples: &[u16],
    ) -> PathBuf {
        let path = dir.join(name);
        let mut writer = StripWriter::create(
            &path,
            StripSpec {
                width: size.0,
                height: size.1,
                samples_per_pixel: spp,
                bits_per_sample: bits,
                compression: StripCompression::None,
                description: None,
                metadata: MetadataTags::default(),
            },
        )
        .expect("the TIFF writer opens");
        writer
            .write_rows(&sample_bytes(samples, bits, false))
            .expect("the rows are written");
        writer.finish().expect("the TIFF is finished");
        path
    }

    fn write_png(
        dir: &Path,
        name: &str,
        size: (u32, u32),
        color: png::ColorType,
        bits: u16,
        palette: Option<&[u8]>,
        data: &[u8],
    ) -> PathBuf {
        let path = dir.join(name);
        let mut encoder = png::Encoder::new(
            std::io::BufWriter::new(std::fs::File::create(&path).expect("the PNG file")),
            size.0,
            size.1,
        );
        encoder.set_color(color);
        encoder.set_depth(if bits == 16 {
            png::BitDepth::Sixteen
        } else {
            png::BitDepth::Eight
        });
        if let Some(palette) = palette {
            encoder.set_palette(palette.to_vec());
        }
        let mut writer = encoder.write_header().expect("the PNG header");
        writer.write_image_data(data).expect("the PNG pixels");
        writer.finish().expect("the PNG is finished");
        path
    }

    /// A JPEG of dithered white with dithered black rectangles; returns the
    /// file and the pixels inside the rectangles.
    fn write_jpeg(
        dir: &Path,
        name: &str,
        size: (u32, u32),
        channels: usize,
    ) -> (PathBuf, Vec<(u32, u32)>) {
        let rectangles = [(5..15u32, 3..9u32), (20..33, 10..20)];
        let mut state = 5u64;
        let mut noise = move |range: u64| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) % range
        };
        let mut data = Vec::new();
        let mut inside = Vec::new();
        for y in 0..size.1 {
            for x in 0..size.0 {
                let black = rectangles
                    .iter()
                    .any(|(xs, ys)| xs.contains(&x) && ys.contains(&y));
                if black {
                    inside.push((x, y));
                }
                let value = if black { noise(31) } else { 225 + noise(31) } as u8;
                data.extend(std::iter::repeat_n(value, channels));
            }
        }
        let color = if channels == 1 {
            jpeg_encoder::ColorType::Luma
        } else {
            jpeg_encoder::ColorType::Rgb
        };
        let path = dir.join(name);
        let mut bytes = Vec::new();
        jpeg_encoder::Encoder::new(&mut bytes, 95)
            .encode(&data, size.0 as u16, size.1 as u16, color)
            .expect("the JPEG encodes");
        std::fs::write(&path, bytes).expect("the JPEG file");
        (path, inside)
    }

    const SIZE: (u32, u32) = (37, 23);

    #[test]
    fn decode_tiff_variants_mark_the_painted_pixels() {
        let dir = tempfile::tempdir().unwrap();
        for (spp, bits) in [(1, 8), (1, 16), (3, 8), (3, 16), (4, 8)] {
            let samples = boundary_samples(SIZE.0, SIZE.1, spp, bits);
            let want = painted(SIZE.0, &samples, spp, bits);
            assert!(
                want.len() > 20 && want.len() < 800,
                "{spp} samples, {bits} bit: {}",
                want.len()
            );
            let name = format!("{spp}x{bits}.tif");
            let path = write_tiff(dir.path(), &name, SIZE, spp as u32, bits, &samples);
            let mask = load(&path, SIZE, 0, budget()).unwrap();
            assert_eq!(mask.positions(), want, "TIFF {spp} samples, {bits} bit");
            for band_rows in [1, 5, 23] {
                let banded = decode_frames(&path, SIZE, Some(band_rows), budget()).unwrap();
                assert_eq!(
                    banded.positions(),
                    want,
                    "TIFF {spp} samples, {bits} bit, {band_rows} rows a band"
                );
            }
        }
    }

    #[test]
    fn decode_png_variants_mark_the_painted_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let variants = [
            (png::ColorType::Grayscale, 1, 8),
            (png::ColorType::Rgb, 3, 16),
            (png::ColorType::Rgba, 4, 8),
            (png::ColorType::Rgba, 4, 16),
            (png::ColorType::GrayscaleAlpha, 2, 16),
        ];
        for (color, spp, bits) in variants {
            let samples = boundary_samples(SIZE.0, SIZE.1, spp, bits);
            let want = painted(SIZE.0, &samples, spp, bits);
            assert!(want.len() > 20, "{color:?} {bits} bit: {}", want.len());
            let data = sample_bytes(&samples, bits, true);
            let path = write_png(dir.path(), "mask.png", SIZE, color, bits, None, &data);
            let mask = load(&path, SIZE, 0, budget()).unwrap();
            assert_eq!(mask.positions(), want, "PNG {color:?} {bits} bit");
        }

        let palette = [
            0, 0, 0, 127, 127, 127, 128, 0, 0, 127, 127, 128, 255, 255, 255,
        ];
        let indices: Vec<u8> = (0..SIZE.0 * SIZE.1).map(|at| (at * 7 % 5) as u8).collect();
        let dark = [true, true, false, false, false];
        let want: Vec<(u32, u32)> = (0..SIZE.0 * SIZE.1)
            .filter(|&at| dark[indices[at as usize] as usize])
            .map(|at| (at % SIZE.0, at / SIZE.0))
            .collect();
        let path = write_png(
            dir.path(),
            "palette.png",
            SIZE,
            png::ColorType::Indexed,
            8,
            Some(&palette),
            &indices,
        );
        assert_eq!(
            load(&path, SIZE, 0, budget()).unwrap().positions(),
            want,
            "PNG palette"
        );
    }

    #[test]
    fn decode_png_transparent_black_is_not_a_defect() {
        let dir = tempfile::tempdir().unwrap();
        let rgba = [0, 0, 0, 0, 0, 0, 0, 255, 0, 0, 0, 127, 0, 0, 0, 128];
        let path = write_png(
            dir.path(),
            "alpha.png",
            (4, 1),
            png::ColorType::Rgba,
            8,
            None,
            &rgba,
        );
        assert_eq!(
            load(&path, (4, 1), 0, budget()).unwrap().positions(),
            [(1, 0), (3, 0)]
        );
    }

    #[test]
    fn decode_jpeg_noise_around_black_paint_yields_the_painted_pixels() {
        let dir = tempfile::tempdir().unwrap();
        for channels in [1, 3] {
            let name = format!("mask{channels}.jpg");
            let (path, want) = write_jpeg(dir.path(), &name, SIZE, channels);
            let mask = load(&path, SIZE, 0, budget()).unwrap();
            assert_eq!(mask.positions(), want, "JPEG with {channels} channels");
        }
    }

    #[test]
    fn decode_applies_the_grow_radius() {
        let dir = tempfile::tempdir().unwrap();
        let mut samples = vec![255u16; 9 * 9];
        samples[4 * 9 + 4] = 0;
        let path = write_tiff(dir.path(), "dot.tif", (9, 9), 1, 8, &samples);
        assert_eq!(load(&path, (9, 9), 0, budget()).unwrap().count(), 1);
        assert_eq!(load(&path, (9, 9), 2, budget()).unwrap().count(), 13);
    }

    fn rejected(path: &Path, frame: (u32, u32)) -> DefectMaskError {
        load(path, frame, 0, budget()).expect_err("the mask is refused")
    }

    fn mask_files(dir: &Path) -> Vec<PathBuf> {
        let samples = boundary_samples(SIZE.0, SIZE.1, 1, 8);
        let tiff = write_tiff(dir, "size.tif", SIZE, 1, 8, &samples);
        let png = write_png(
            dir,
            "size.png",
            SIZE,
            png::ColorType::Grayscale,
            8,
            None,
            &sample_bytes(&samples, 8, true),
        );
        let (jpeg, _) = write_jpeg(dir, "size.jpg", SIZE, 1);
        vec![tiff, png, jpeg]
    }

    #[test]
    fn rejects_a_mask_of_another_size_naming_both_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let files = mask_files(dir.path());
        for path in &files {
            for frames in [(23, 37), (38, 23), (37, 22), (37, 24), (512, 512)] {
                match rejected(path, frames) {
                    DefectMaskError::SizeMismatch {
                        mask, frames: got, ..
                    } => {
                        assert_eq!((mask, got), (SIZE, frames), "{}", path.display());
                    }
                    other => panic!("{}: expected a size mismatch, got {other}", path.display()),
                }
            }
        }
        assert_eq!(
            rejected(&files[0], (512, 512)).to_string(),
            format!(
                "{}: the defect mask is 37×23 pixels but the frames are 512×512; the mask must be the frames' size",
                files[0].display()
            )
        );
    }

    #[test]
    fn rejects_a_png_by_its_header_size_before_decoding_its_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let path = mask_files(dir.path()).swap_remove(1);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(8 + 25 + 8 + 2);
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(
            rejected(&path, (37, 22)),
            DefectMaskError::SizeMismatch { .. }
        ));
    }

    #[test]
    fn rejects_unreadable_and_unsupported_files_naming_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut unreadable = Vec::new();
        for name in ["gone.tif", "gone.png", "gone.jpg"] {
            unreadable.push(dir.path().join(name));
        }
        for name in ["junk.tif", "junk.png", "junk.jpg"] {
            let path = dir.path().join(name);
            std::fs::write(&path, b"this is not an image file at all").unwrap();
            unreadable.push(path);
        }
        let samples = boundary_samples(SIZE.0, SIZE.1, 1, 8);
        let one_bit = write_tiff(dir.path(), "one-bit.tif", SIZE, 1, 8, &samples);
        let mut bytes = std::fs::read(&one_bit).unwrap();
        let ifd = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let count = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
        let entry = (0..count)
            .map(|at| ifd + 2 + at * 12)
            .find(|&at| bytes[at..at + 2] == 258u16.to_le_bytes())
            .expect("a BitsPerSample entry");
        bytes[entry + 8..entry + 10].copy_from_slice(&1u16.to_le_bytes());
        std::fs::write(&one_bit, bytes).unwrap();
        unreadable.push(one_bit);
        for path in unreadable {
            match rejected(&path, SIZE) {
                error @ DefectMaskError::Unreadable { .. } => {
                    let shown = path.display().to_string();
                    assert!(error.to_string().contains(&shown), "{error}");
                }
                other => panic!(
                    "{}: expected an unreadable file, got {other}",
                    path.display()
                ),
            }
        }
        for name in ["mask.bmp", "mask.cr2", "mask"] {
            let path = dir.path().join(name);
            std::fs::write(&path, b"x").unwrap();
            match rejected(&path, SIZE) {
                error @ DefectMaskError::UnsupportedFormat { .. } => {
                    let shown = path.display().to_string();
                    assert!(error.to_string().contains(&shown), "{error}");
                }
                other => panic!(
                    "{}: expected an unsupported format, got {other}",
                    path.display()
                ),
            }
        }
    }

    #[test]
    fn fixture_mask_loads_against_the_hot_pixel_frames() {
        let set =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/mantis-512-hot-pixels");
        let paths = crate::io::frame_paths(&set.join("frames")).expect("the fixture frames list");
        let frames = crate::io::open_frame_set(&paths, budget()).expect("the fixture opens");
        let frame = frames.reader.dimensions();
        let mask = set.join("defect-mask.tif");
        let painted = load(&mask, frame, 0, budget()).unwrap();
        assert_eq!(painted.count(), 86);
        let grown = load(&mask, frame, 2, budget()).unwrap();
        assert_eq!(grown.count(), 200);
        assert_eq!(grown, brute_grow(&painted, 2));
    }

    #[test]
    #[ignore = "measurement: prints the wall time of grow(16) on a full 44.8 MP mask"]
    fn grow_of_a_black_full_frame_mask_at_radius_16() {
        let (width, height) = (8192, 5464);
        let mut mask = DefectMask::new(width, height);
        for y in 0..height {
            for x in 0..width {
                mask.insert(x, y);
            }
        }
        let start = std::time::Instant::now();
        let grown = mask.grow(16);
        eprintln!("grow(16) on {width}x{height}: {:?}", start.elapsed());
        assert_eq!(grown.count(), width as usize * height as usize);
    }
}
