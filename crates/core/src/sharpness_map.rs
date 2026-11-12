// SPDX-License-Identifier: GPL-3.0-or-later
//! Sharpness maps on one scale for a whole stack: the depth map's focus score
//! of a pixel divided by [`stack_sharpness_scale`]. Equal values mean equal
//! sharpness in every frame and in a composite. Everything runs tile by tile
//! over [`sharpness_plan`], and no value depends on the tile size.

use std::sync::Mutex;

use crate::algorithm::TileReader;
use crate::gpu::SharpnessBackend;
pub use crate::sharp_areas::sharpness_plan;
use crate::sharp_areas::{
    SharpAreasError, eligible_backend, frame_workers, score_frames_of_tile, score_tile,
};
use crate::tile::{PlannedTile, Rect, TileControl};

/// The share of pixels whose peak focus score lies at or below the scale.
pub const SHARPNESS_PERCENTILE: f64 = 0.99;

/// Histogram bins over the bit patterns of finite non-negative `f32` scores
/// (`to_bits() >> BIN_SHIFT`): a bin is 2^-11 wide relative to its value.
const BIN_SHIFT: u32 = 12;
const BINS: usize = ((f32::MAX.to_bits() >> BIN_SHIFT) + 1) as usize;

/// The stack-wide sharpness scale: the [`SHARPNESS_PERCENTILE`] of each
/// pixel's peak focus score over `frames` frames of `extent`, read tile by
/// tile of `tile_size`. Taken over per-pixel peaks, it does not move with the
/// frame count. It is the lower edge of its histogram bin (relative width
/// 2^-11), identical at every tile size and admission cap; 0.0 without frames
/// or detail. `control` stops the pass with [`SharpAreasError::Stopped`].
pub fn stack_sharpness_scale(
    reader: &dyn TileReader,
    frames: usize,
    extent: (u32, u32),
    spp: u32,
    tile_size: u32,
    control: &dyn TileControl,
) -> Result<f32, SharpAreasError> {
    let plan = sharpness_plan(extent, tile_size)?;
    if frames == 0 {
        return Ok(0.0);
    }
    let (pool, gate) = frame_workers()?;
    let mut histogram = vec![0u64; BINS];
    for tile in plan.tiles() {
        let (read, interior) = (tile.read(), tile.interior());
        let peaks = Mutex::new(vec![
            0.0f32;
            interior.width as usize * interior.height as usize
        ]);
        score_frames_of_tile(
            &pool,
            &gate,
            reader,
            read,
            spp,
            control,
            None,
            &mut vec![(); frames],
            |(), scores| {
                let mut peaks = peaks.lock().unwrap();
                for (row, peak_row) in peaks.chunks_exact_mut(interior.width as usize).enumerate() {
                    let start = ((interior.y - read.y) as usize + row) * read.width as usize
                        + (interior.x - read.x) as usize;
                    for (peak, score) in peak_row.iter_mut().zip(&scores[start..]) {
                        *peak = peak.max(*score);
                    }
                }
            },
        )?;
        for peak in peaks.into_inner().unwrap() {
            histogram[(peak.to_bits() >> BIN_SHIFT) as usize] += 1;
        }
    }
    let pixels = u64::from(extent.0) * u64::from(extent.1);
    let rank = ((SHARPNESS_PERCENTILE * pixels as f64).ceil() as u64).max(1);
    let mut seen = 0;
    let bin = histogram
        .iter()
        .position(|count| {
            seen += count;
            seen >= rank
        })
        .expect("the histogram holds every pixel, and the rank is at most the pixel count");
    Ok(f32::from_bits((bin as u32) << BIN_SHIFT))
}

