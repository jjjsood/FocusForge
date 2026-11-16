// SPDX-License-Identifier: GPL-3.0-or-later

//! The defect repair reader against a brute-force window median, on
//! synthetic frames served from memory.

use focusforge_core::algorithm::{AlgorithmError, TileReader};
use focusforge_core::defect_mask::DefectMask;
use focusforge_core::defect_repair::{MAX_REPAIR_REACH, RepairReader};
use focusforge_core::equalize::{GainReader, UNIT_GAIN};
use focusforge_core::tile::{Rect, SampleFormat};

const WIDTH: u32 = 37;
const HEIGHT: u32 = 23;
const SPP: usize = 3;

/// Frames of pseudo-random samples held in memory, served as `u16` or as
/// native `f32`.
struct Frames {
    format: SampleFormat,
    width: u32,
    height: u32,
    frames: Vec<Vec<f32>>,
}

impl Frames {
    fn new(format: SampleFormat, frame_count: usize) -> Self {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let frames = (0..frame_count)
            .map(|_| {
                (0..WIDTH as usize * HEIGHT as usize * SPP)
                    .map(|_| {
                        state = state
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        let sample = (state >> 40) as u16;
                        match format {
                            SampleFormat::U16 => f32::from(sample),
                            SampleFormat::F32 => f32::from(sample) * 1.37 + 0.25,
                        }
                    })
                    .collect()
            })
            .collect();
        Frames {
            format,
            width: WIDTH,
            height: HEIGHT,
            frames,
        }
    }

    fn rows(
        &self,
        frame: usize,
        rect: Rect,
    ) -> Result<impl Iterator<Item = &[f32]>, AlgorithmError> {
        let fits = rect.width > 0
            && rect.height > 0
            && rect.x + rect.width <= self.width
            && rect.y + rect.height <= self.height;
        if !fits {
            return Err(AlgorithmError::Failed {
                algorithm: "frames".to_string(),
                detail: format!("{rect:?} is outside the frame"),
            });
        }
        let source = &self.frames[frame];
        let width = self.width as usize;
        Ok((rect.y..rect.y + rect.height).map(move |y| {
            let start = (y as usize * width + rect.x as usize) * SPP;
            &source[start..start + rect.width as usize * SPP]
        }))
    }
}

impl TileReader for Frames {
    fn sample_format(&self) -> SampleFormat {
        self.format
    }

    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        assert_eq!(self.format, SampleFormat::U16, "a native f32 reader");
        let row = rect.width as usize * SPP;
        for (dst, src) in out.chunks_exact_mut(row).zip(self.rows(frame, rect)?) {
            for (dst, &src) in dst.iter_mut().zip(src) {
                *dst = src as u16;
            }
        }
        Ok(())
    }

    fn read_tile_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        let row = rect.width as usize * SPP;
        for (dst, src) in out.chunks_exact_mut(row).zip(self.rows(frame, rect)?) {
            dst.copy_from_slice(src);
        }
        Ok(())
    }

    fn read_tile_gained_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        gain: u32,
        out: &mut [f32],
    ) -> Result<(), AlgorithmError> {
        if self.format == SampleFormat::U16 {
            let mut narrow = vec![0u16; out.len()];
            self.read_tile_gained_into(frame, rect, gain, &mut narrow)?;
            out.iter_mut()
                .zip(narrow)
                .for_each(|(wide, narrow)| *wide = f32::from(narrow));
            return Ok(());
        }
        self.read_tile_f32_into(frame, rect, out)?;
        let scale = gain as f32 / UNIT_GAIN as f32;
        out.iter_mut().for_each(|sample| *sample *= scale);
        Ok(())
    }
}

fn full() -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: WIDTH,
        height: HEIGHT,
    }
}

fn read_u16(reader: &dyn TileReader, frame: usize, rect: Rect) -> Vec<u16> {
    let mut out = vec![0; rect.width as usize * rect.height as usize * SPP];
    reader.read_tile_into(frame, rect, &mut out).unwrap();
    out
}

