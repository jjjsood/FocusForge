// SPDX-License-Identifier: GPL-3.0-or-later
//! Sharp areas per image: square cells of `cell-size` px from the top left
//! (smaller at the right and bottom edges), each scored per frame by the mean
//! depth-map focus score. A cell counts in the frame of its peak score (the
//! lowest index on a tie) when the peak exceeds `contrast` times its lowest
//! score; any other cell counts in no frame.

use std::sync::LazyLock;

use serde::Serialize;

use crate::algorithm::{AlgorithmError, TileReader};
use crate::fuse::PyramidError;
use crate::fuse::laplacian::score_plane;
use crate::fuse::sharpness::{DETAIL_REACH, detail_plane};
use crate::params::{self, ParamDesc, ParamError, ParamKind, ParamValue, ParamValues};
use crate::tile::plan::TileSupport;
use crate::tile::{TileControl, TileError, TilePlan};

/// Radius of the box the focus score sums over, the depth map's default.
const SCORE_RADIUS: u32 = 4;

/// The count's inputs, in the shape every front end reads.
pub static SHARP_AREA_PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![
        ParamDesc {
            id: "cell-size".to_string(),
            label: "Area size (px)".to_string(),
            help: "Side of the square cells the image is divided into; each cell is counted \
                   in one frame."
                .to_string(),
            kind: ParamKind::Int { min: 16, max: 512 },
            default: ParamValue::Int(64),
            advanced: false,
            auto: None,
            host_default: None,
            stack_max: None,
        },
        ParamDesc {
            id: "contrast".to_string(),
            label: "Peak-to-floor contrast".to_string(),
            help: "A cell is counted when its best frame scores more than this many times \
                   its worst frame; a cell without that contrast is counted in no frame."
                .to_string(),
            kind: ParamKind::Float {
                min: 1.0,
                max: 10.0,
                step: 0.1,
                logarithmic: false,
            },
            default: ParamValue::Float(2.0),
            advanced: false,
            auto: None,
            host_default: None,
            stack_max: None,
        },
    ]
});