/// The sharpness map of `frame` over `tile`'s interior, row-major: each
/// pixel's focus score divided by `scale`. A value above 1.0 is sharper than
/// the scale and is kept. A `scale` that is not positive gives all zeros.
///
/// A composite image is mapped by calling this on a reader of the composite
/// (frame 0) with the frames' scale.
pub fn sharpness_map_tile(
    reader: &dyn TileReader,
    frame: usize,
    tile: &PlannedTile,
    spp: u32,
    scale: f32,
) -> Result<Vec<f32>, SharpAreasError> {
    let (read, interior) = (tile.read(), tile.interior());
    let scores = score_tile(reader, frame, read, spp, &mut Vec::new())?;
    Ok(interior_map(&scores, read, interior, scale))
}

/// The scores over `interior`, which lies inside `read`, divided by `scale`
/// (zeros for a `scale` that is not positive).
fn interior_map(scores: &[f32], read: Rect, interior: Rect, scale: f32) -> Vec<f32> {
    let mut map = Vec::with_capacity(interior.width as usize * interior.height as usize);
    for y in interior.y..interior.y + interior.height {
        let start = ((y - read.y) * read.width + (interior.x - read.x)) as usize;
        let row = &scores[start..start + interior.width as usize];
        map.extend(
            row.iter()
                .map(|score| if scale > 0.0 { score / scale } else { 0.0 }),
        );
    }
    map
}