fn read_f32(reader: &dyn TileReader, frame: usize, rect: Rect) -> Vec<f32> {
    let mut out = vec![0.0; rect.width as usize * rect.height as usize * SPP];
    reader.read_tile_f32_into(frame, rect, &mut out).unwrap();
    out
}

fn random_mask(percent: u64, seed: u64) -> DefectMask {
    let mut state = seed;
    let mut mask = DefectMask::new(WIDTH, HEIGHT);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
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

/// What a read of `source` (full frame, `SPP` interleaved samples) should
/// give: every masked pixel's channels are the lower median of the unmasked
/// pixels in the smallest square window around it that holds any.
fn expected<T: Copy>(
    source: &[T],
    mask: &DefectMask,
    order: fn(&T, &T) -> std::cmp::Ordering,
) -> Vec<T> {
    let mut out = source.to_vec();
    let (w, h) = (WIDTH as i64, HEIGHT as i64);
    for (x, y) in mask.positions() {
        for half in 1..=w.max(h) {
            let healthy: Vec<usize> = (-half..=half)
                .flat_map(|dy| (-half..=half).map(move |dx| (x as i64 + dx, y as i64 + dy)))
                .filter(|&(nx, ny)| (0..w).contains(&nx) && (0..h).contains(&ny))
                .filter(|&(nx, ny)| !mask.contains(nx as u32, ny as u32))
                .map(|(nx, ny)| (ny * w + nx) as usize)
                .collect();
            if healthy.is_empty() {
                continue;
            }
            for channel in 0..SPP {
                let mut values: Vec<T> = healthy
                    .iter()
                    .map(|&at| source[at * SPP + channel])
                    .collect();
                values.sort_by(order);
                out[(y as usize * WIDTH as usize + x as usize) * SPP + channel] =
                    values[(values.len() - 1) / 2];
            }
            break;
        }
    }
    out
}

const GAINS: [u32; 3] = [
    UNIT_GAIN,
    UNIT_GAIN + UNIT_GAIN * 3 / 10,
    UNIT_GAIN * 7 / 10,
];

#[test]
fn repair_replaces_only_masked_pixels() {
    for percent in [1, 30] {
        let mask = random_mask(percent, 5 + percent);
        assert!(mask.count() > 0, "{percent}%: an empty mask proves nothing");

        let narrow = Frames::new(SampleFormat::U16, 2);
        let repair = RepairReader::new(&narrow, &mask, SPP as u32).unwrap();
        let wide = Frames::new(SampleFormat::F32, 2);
        let repair_wide = RepairReader::new(&wide, &mask, SPP as u32).unwrap();
        assert_eq!(repair_wide.sample_format(), SampleFormat::F32);

        for frame in 0..2 {
            let source = read_u16(&narrow, frame, full());
            let want = expected(&source, &mask, u16::cmp);
            assert_ne!(want, source, "{percent}%: the mask must change samples");
            assert_eq!(read_u16(&repair, frame, full()), want, "{percent}% u16");

            let source = read_f32(&narrow, frame, full());
            let want = expected(&source, &mask, f32::total_cmp);
            assert_eq!(
                read_f32(&repair, frame, full()),
                want,
                "{percent}% u16 as f32"
            );

            let source = read_f32(&wide, frame, full());
            let want = expected(&source, &mask, f32::total_cmp);
            assert_ne!(want, source);
            assert_eq!(
                read_f32(&repair_wide, frame, full()),
                want,
                "{percent}% f32"
            );
        }

        // gained(repair) == repair(gained)
        let gains = GAINS;
        for (frames, plain, label) in [(&narrow, &repair, "u16"), (&wide, &repair_wide, "f32")] {
            let gained_after = GainReader::new(plain, &gains);
            let gained_inner = GainReader::new(frames, &gains);
            let gained_first = RepairReader::new(&gained_inner, &mask, SPP as u32).unwrap();
            for frame in 0..2 {
                assert_eq!(
                    read_f32(&gained_after, frame, full()),
                    read_f32(&gained_first, frame, full()),
                    "{percent}% {label} frame {frame} f32"
                );
                if label == "u16" {
                    assert_eq!(
                        read_u16(&gained_after, frame, full()),
                        read_u16(&gained_first, frame, full()),
                        "{percent}% frame {frame} u16"
                    );
                }
            }
        }
    }
}

/// Reads `frame` in `tile`-sized tiles stitched into one frame buffer.
fn read_in_tiles(reader: &dyn TileReader, frame: usize, tile: u32) -> Vec<u16> {
    let mut out = vec![0u16; WIDTH as usize * HEIGHT as usize * SPP];
    for y in (0..HEIGHT).step_by(tile as usize) {
        for x in (0..WIDTH).step_by(tile as usize) {
            let rect = Rect {
                x,
                y,
                width: tile.min(WIDTH - x),
                height: tile.min(HEIGHT - y),
            };
            let samples = read_u16(reader, frame, rect);
            for (row, line) in samples.chunks_exact(rect.width as usize * SPP).enumerate() {
                let start = ((y as usize + row) * WIDTH as usize + x as usize) * SPP;
                out[start..start + line.len()].copy_from_slice(line);
            }
        }
    }
    out
}

#[test]
fn repair_is_tile_size_independent() {
    let mut mask = DefectMask::new(WIDTH, HEIGHT);
    for (x, y) in [
        (0, 0),
        (WIDTH - 1, 0),
        (0, HEIGHT - 1),
        (WIDTH - 1, HEIGHT - 1),
        (6, 6),
        (7, 6),
        (15, 9),
        (16, 9),
        (3, 15),
        (3, 16),
        (31, 20),
        (0, 11),
    ] {
        mask.insert(x, y);
    }
    let mut spot = DefectMask::new(WIDTH, HEIGHT);
    spot.insert(22, 11);
    let mask = {
        let mut union = mask;
        union.union_with(&spot.grow(4));
        union
    };

    let frames = Frames::new(SampleFormat::U16, 1);
    let repair = RepairReader::new(&frames, &mask, SPP as u32).unwrap();
    let reference = read_u16(&repair, 0, full());
    assert_eq!(
        reference,
        expected(&read_u16(&frames, 0, full()), &mask, u16::cmp)
    );
    for tile in [1, 3, 7, 16] {
        assert_eq!(read_in_tiles(&repair, 0, tile), reference, "tile {tile}");
    }
}

#[test]
fn repair_refuses_a_defect_beyond_the_reach() {
    let rejected = |width: u32, healthy: Option<u32>| {
        let mut mask = DefectMask::new(width, 1);
        for x in 0..width {
            if Some(x) != healthy {
                mask.insert(x, 0);
            }
        }
        let frames = Frames {
            format: SampleFormat::U16,
            width,
            height: 1,
            frames: vec![vec![0.0; width as usize * SPP]],
        };
        RepairReader::new(&frames, &mask, SPP as u32).err()
    };
    let reach = MAX_REPAIR_REACH;
    // Every defect at most `reach` from the one healthy pixel: accepted.
    assert!(rejected(2 * reach + 1, Some(reach)).is_none());
    // The last defect is one pixel too far; its position is named.
    let message = rejected(2 * reach + 2, Some(reach))
        .expect("a defect past the reach is refused")
        .to_string();
    assert!(
        message.contains(&format!("({}, 0)", 2 * reach + 1)),
        "{message}"
    );
    // An all-black mask has no healthy pixel at all.
    let message = rejected(10, None)
        .expect("an all-defect mask is refused")
        .to_string();
    assert!(message.contains("(0, 0)"), "{message}");
}