/// The result of [`count_sharp_areas`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SharpAreas {
    /// Cells in the image: `ceil(width / cell-size) × ceil(height / cell-size)`.
    pub cells: u32,
    /// Cells counted in each frame, in input order.
    pub counts: Vec<u32>,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SharpAreasError {
    #[error(transparent)]
    Params(#[from] ParamError),
    #[error(transparent)]
    Read(#[from] AlgorithmError),
    #[error(transparent)]
    Tile(#[from] TileError),
    #[error(transparent)]
    Detail(#[from] PyramidError),
    #[error("the count was stopped")]
    Stopped,
}

/// Counts the sharp areas of each of `frames` frames of `(width, height)`
/// with `spp` samples per pixel, read tile by tile of `tile_size` from
/// `reader`. The result does not depend on `tile_size`.
///
/// `values` are checked against [`SHARP_AREA_PARAMS`] before any read.
/// `control` is asked to stop before each tile and told of each finished one.
pub fn count_sharp_areas(
    reader: &dyn TileReader,
    frames: usize,
    (width, height): (u32, u32),
    spp: u32,
    values: &ParamValues,
    tile_size: u32,
    control: &dyn TileControl,
) -> Result<SharpAreas, SharpAreasError> {
    params::validate(&SHARP_AREA_PARAMS, values)?;
    let resolved = params::resolved_values(&params::resolve_in(
        &SHARP_AREA_PARAMS,
        values,
        None,
        &crate::host::snapshot(),
    )?);
    let cell = resolved["cell-size"]
        .as_i64()
        .expect("cell-size resolves to an integer") as u32;
    let contrast = resolved["contrast"]
        .as_f64()
        .expect("contrast resolves to a number");

    let plan = TilePlan::new(
        width,
        height,
        tile_size,
        TileSupport {
            reach_px: DETAIL_REACH + SCORE_RADIUS,
            decimation_levels: 1,
        },
    )?;
    let columns = width.div_ceil(cell) as usize;
    let cells = columns * height.div_ceil(cell) as usize;
    // Pixels in each cell: the edge cells are cut short.
    let area: Vec<f64> = (0..cells)
        .map(|index| {
            let (column, row) = ((index % columns) as u32, (index / columns) as u32);
            let across = cell.min(width - column * cell);
            let down = cell.min(height - row * cell);
            f64::from(across) * f64::from(down)
        })
        .collect();

    let mut peak = vec![f64::NEG_INFINITY; cells];
    let mut peak_frame = vec![0usize; cells];
    let mut floor = vec![f64::INFINITY; cells];
    let mut sums = vec![0.0f64; cells];
    let mut samples = Vec::new();
    for frame in 0..frames {
        sums.fill(0.0);
        for tile in plan.tiles() {
            if control.should_stop() {
                return Err(SharpAreasError::Stopped);
            }
            let (read, interior) = (tile.read(), tile.interior());
            samples.resize(
                read.width as usize * read.height as usize * spp as usize,
                0u16,
            );
            reader.read_tile_into(frame, read, &mut samples)?;
            let detail = detail_plane(&samples, read.width, read.height, spp)?;
            let scores = score_plane(&detail, SCORE_RADIUS);
            for y in interior.y..interior.y + interior.height {
                let row = ((y - read.y) * read.width) as usize;
                let cell_row = (y / cell) as usize * columns;
                for x in interior.x..interior.x + interior.width {
                    sums[cell_row + (x / cell) as usize] +=
                        f64::from(scores[row + (x - read.x) as usize]);
                }
            }
            control.unit_completed(u64::from(read.width) * u64::from(read.height));
        }
        for index in 0..cells {
            let mean = sums[index] / area[index];
            if mean > peak[index] {
                peak[index] = mean;
                peak_frame[index] = frame;
            }
            floor[index] = floor[index].min(mean);
        }
    }

    let mut counts = vec![0u32; frames];
    for index in 0..cells {
        if frames > 0 && peak[index] > contrast * floor[index] {
            counts[peak_frame[index]] += 1;
        }
    }
    Ok(SharpAreas {
        cells: cells as u32,
        counts,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::io::{FrameError, TiffError};
    use crate::params::ParamViolation;
    use crate::tile::{NoControl, Rect};

    const WIDTH: u32 = 200;
    const HEIGHT: u32 = 136;
    const TILE: u32 = 96;

    struct VecReader {
        frames: Vec<Vec<u16>>,
    }

    impl TileReader for VecReader {
        fn read_tile_into(
            &self,
            frame: usize,
            rect: Rect,
            out: &mut [u16],
        ) -> Result<(), AlgorithmError> {
            let expected = rect.width as usize * rect.height as usize;
            if out.len() != expected {
                return Err(AlgorithmError::Frame(FrameError::Tiff(
                    TiffError::BufferSize {
                        expected,
                        got: out.len(),
                    },
                )));
            }
            for (row, chunk) in out.chunks_exact_mut(rect.width as usize).enumerate() {
                let start = (rect.y as usize + row) * WIDTH as usize + rect.x as usize;
                chunk.copy_from_slice(&self.frames[frame][start..start + chunk.len()]);
            }
            Ok(())
        }
    }

    struct PanicReader;

    impl TileReader for PanicReader {
        fn read_tile_into(&self, _: usize, _: Rect, _: &mut [u16]) -> Result<(), AlgorithmError> {
            panic!("a refused count must not read");
        }
    }

    fn texture(x: u32, y: u32) -> u16 {
        let h =
            (x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)).wrapping_mul(2_246_822_519);
        ((h >> 16) as u16) | 0x4000
    }

    /// A grey frame, textured at the columns where `textured` holds.
    fn frame(textured: impl Fn(u32) -> bool) -> Vec<u16> {
        let mut samples = Vec::new();
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                samples.push(if textured(x) { texture(x, y) } else { 30_000 });
            }
        }
        samples
    }

    /// Frame 0 is sharp left of column 128, frame 2 right of it, frame 1 flat.
    fn split_stack() -> VecReader {
        VecReader {
            frames: vec![frame(|x| x < 128), frame(|_| false), frame(|x| x >= 128)],
        }
    }

    fn values(pairs: &[(&str, ParamValue)]) -> ParamValues {
        pairs
            .iter()
            .map(|(id, value)| ((*id).to_string(), value.clone()))
            .collect()
    }

    fn count(reader: &VecReader, values: &ParamValues) -> Result<SharpAreas, SharpAreasError> {
        count_sharp_areas(
            reader,
            reader.frames.len(),
            (WIDTH, HEIGHT),
            1,
            values,
            TILE,
            &NoControl,
        )
    }

    #[test]
    fn a_split_stack_counts_each_side_in_its_sharp_end_frame() {
        let result = count(&split_stack(), &ParamValues::new()).unwrap();
        // 4 columns × 3 rows of 64 px cells; the edge cells are smaller.
        assert_eq!(result.cells, 12);
        assert_eq!(result.counts, vec![6, 0, 6]);
    }

    #[test]
    fn a_tied_peak_counts_in_the_lowest_index_frame() {
        let tied = VecReader {
            frames: vec![frame(|_| true), frame(|_| true), frame(|_| false)],
        };
        let result = count(&tied, &ParamValues::new()).unwrap();
        assert_eq!(result.counts, vec![12, 0, 0]);
    }

    #[test]
    fn a_flat_stack_counts_nothing() {
        let flat = VecReader {
            frames: vec![frame(|_| false), frame(|_| false)],
        };
        let result = count(&flat, &ParamValues::new()).unwrap();
        assert_eq!(result.counts, vec![0, 0]);
    }

    #[test]
    fn a_single_frame_counts_nothing() {
        let single = VecReader {
            frames: vec![frame(|_| true)],
        };
        let result = count(&single, &ParamValues::new()).unwrap();
        assert_eq!(result.counts, vec![0]);
    }

    #[test]
    fn the_cell_size_sets_the_cell_count() {
        let result = count(
            &split_stack(),
            &values(&[("cell-size", ParamValue::Int(100))]),
        )
        .unwrap();
        assert_eq!(result.cells, 4);
    }

    /// Frame 0 at full texture, frame 1 at a third of its amplitude: every
    /// cell's peak is about three times its floor.
    #[test]
    fn the_contrast_decides_whether_a_cell_counts() {
        let faint = (0..WIDTH * HEIGHT)
            .map(|i| {
                let t = i32::from(texture(i % WIDTH, i / WIDTH));
                (30_000 + (t - 30_000) / 3) as u16
            })
            .collect();
        let stack = VecReader {
            frames: vec![frame(|_| true), faint],
        };
        let at = |contrast| {
            count(
                &stack,
                &values(&[("contrast", ParamValue::Float(contrast))]),
            )
        };
        assert_eq!(at(2.0).unwrap().counts, vec![12, 0]);
        assert_eq!(at(4.0).unwrap().counts, vec![0, 0]);
    }

    struct Recorder {
        stop_after: usize,
        pixels: Mutex<Vec<u64>>,
        asked: AtomicUsize,
    }

    impl TileControl for Recorder {
        fn should_stop(&self) -> bool {
            self.asked.fetch_add(1, Ordering::Relaxed);
            self.pixels.lock().unwrap().len() >= self.stop_after
        }

        fn unit_completed(&self, pixels: u64) {
            self.pixels.lock().unwrap().push(pixels);
        }
    }

    fn run_with(stop_after: usize) -> (Recorder, Result<SharpAreas, SharpAreasError>) {
        let control = Recorder {
            stop_after,
            pixels: Mutex::new(Vec::new()),
            asked: AtomicUsize::new(0),
        };
        let result = count_sharp_areas(
            &split_stack(),
            3,
            (WIDTH, HEIGHT),
            1,
            &ParamValues::new(),
            TILE,
            &control,
        );
        (control, result)
    }

    #[test]
    fn a_stopping_control_ends_the_count_after_its_units() {
        let (control, result) = run_with(2);
        assert!(
            matches!(result, Err(SharpAreasError::Stopped)),
            "{result:?}"
        );
        assert_eq!(control.pixels.lock().unwrap().len(), 2);
        assert_eq!(control.asked.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn every_finished_tile_reports_its_read_area() {
        let (control, result) = run_with(usize::MAX);
        result.unwrap();
        let plan = TilePlan::new(
            WIDTH,
            HEIGHT,
            TILE,
            TileSupport {
                reach_px: DETAIL_REACH + SCORE_RADIUS,
                decimation_levels: 1,
            },
        )
        .unwrap();
        let per_frame: Vec<u64> = plan
            .tiles()
            .iter()
            .map(|tile| u64::from(tile.read().width) * u64::from(tile.read().height))
            .collect();
        assert!(per_frame.len() > 1, "the tile size is below the extent");
        let expected: Vec<u64> = (0..3).flat_map(|_| per_frame.clone()).collect();
        assert_eq!(*control.pixels.lock().unwrap(), expected);
    }

    #[test]
    fn the_table_declares_both_parameters_with_their_bounds() {
        let resolved = params::resolve_in(
            &SHARP_AREA_PARAMS,
            &ParamValues::new(),
            None,
            &crate::host::snapshot(),
        )
        .unwrap();
        let resolved = params::resolved_values(&resolved);
        assert_eq!(resolved["cell-size"], ParamValue::Int(64));
        assert_eq!(resolved["contrast"], ParamValue::Float(2.0));
        assert!(matches!(
            SHARP_AREA_PARAMS[0].kind,
            ParamKind::Int { min: 16, max: 512 }
        ));
        assert!(matches!(
            SHARP_AREA_PARAMS[1].kind,
            ParamKind::Float { min, max, .. } if min == 1.0 && max == 10.0
        ));
        assert!(
            SHARP_AREA_PARAMS
                .iter()
                .all(|desc| !desc.label.is_empty() && !desc.help.is_empty())
        );
    }

    #[test]
    fn an_out_of_range_value_is_refused_before_any_read() {
        let refused = [
            ("cell-size", ParamValue::Int(15)),
            ("cell-size", ParamValue::Int(513)),
            ("contrast", ParamValue::Float(0.5)),
        ];
        for (id, value) in refused {
            let result = count_sharp_areas(
                &PanicReader,
                3,
                (WIDTH, HEIGHT),
                1,
                &values(&[(id, value)]),
                TILE,
                &NoControl,
            );
            let Err(SharpAreasError::Params(err)) = result else {
                panic!("{id} must be refused: {result:?}");
            };
            assert!(
                matches!(&err.violations()[0], ParamViolation::OutOfRange { id: got, .. } if got == id),
                "{err}"
            );
        }
    }

    #[test]
    fn an_unknown_key_is_refused() {
        let result = count_sharp_areas(
            &PanicReader,
            3,
            (WIDTH, HEIGHT),
            1,
            &values(&[("cells", ParamValue::Int(64))]),
            TILE,
            &NoControl,
        );
        assert!(
            matches!(result, Err(SharpAreasError::Params(_))),
            "{result:?}"
        );
    }
}