/// The sharpness maps of `frames` frames of `extent` as [`sharpness_map_tile`]
/// gives them, computed tile by tile of `tile_size`: the frames of one tile
/// are scored in parallel (by `backend` when the run reaches its
/// [`SharpnessBackend::min_pixel_frames`]) and `write(frame, interior, map)`
/// is called for each frame in frame order before the next tile. `control`
/// stops the run with [`SharpAreasError::Stopped`].
#[allow(clippy::too_many_arguments)]
pub fn sharpness_maps<E: From<SharpAreasError>>(
    reader: &dyn TileReader,
    frames: usize,
    extent: (u32, u32),
    spp: u32,
    scale: f32,
    tile_size: u32,
    control: &dyn TileControl,
    backend: Option<&dyn SharpnessBackend>,
    mut write: impl FnMut(usize, Rect, &[f32]) -> Result<(), E>,
) -> Result<(), E> {
    let plan = sharpness_plan(extent, tile_size).map_err(SharpAreasError::from)?;
    let backend = eligible_backend(backend, frames, extent);
    let (pool, gate) = frame_workers().map_err(SharpAreasError::from)?;
    let mut maps = vec![Vec::new(); frames];
    for tile in plan.tiles() {
        let (read, interior) = (tile.read(), tile.interior());
        score_frames_of_tile(
            &pool,
            &gate,
            reader,
            read,
            spp,
            control,
            backend,
            &mut maps,
            |map, scores| *map = interior_map(scores, read, interior, scale),
        )?;
        for (frame, map) in maps.iter().enumerate() {
            write(frame, interior, map)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::algorithm::{AlgorithmError, TileReader};
    use crate::fuse::laplacian::score_plane;
    use crate::fuse::sharpness::DETAIL_REACH;
    use crate::fuse::sharpness::detail_plane;
    use crate::job::JobControl;
    use crate::sharp_areas::double;
    use crate::sharp_areas::{SCORE_RADIUS, SharpAreasError};
    use crate::tile::{NoControl, Rect, TileControl};

    struct Stack {
        width: u32,
        height: u32,
        spp: u32,
        frames: Vec<Vec<u16>>,
        reads: AtomicUsize,
    }

    impl TileReader for Stack {
        fn read_tile_into(
            &self,
            frame: usize,
            rect: Rect,
            out: &mut [u16],
        ) -> Result<(), AlgorithmError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let row_len = rect.width as usize * self.spp as usize;
            for (row, chunk) in out.chunks_exact_mut(row_len).enumerate() {
                let start = ((rect.y as usize + row) * self.width as usize + rect.x as usize)
                    * self.spp as usize;
                chunk.copy_from_slice(&self.frames[frame][start..start + row_len]);
            }
            Ok(())
        }
    }

    fn texture(x: u32, y: u32) -> i32 {
        let h =
            (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)).wrapping_mul(2_246_822_519);
        i32::from((h >> 16) as u16 | 0x4000)
    }

    /// Grey 30 000 plus texture scaled by `amplitude(x, y)` in `0..=1`; each
    /// channel of a pixel carries its own texture.
    fn plane(w: u32, h: u32, spp: u32, amplitude: impl Fn(u32, u32) -> f32) -> Vec<u16> {
        let mut samples = Vec::new();
        for y in 0..h {
            for x in 0..w {
                for c in 0..spp {
                    let swing = (texture(x + c * 7919, y) - 40_000) as f32 * amplitude(x, y);
                    samples.push((30_000.0 + swing).round() as u16);
                }
            }
        }
        samples
    }

    fn stack(w: u32, h: u32, spp: u32, frames: Vec<Vec<u16>>) -> Stack {
        Stack {
            width: w,
            height: h,
            spp,
            frames,
            reads: AtomicUsize::new(0),
        }
    }

    /// Frame `k`'s amplitude steps with the column band and `k`, so the
    /// frame that is sharpest changes along the image.
    fn graded(n: usize, w: u32, h: u32, spp: u32) -> Stack {
        let frames = (0..n)
            .map(|k| {
                plane(w, h, spp, |x, _| {
                    (1 + (x / 16 + k as u32) % (n as u32 + 1)) as f32 / (n as f32 + 2.0)
                })
            })
            .collect();
        stack(w, h, spp, frames)
    }

    fn whole_scores(stack: &Stack, frame: usize) -> Vec<f32> {
        let detail =
            detail_plane(&stack.frames[frame], stack.width, stack.height, stack.spp).unwrap();
        score_plane(&detail, SCORE_RADIUS)
    }

    /// The scale from whole-frame scores: sort the per-pixel peaks' bins and
    /// pick the percentile's rank.
    fn oracle_scale(stack: &Stack) -> f32 {
        let mut peaks = vec![0.0f32; (stack.width * stack.height) as usize];
        for frame in 0..stack.frames.len() {
            for (peak, score) in peaks.iter_mut().zip(whole_scores(stack, frame)) {
                *peak = peak.max(score);
            }
        }
        let mut bins: Vec<u32> = peaks.iter().map(|p| p.to_bits() >> 12).collect();
        bins.sort_unstable();
        let rank = ((0.99 * bins.len() as f64).ceil() as usize).max(1);
        f32::from_bits(bins[rank - 1] << 12)
    }

    fn scale_at(stack: &Stack, tile: u32, control: &dyn TileControl) -> f32 {
        stack_sharpness_scale(
            stack,
            stack.frames.len(),
            (stack.width, stack.height),
            stack.spp,
            tile,
            control,
        )
        .unwrap()
    }

    /// `frame`'s map stitched from the plan's tiles.
    fn stitched(stack: &Stack, frame: usize, tile: u32, scale: f32) -> Vec<f32> {
        let plan = sharpness_plan((stack.width, stack.height), tile).unwrap();
        let mut map = vec![f32::NAN; (stack.width * stack.height) as usize];
        for planned in plan.tiles() {
            let values = sharpness_map_tile(stack, frame, planned, stack.spp, scale).unwrap();
            let rect = planned.interior();
            for (row, chunk) in values.chunks_exact(rect.width as usize).enumerate() {
                let start = (rect.y as usize + row) * stack.width as usize + rect.x as usize;
                map[start..start + chunk.len()].copy_from_slice(chunk);
            }
        }
        map
    }

    fn bits(values: &[f32]) -> Vec<u32> {
        values.iter().map(|v| v.to_bits()).collect()
    }

    #[test]
    fn stack_sharpness_scale_matches_whole_frame_percentile() {
        for (w, h) in [(101, 67), (96, 80)] {
            for spp in [1, 3] {
                for n in [1, 2, 5] {
                    let stack = graded(n, w, h, spp);
                    let wanted = oracle_scale(&stack);
                    let at = format!("{n} frames, {w}x{h}, spp {spp}");
                    assert!(wanted > 0.0, "{at}: the stack has detail");
                    assert_eq!(wanted.to_bits() & 0xFFF, 0, "{at}: a bin's lower edge");
                    let got = scale_at(&stack, 80, &NoControl);
                    assert_eq!(got.to_bits(), wanted.to_bits(), "{at}: {got} vs {wanted}");
                }
            }
        }
    }

    /// `sharpness_maps` over `stack`, stitched per frame.
    fn mapped(
        stack: &Stack,
        tile: u32,
        scale: f32,
        backend: Option<&dyn crate::gpu::SharpnessBackend>,
    ) -> Result<Vec<Vec<f32>>, SharpAreasError> {
        let (w, h) = (stack.width as usize, stack.height as usize);
        let mut maps = vec![vec![f32::NAN; w * h]; stack.frames.len()];
        sharpness_maps(
            stack,
            stack.frames.len(),
            (stack.width, stack.height),
            stack.spp,
            scale,
            tile,
            &NoControl,
            backend,
            |frame, rect: Rect, values: &[f32]| -> Result<(), SharpAreasError> {
                for (row, chunk) in values.chunks_exact(rect.width as usize).enumerate() {
                    let start = (rect.y as usize + row) * w + rect.x as usize;
                    maps[frame][start..start + chunk.len()].copy_from_slice(chunk);
                }
                Ok(())
            },
        )?;
        Ok(maps)
    }

    fn frame_tiles(stack: &Stack, tile: u32) -> usize {
        stack.frames.len()
            * sharpness_plan((stack.width, stack.height), tile)
                .unwrap()
                .tiles()
                .len()
    }

    fn assert_maps_close(got: &[Vec<f32>], want: &[Vec<f32>], at: &str) {
        for (frame, (got, want)) in got.iter().zip(want).enumerate() {
            for (i, (g, w)) in got.iter().zip(want).enumerate() {
                assert!(
                    (g - w).abs() <= 4e-6 * w.abs(),
                    "{at}: frame {frame} value {i}: {g} vs {w}"
                );
            }
        }
    }

    #[test]
    fn sharpness_backend_maps_match_the_per_tile_oracle() {
        for spp in [1, 3] {
            let stack = graded(3, 101, 67, spp);
            let scale = scale_at(&stack, 80, &NoControl);
            let want: Vec<Vec<f32>> = (0..3).map(|f| stitched(&stack, f, 80, scale)).collect();
            let at = format!("spp {spp}");
            assert_eq!(
                mapped(&stack, 80, scale, None).unwrap(),
                want,
                "{at}: the CPU maps"
            );
            let double = double::Double::default();
            let got = mapped(&stack, 80, scale, Some(&double)).unwrap();
            assert_maps_close(&got, &want, &at);
            assert_eq!(double.calls.load(Ordering::SeqCst), frame_tiles(&stack, 80));
            let zero = mapped(&stack, 80, 0.0, Some(&double)).unwrap();
            assert!(zero.iter().flatten().all(|v| *v == 0.0), "{at}: scale 0");
        }
    }

    #[test]
    fn sharpness_backend_maps_are_not_offered_work_under_its_threshold() {
        let stack = graded(3, 101, 67, 1);
        let scale = scale_at(&stack, 80, &NoControl);
        let double = double::Double {
            min_pixel_frames: 3 * 101 * 67 + 1,
            ..Default::default()
        };
        let got = mapped(&stack, 80, scale, Some(&double)).unwrap();
        assert_eq!(got, mapped(&stack, 80, scale, None).unwrap());
        assert_eq!(double.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn sharpness_backend_maps_finish_on_the_cpu_after_declines_and_losses() {
        for spp in [1, 3] {
            let stack = graded(3, 101, 67, spp);
            let scale = scale_at(&stack, 80, &NoControl);
            let want = mapped(&stack, 80, scale, None).unwrap();
            let declining = double::Double {
                decline_every_second: true,
                ..Default::default()
            };
            let got = mapped(&stack, 80, scale, Some(&declining)).unwrap();
            assert_maps_close(&got, &want, "declining");
            let losing = double::Double {
                fail_from: Some(3),
                fail_loses_device: true,
                ..Default::default()
            };
            let got = mapped(&stack, 80, scale, Some(&losing)).unwrap();
            assert_maps_close(&got, &want, "losing");
            assert_eq!(losing.calls.load(Ordering::SeqCst), 3);
            assert_eq!(losing.calls_after_loss.load(Ordering::SeqCst), 0);
            let failing = double::Double {
                fail_from: Some(3),
                ..Default::default()
            };
            let result = mapped(&stack, 80, scale, Some(&failing));
            assert!(
                matches!(result, Err(SharpAreasError::Read(_))),
                "{result:?}"
            );
        }
    }

    #[test]
    fn sharpness_backend_counts_on_three_channels_match_the_cpu() {
        let stack = graded(3, 101, 67, 3);
        let count = |backend: Option<&dyn crate::gpu::SharpnessBackend>| {
            crate::sharp_areas::count_sharp_areas_reporting_with_backend(
                &stack,
                3,
                (101, 67),
                3,
                &crate::params::ParamValues::new(),
                80,
                &NoControl,
                &mut |_| {},
                backend,
            )
            .unwrap()
        };
        let double = double::Double::default();
        assert_eq!(count(Some(&double)), count(None));
        assert_eq!(double.calls.load(Ordering::SeqCst), frame_tiles(&stack, 80));
    }

    #[test]
    fn sharpness_maps_do_not_depend_on_tiling() {
        // A tile's interior is the tile size minus twice the reach, so tiles
        // of 96 and 112 cut 160x160 and 192x96 into equal 80 and 96 px interiors.
        let cases: [(u32, u32, u32, &[u32]); 3] = [
            (101, 67, 1, &[80, 96, 128]),
            (160, 160, 1, &[96, 80]),
            (192, 96, 3, &[112, 80]),
        ];
        for (w, h, spp, tiles) in cases {
            let stack = graded(3, w, h, spp);
            if w != 101 {
                let plan = sharpness_plan((w, h), tiles[0]).unwrap();
                let side = tiles[0] - 2 * (DETAIL_REACH + SCORE_RADIUS);
                assert!(plan.tiles().iter().all(|t| {
                    let r = t.interior();
                    r.width == side && r.height == side.min(h)
                }));
            }
            let one_tile = scale_at(&stack, 1000, &NoControl);
            let whole: Vec<f32> = whole_scores(&stack, 1)
                .iter()
                .map(|score| score / one_tile)
                .collect();
            for &tile in tiles {
                let at = format!("{w}x{h}, spp {spp}, tile {tile}");
                let scale = scale_at(&stack, tile, &NoControl);
                assert_eq!(scale.to_bits(), one_tile.to_bits(), "scale at {at}");
                let map = stitched(&stack, 1, tile, one_tile);
                assert_eq!(bits(&map), bits(&whole), "map at {at}");
            }
            let map = stitched(&stack, 1, 1000, one_tile);
            assert_eq!(bits(&map), bits(&whole), "map in one tile, {w}x{h}");
            for cap in [1, 4] {
                let control = JobControl::new();
                control.set_admission_cap(Some(cap));
                let scale = scale_at(&stack, tiles[0], &*control);
                assert_eq!(
                    scale.to_bits(),
                    one_tile.to_bits(),
                    "{w}x{h} scale at cap {cap}"
                );
            }
        }
    }

    #[test]
    fn frame_sharpness_map_is_score_over_stack_scale() {
        let (w, h) = (101, 67);
        let half = |gain: f32| move |x: u32, _| if x < w / 2 { gain } else { 0.0 };
        let stack = stack(
            w,
            h,
            1,
            vec![plane(w, h, 1, half(1.0)), plane(w, h, 1, half(0.25))],
        );
        let scale = scale_at(&stack, 80, &NoControl);
        assert!(scale > 0.0);
        let sharp = stitched(&stack, 0, 80, scale);
        let blurred = stitched(&stack, 1, 80, scale);
        let score = whole_scores(&stack, 0);
        assert_eq!(
            sharp[20 * w as usize + 20],
            score[20 * w as usize + 20] / scale
        );
        let mean = |map: &[f32]| {
            let region: Vec<f32> = (0..h)
                .flat_map(|y| (8..w / 2 - 8).map(move |x| (y * w + x) as usize))
                .map(|i| map[i])
                .collect();
            region.iter().sum::<f32>() / region.len() as f32
        };
        assert!(
            mean(&sharp) > 2.0 * mean(&blurred),
            "{} vs {}",
            mean(&sharp),
            mean(&blurred)
        );
        for map in [&sharp, &blurred] {
            for y in 0..h {
                for x in w / 2 + 12..w {
                    assert_eq!(map[(y * w + x) as usize], 0.0, "uniform region at {x},{y}");
                }
            }
        }
    }

    #[test]
    fn composite_sharpness_map_uses_the_stack_scale() {
        let (w, h) = (101, 67);
        let frames: Vec<Vec<u16>> = (0..3)
            .map(|k| plane(w, h, 1, move |x, _| 0.15 * (1 + (x / 16 + k) % 3) as f32))
            .collect();
        let frames = stack(w, h, 1, frames);
        let scale = scale_at(&frames, 80, &NoControl);
        let above_one =
            |map: &[f32]| map.iter().filter(|v| **v > 1.0).count() as f32 / map.len() as f32;

        let same = stack(w, h, 1, vec![frames.frames[2].clone()]);
        assert_eq!(
            bits(&stitched(&same, 0, 80, scale)),
            bits(&stitched(&frames, 2, 80, scale)),
            "a composite identical to frame 2"
        );

        let sharper = stack(w, h, 1, vec![plane(w, h, 1, |_, _| 1.0)]);
        let composite = stitched(&sharper, 0, 80, scale);
        assert!(above_one(&composite) > 0.9, "{}", above_one(&composite));
        for k in 0..3 {
            let share = above_one(&stitched(&frames, k, 80, scale));
            assert!(share < 0.05, "frame {k} reads above 1.0 in {share}");
        }
    }

    struct StopAfter {
        units: usize,
        done: AtomicUsize,
    }

    impl TileControl for StopAfter {
        fn should_stop(&self) -> bool {
            self.done.load(Ordering::SeqCst) >= self.units
        }

        fn unit_completed(&self, _: u64) {
            self.done.fetch_add(1, Ordering::SeqCst);
        }

        fn admission_cap(&self) -> usize {
            1
        }
    }

    #[test]
    fn sharpness_map_flat_stack_and_stop() {
        let (w, h) = (101, 67);
        let flat = stack(w, h, 1, vec![plane(w, h, 1, |_, _| 0.0); 2]);
        assert_eq!(scale_at(&flat, 80, &NoControl), 0.0);
        let none = stack_sharpness_scale(&flat, 0, (w, h), 1, 80, &NoControl).unwrap();
        assert_eq!(none, 0.0, "a stack without frames");
        for map in [stitched(&flat, 0, 80, 0.0), {
            let textured = stack(w, h, 1, vec![plane(w, h, 1, |_, _| 1.0)]);
            stitched(&textured, 0, 80, 0.0)
        }] {
            assert!(map.iter().all(|v| *v == 0.0), "scale 0 gives zeros");
        }

        for units in [0, 1] {
            let stack = graded(3, w, h, 1);
            let control = StopAfter {
                units,
                done: AtomicUsize::new(0),
            };
            let result = stack_sharpness_scale(&stack, 3, (w, h), 1, 80, &control);
            assert!(
                matches!(result, Err(SharpAreasError::Stopped)),
                "{result:?}"
            );
            assert_eq!(
                stack.reads.load(Ordering::SeqCst),
                units,
                "reads after the stop"
            );
        }
    }
}
