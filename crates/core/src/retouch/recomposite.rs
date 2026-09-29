// SPDX-License-Identifier: GPL-3.0-or-later

//! Recompositing an image from a Z-Map and its source frames: a per-pixel
//! gather, never a blend -- each output pixel is the sample of the frame
//! its index names, and [`NO_SOURCE`] gathers an all-zero sample. The
//! region dispatch is the only entry point: a whole-image recomposite is
//! just a region equal to the map's extent.

use rayon::prelude::*;

use crate::algorithm::TileReader;
use crate::tile::Rect;
use crate::zmap::NO_SOURCE;

use super::RetouchError;

/// Where a recomposite reads its pixels from: a tile-wise reader plus the
/// facts needed to interpret it. `origin` is the Z-Map's `(0, 0)` in the
/// reader's own frame coordinates: a region at `(x, y)` of the map is read
/// from `(x + origin.0, y + origin.1)` of every frame.
pub struct Sources<'a> {
    pub reader: &'a dyn TileReader,
    pub frame_count: u32,
    pub samples_per_pixel: u32,
    pub origin: (u32, u32),
}

/// The sorted, distinct, non-[`NO_SOURCE`] frame indices `indices` names.
/// Any other index `>= frame_count` is [`RetouchError::SourceOutOfRange`].
pub fn named_frames(indices: &[u16], frame_count: u32) -> Result<Vec<u16>, RetouchError> {
    let mut seen = std::collections::BTreeSet::new();
    for &index in indices {
        if index == NO_SOURCE {
            continue;
        }
        if u32::from(index) >= frame_count {
            return Err(RetouchError::SourceOutOfRange {
                source_id: index,
                frame_count,
            });
        }
        seen.insert(index);
    }
    Ok(seen.into_iter().collect())
}

/// Gathers `region`'s composite into `out`: each pixel takes the sample of
/// the frame `indices` names, [`NO_SOURCE`] yielding zero, reading one
/// region-sized buffer per named frame at a time. `indices.len()` must
/// equal `region`'s area, `out.len()` that area times
/// `sources.samples_per_pixel`, else [`RetouchError::TileBufferSize`].
pub fn recomposite(
    indices: &[u16],
    region: Rect,
    sources: &Sources,
    out: &mut [u16],
) -> Result<(), RetouchError> {
    let area = region.width as usize * region.height as usize;
    if indices.len() != area {
        return Err(RetouchError::TileBufferSize {
            expected: area,
            actual: indices.len(),
        });
    }
    let spp = sources.samples_per_pixel as usize;
    let expected_out = area * spp;
    if out.len() != expected_out {
        return Err(RetouchError::TileBufferSize {
            expected: expected_out,
            actual: out.len(),
        });
    }

    out.fill(0);
    let frames = named_frames(indices, sources.frame_count)?;
    if frames.is_empty() {
        return Ok(());
    }

    let frame_rect = Rect {
        x: region.x + sources.origin.0,
        y: region.y + sources.origin.1,
        width: region.width,
        height: region.height,
    };
    let mut frame_buf = vec![0u16; area * spp];
    for &frame in &frames {
        sources
            .reader
            .read_tile_into(frame as usize, frame_rect, &mut frame_buf)?;
        out.par_chunks_mut(spp)
            .zip(indices.par_iter())
            .zip(frame_buf.par_chunks(spp))
            .for_each(|((out_px, &index), src_px)| {
                if index == frame {
                    out_px.copy_from_slice(src_px);
                }
            });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithm::AlgorithmError;
    use crate::io::TiffStack;
    use crate::zmap::ZMapReader;
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("fixtures")
    }

    fn golden_dir() -> PathBuf {
        fixtures_dir().join("mantis-512").join("golden")
    }

    fn frames_dir() -> PathBuf {
        fixtures_dir().join("mantis-512").join("frames")
    }

    /// A per-pixel reference gather: read every fixture frame whole, then
    /// index into it per pixel. Never the code under test's own path.
    fn reference_gather(indices: &[u16], region: Rect, stack: &TiffStack, spp: u32) -> Vec<u16> {
        let (width, height) = stack.dimensions();
        let whole = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let frames: Vec<Vec<u16>> = (0..stack.len())
            .map(|f| stack.read_tile(f, whole).unwrap().samples)
            .collect();
        let mut out = vec![0u16; indices.len() * spp as usize];
        for (i, &index) in indices.iter().enumerate() {
            if index == NO_SOURCE {
                continue;
            }
            let x = region.x + (i as u32) % region.width;
            let y = region.y + (i as u32) / region.width;
            let src_i = (y * width + x) as usize;
            let frame = &frames[index as usize];
            out[i * spp as usize..(i + 1) * spp as usize]
                .copy_from_slice(&frame[src_i * spp as usize..(src_i + 1) * spp as usize]);
        }
        out
    }

    /// Recomposites `region` of a map whose `(0, 0)` sits at `origin` in the
    /// frames, and checks it against the reference gather over `window`: the
    /// same pixels named directly in frame coordinates.
    fn check_matches_reference(window: Rect, region: Rect, origin: (u32, u32)) {
        let stack = TiffStack::open(frames_dir()).expect("open fixture frames");
        let spp = stack.header(0).samples_per_pixel;
        let zmap = ZMapReader::open(&golden_dir().join("laplacian-zmap.tif")).expect("open zmap");
        let mut indices = vec![0u16; zmap.info().tile_len(window)];
        zmap.read_tile_into(window, &mut indices)
            .expect("read zmap window");

        let sources = Sources {
            reader: &stack,
            frame_count: stack.len() as u32,
            samples_per_pixel: spp,
            origin,
        };
        let mut actual = vec![0u16; indices.len() * spp as usize];
        recomposite(&indices, region, &sources, &mut actual).expect("recomposite");

        let expected = reference_gather(&indices, window, &stack, spp);
        assert_eq!(actual, expected, "region {region:?} at origin {origin:?}");
    }

    #[test]
    fn matches_reference_gather() {
        let (width, height) = TiffStack::open(frames_dir())
            .expect("open fixture frames")
            .dimensions();
        let whole = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        check_matches_reference(whole, whole, (0, 0));
        let window = Rect {
            x: 50,
            y: 70,
            width: 400,
            height: 300,
        };
        check_matches_reference(window, window, (0, 0));
        let crop = Rect {
            x: 0,
            y: 0,
            width: 400,
            height: 300,
        };
        check_matches_reference(window, crop, (50, 70));
    }

    /// An in-memory `TileReader` whose samples name their own frame and
    /// source coordinate: `frame*10_000 + y*100 + x`, one sample per pixel.
    struct TranslatingFrames;

    impl TileReader for TranslatingFrames {
        fn read_tile_into(
            &self,
            frame: usize,
            rect: Rect,
            out: &mut [u16],
        ) -> Result<(), AlgorithmError> {
            for row in 0..rect.height {
                for col in 0..rect.width {
                    let x = rect.x + col;
                    let y = rect.y + row;
                    out[(row * rect.width + col) as usize] =
                        (frame as u32 * 10_000 + y * 100 + x) as u16;
                }
            }
            Ok(())
        }
    }

    #[test]
    fn gathers_through_a_warped_reader_at_a_non_zero_origin() {
        use crate::align::{Transform, WarpingReader};

        let frame_dims = (64u32, 64u32);
        // Integer shifts of both signs, in x and y.
        let shifts: [(i64, i64); 3] = [(0, 0), (4, -2), (-3, 5)];
        let transforms: Vec<Transform> = shifts
            .iter()
            .map(|&(dx, dy)| Transform::translation(dx as f64, dy as f64))
            .collect();
        let inner = TranslatingFrames;
        let warping = WarpingReader::new(&inner, frame_dims, 1, transforms);

        let origin = (5u32, 3u32);
        let region = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 15,
        };
        let indices = checker_indices(region.width, region.height, 3);
        let sources = Sources {
            reader: &warping,
            frame_count: 3,
            samples_per_pixel: 1,
            origin,
        };
        let mut actual = vec![0u16; indices.len()];
        recomposite(&indices, region, &sources, &mut actual).expect("recomposite");

        for (i, &index) in indices.iter().enumerate() {
            let x = region.x + (i as u32) % region.width;
            let y = region.y + (i as u32) / region.width;
            let (dx, dy) = shifts[index as usize];
            let sx = (x + origin.0) as i64 + dx;
            let sy = (y + origin.1) as i64 + dy;
            let expected = (u32::from(index) * 10_000 + sy as u32 * 100 + sx as u32) as u16;
            assert_eq!(
                actual[i], expected,
                "pixel {i} (frame {index}) must equal a direct shifted gather"
            );
        }
    }

    /// An in-memory `TileReader` recording which frames it was asked for,
    /// producing samples `frame*1000 + x` so each gathered sample names its
    /// source frame and column, one sample per pixel (`spp == 1`).
    struct RecordingReader {
        reads: Mutex<Vec<usize>>,
    }

    impl RecordingReader {
        fn new() -> Self {
            RecordingReader {
                reads: Mutex::new(Vec::new()),
            }
        }

        fn reads(&self) -> Vec<usize> {
            self.reads.lock().unwrap().clone()
        }
    }

    impl TileReader for RecordingReader {
        fn read_tile_into(
            &self,
            frame: usize,
            rect: Rect,
            out: &mut [u16],
        ) -> Result<(), AlgorithmError> {
            self.reads.lock().unwrap().push(frame);
            for row in 0..rect.height {
                for col in 0..rect.width {
                    let x = rect.x + col;
                    out[(row * rect.width + col) as usize] = (frame as u32 * 1000 + x) as u16;
                }
            }
            Ok(())
        }
    }

    fn sources(reader: &dyn TileReader, frame_count: u32) -> Sources<'_> {
        Sources {
            reader,
            frame_count,
            samples_per_pixel: 1,
            origin: (0, 0),
        }
    }

    #[test]
    fn no_source_gathers_zero_and_reads_nothing_for_it() {
        let region = Rect {
            x: 0,
            y: 0,
            width: 13,
            height: 9,
        };
        // Mix `NO_SOURCE` with two of five frames.
        let indices: Vec<u16> = (0..(13 * 9))
            .map(|i| match i % 3 {
                0 => NO_SOURCE,
                1 => 2,
                _ => 4,
            })
            .collect();
        let reader = RecordingReader::new();
        let src = sources(&reader, 5);
        let mut out = vec![0u16; indices.len()];
        recomposite(&indices, region, &src, &mut out).expect("recomposite");

        for (i, &index) in indices.iter().enumerate() {
            if index == NO_SOURCE {
                assert_eq!(out[i], 0, "NO_SOURCE pixel {i} must gather zero");
            } else {
                let x = region.x + (i as u32) % region.width;
                assert_eq!(out[i], (u32::from(index) * 1000 + x) as u16, "pixel {i}");
            }
        }
        let mut reads = reader.reads();
        reads.sort_unstable();
        reads.dedup();
        assert_eq!(reads, vec![2, 4], "must read exactly the named frames");
    }

    #[test]
    fn out_of_range_index_is_refused() {
        let region = Rect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let reader = RecordingReader::new();
        let src = sources(&reader, 5);
        let indices = vec![0u16, 5, 0, 0];
        let mut out = vec![0u16; 4];
        match recomposite(&indices, region, &src, &mut out) {
            Err(RetouchError::SourceOutOfRange {
                source_id: 5,
                frame_count: 5,
            }) => {}
            other => panic!("expected SourceOutOfRange, got {other:?}"),
        }

        let bad_out = vec![0u16; 3];
        let indices_ok = vec![0u16; 4];
        let mut bad_out_buf = bad_out.clone();
        match recomposite(&indices_ok, region, &src, &mut bad_out_buf) {
            Err(RetouchError::TileBufferSize {
                expected: 4,
                actual: 3,
            }) => {}
            other => panic!("expected TileBufferSize, got {other:?}"),
        }
    }

    fn checker_indices(width: u32, height: u32, frame_count: u32) -> Vec<u16> {
        (0..height)
            .flat_map(|y| (0..width).map(move |x| ((x + y) % frame_count) as u16))
            .collect()
    }

    fn full_recomposite(indices: &[u16], width: u32, height: u32, src: &Sources) -> Vec<u16> {
        let region = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let mut out = vec![0u16; indices.len() * src.samples_per_pixel as usize];
        recomposite(indices, region, src, &mut out).expect("full recomposite");
        out
    }

    fn splice_region(canvas: &mut [u16], canvas_width: u32, region: Rect, region_out: &[u16]) {
        splice_region_spp(canvas, canvas_width, region, region_out, 1);
    }

    #[test]
    fn region_dispatch_composes() {
        let (width, height) = (64u32, 101u32);
        let indices = checker_indices(width, height, 5);
        let reader = RecordingReader::new();
        let src = sources(&reader, 5);
        let expected = full_recomposite(&indices, width, height, &src);

        // 37-row bands.
        let mut by_bands = vec![0u16; indices.len()];
        let mut y = 0u32;
        while y < height {
            let band_height = 37u32.min(height - y);
            let region = Rect {
                x: 0,
                y,
                width,
                height: band_height,
            };
            let region_indices =
                &indices[(y * width) as usize..((y + band_height) * width) as usize];
            let mut region_out = vec![0u16; region_indices.len()];
            recomposite(region_indices, region, &src, &mut region_out).expect("band recomposite");
            splice_region(&mut by_bands, width, region, &region_out);
            y += band_height;
        }
        assert_eq!(by_bands, expected, "band-composed must equal whole");

        // Four unequal quadrants.
        let (mid_x, mid_y) = (23u32, 41u32);
        let quadrants = [
            Rect {
                x: 0,
                y: 0,
                width: mid_x,
                height: mid_y,
            },
            Rect {
                x: mid_x,
                y: 0,
                width: width - mid_x,
                height: mid_y,
            },
            Rect {
                x: 0,
                y: mid_y,
                width: mid_x,
                height: height - mid_y,
            },
            Rect {
                x: mid_x,
                y: mid_y,
                width: width - mid_x,
                height: height - mid_y,
            },
        ];
        let mut by_quadrants = vec![0u16; indices.len()];
        for region in quadrants {
            let mut region_indices = vec![0u16; (region.width * region.height) as usize];
            for row in 0..region.height {
                let start = ((region.y + row) * width + region.x) as usize;
                let dest = (row * region.width) as usize;
                region_indices[dest..dest + region.width as usize]
                    .copy_from_slice(&indices[start..start + region.width as usize]);
            }
            let mut region_out = vec![0u16; region_indices.len()];
            recomposite(&region_indices, region, &src, &mut region_out)
                .expect("quadrant recomposite");
            splice_region(&mut by_quadrants, width, region, &region_out);
        }
        assert_eq!(by_quadrants, expected, "quadrant-composed must equal whole");

        // A single region dispatch leaves the rest of a sentinel-filled
        // canvas untouched.
        const SENTINEL: u16 = 0xDEAD;
        let region = quadrants[0];
        let region_indices: Vec<u16> = (0..region.height)
            .flat_map(|row| {
                let start = ((region.y + row) * width + region.x) as usize;
                indices[start..start + region.width as usize].to_vec()
            })
            .collect();
        let mut region_out = vec![0u16; region_indices.len()];
        recomposite(&region_indices, region, &src, &mut region_out).expect("region recomposite");
        let mut canvas = vec![SENTINEL; indices.len()];
        splice_region(&mut canvas, width, region, &region_out);
        for y in 0..height {
            for x in 0..width {
                let i = (y * width + x) as usize;
                let inside = x < region.width && y < region.height;
                if inside {
                    assert_eq!(canvas[i], expected[i], "inside region ({x},{y})");
                } else {
                    assert_eq!(
                        canvas[i], SENTINEL,
                        "outside region ({x},{y}) must stay sentinel"
                    );
                }
            }
        }
    }

    #[test]
    fn stroke_recomposites_its_dirty_region_only() {
        use crate::project::Op;
        use crate::retouch::EditSession;

        let stack = TiffStack::open(frames_dir()).expect("open fixture frames");
        let spp = stack.header(0).samples_per_pixel;
        let zmap = ZMapReader::open(&golden_dir().join("laplacian-zmap.tif")).expect("open zmap");
        let info = zmap.info();
        let whole = Rect {
            x: 0,
            y: 0,
            width: info.width(),
            height: info.height(),
        };
        let mut pre_indices = vec![0u16; info.tile_len(whole)];
        zmap.read_tile_into(whole, &mut pre_indices)
            .expect("read whole zmap");

        let src = Sources {
            reader: &stack,
            frame_count: stack.len() as u32,
            samples_per_pixel: spp,
            origin: (0, 0),
        };
        let pre_composite = full_recomposite(&pre_indices, whole.width, whole.height, &src);

        let ops_text = std::fs::read_to_string(fixtures_dir().join("retouch/stroke-list.json"))
            .expect("read stroke list");
        let ops: Vec<Op> = serde_json::from_str(&ops_text).expect("parse stroke list");
        let stroke = ops.into_iter().next().expect("at least one stroke");

        let mut session = EditSession::new(info, whole, pre_indices.clone()).expect("session");
        let region = session
            .append(stroke)
            .expect("apply stroke")
            .expect("stroke has a non-empty footprint");

        let mut region_indices = vec![0u16; (region.width * region.height) as usize];
        for row in 0..region.height {
            let start = ((region.y + row) * whole.width + region.x) as usize;
            let dest = (row * region.width) as usize;
            region_indices[dest..dest + region.width as usize]
                .copy_from_slice(&session.indices()[start..start + region.width as usize]);
        }
        let mut region_composite = vec![0u16; region_indices.len() * spp as usize];
        recomposite(&region_indices, region, &src, &mut region_composite)
            .expect("region recomposite");

        let mut actual = pre_composite.clone();
        splice_region_spp(&mut actual, whole.width, region, &region_composite, spp);

        let expected = full_recomposite(session.indices(), whole.width, whole.height, &src);
        assert_eq!(actual, expected, "splice must equal full recomposite");

        for row in 0..whole.height {
            for col in 0..whole.width {
                let inside = col >= region.x
                    && col < region.x + region.width
                    && row >= region.y
                    && row < region.y + region.height;
                if !inside {
                    let i = (row * whole.width + col) as usize;
                    for c in 0..spp as usize {
                        assert_eq!(
                            actual[i * spp as usize + c],
                            pre_composite[i * spp as usize + c],
                            "outside the dirty region must equal the pre-stroke composite"
                        );
                    }
                }
            }
        }
    }

    fn splice_region_spp(
        canvas: &mut [u16],
        canvas_width: u32,
        region: Rect,
        region_out: &[u16],
        spp: u32,
    ) {
        let spp = spp as usize;
        for row in 0..region.height {
            let canvas_start = (((region.y + row) * canvas_width + region.x) as usize) * spp;
            let region_start = (row * region.width) as usize * spp;
            let len = region.width as usize * spp;
            canvas[canvas_start..canvas_start + len]
                .copy_from_slice(&region_out[region_start..region_start + len]);
        }
    }
}
