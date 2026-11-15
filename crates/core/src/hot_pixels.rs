// SPDX-License-Identifier: GPL-3.0-or-later

//! Hot-pixel detection on frames as read, before alignment. A stuck, hot or
//! dead sensor pixel sits at the same sensor position in every frame, so a
//! pixel is a defect when it is an outlier against its 3×3 neighbourhood in
//! more than half the frames. The result is a [`DefectMask`] in sensor
//! coordinates. Frames are read in full-width strips with a one-row halo; the
//! only whole-frame memory is the `u16` vote counter.

use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::algorithm::{AlgorithmError, TileReader};
use crate::equalize::strip_rows_for;
use crate::tile::Rect;

/// The smallest spread an outlier test divides by, on the 16-bit scale (8-bit
/// sources arrive ×257). A flat neighbourhood has a median absolute deviation
/// of zero; the floor keeps its sensor noise from counting as a defect.
pub const MIN_SPREAD: f64 = 64.0;

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

    fn index(&self, x: u32, y: u32) -> usize {
        y as usize * self.width as usize + x as usize
    }
}

/// The units [`detect`] credits to its counter: one per strip read, every
/// frame in full.
pub fn hot_pixel_units(frame_count: usize, frame: (u32, u32), samples_per_pixel: u32) -> usize {
    frame_count * frame.1.div_ceil(strip_rows_for(frame, samples_per_pixel)) as usize
}

/// The defects of `frame_count` frames of `frame` size: the pixels that are
/// an outlier by more than `threshold` times their neighbourhood's spread in
/// more than half the frames. See [`MIN_SPREAD`]. Frames are read on
/// `workers` threads; the mask is the same for any worker count. `cancelled`
/// is polled before every strip read, and `strips_done` counts the work in
/// the units of [`hot_pixel_units`].
#[allow(clippy::too_many_arguments)] // one input apiece; a struct wrapper would only rename this list
pub fn detect(
    reader: &dyn TileReader,
    frame_count: usize,
    frame: (u32, u32),
    samples_per_pixel: u32,
    threshold: f64,
    workers: usize,
    strips_done: &AtomicUsize,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<DefectMask, AlgorithmError> {
    detect_in_strips(
        reader,
        frame_count,
        frame,
        samples_per_pixel,
        threshold,
        strip_rows_for(frame, samples_per_pixel),
        workers,
        strips_done,
        cancelled,
    )
}

fn failed(detail: String) -> AlgorithmError {
    AlgorithmError::Failed {
        algorithm: "hot-pixels".to_string(),
        detail,
    }
}

#[allow(clippy::too_many_arguments)] // `detect` plus the strip height tests vary
fn detect_in_strips(
    reader: &dyn TileReader,
    frame_count: usize,
    frame: (u32, u32),
    samples_per_pixel: u32,
    threshold: f64,
    strip_rows: u32,
    workers: usize,
    strips_done: &AtomicUsize,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<DefectMask, AlgorithmError> {
    if frame_count > usize::from(u16::MAX) {
        return Err(failed(format!(
            "{frame_count} frames exceed the {} the vote counter holds",
            u16::MAX
        )));
    }
    let (width, height) = frame;
    if width == 0 || height == 0 {
        return Ok(DefectMask::new(width, height));
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers.max(1))
        .thread_name(|i| format!("focusforge-prepare-{i}"))
        .panic_handler(crate::pool::log_worker_panic)
        .build()
        .map_err(|source| failed(format!("hot pixel thread pool: {source}")))?;
    let mut votes = vec![0u16; width as usize * height as usize];
    let chunk = width as usize * strip_rows as usize;
    pool.install(|| {
        for index in 0..frame_count {
            votes
                .par_chunks_mut(chunk)
                .enumerate()
                .map_init(Vec::new, |buffer, (strip, counts)| {
                    let first = strip as u32 * strip_rows;
                    let strip = Strip {
                        frame,
                        samples_per_pixel: samples_per_pixel as usize,
                        first,
                        rows: counts.len() as u32 / width,
                    };
                    if cancelled() {
                        return Err(AlgorithmError::Cancelled);
                    }
                    vote_strip(reader, index, &strip, threshold, buffer, counts)?;
                    strips_done.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .collect::<Result<(), AlgorithmError>>()?;
        }
        Ok::<_, AlgorithmError>(())
    })?;
    let mut mask = DefectMask::new(width, height);
    for (at, &count) in votes.iter().enumerate() {
        if 2 * usize::from(count) > frame_count {
            mask.insert(at as u32 % width, at as u32 / width);
        }
    }
    Ok(mask)
}

/// The rows `first..first + rows` of a frame.
struct Strip {
    frame: (u32, u32),
    samples_per_pixel: usize,
    first: u32,
    rows: u32,
}

/// Adds one vote to `counts`, the strip's row-major counters, for every
/// pixel of the strip of frame `index` that is an outlier in any channel.
/// The strip is read with the rows above and below it that the frame has.
fn vote_strip(
    reader: &dyn TileReader,
    index: usize,
    strip: &Strip,
    threshold: f64,
    buffer: &mut Vec<u16>,
    counts: &mut [u16],
) -> Result<(), AlgorithmError> {
    let (width, height) = strip.frame;
    let above = u32::from(strip.first > 0);
    let top = strip.first - above;
    let bottom = (strip.first + strip.rows + 1).min(height);
    let spp = strip.samples_per_pixel;
    let width = width as usize;
    buffer.resize(width * (bottom - top) as usize * spp, 0);
    reader.read_tile_into(
        index,
        Rect {
            x: 0,
            y: top,
            width: width as u32,
            height: bottom - top,
        },
        buffer,
    )?;
    let buffer_rows = (bottom - top) as usize;
    let sample = |x: usize, row: usize, channel: usize| buffer[(row * width + x) * spp + channel];
    for r in 0..strip.rows as usize {
        let row = r + above as usize;
        for x in 0..width {
            let outlier = (0..spp).any(|channel| {
                let mut neighbours = [0u16; 8];
                let mut n = 0;
                for ny in row.saturating_sub(1)..=(row + 1).min(buffer_rows - 1) {
                    for nx in x.saturating_sub(1)..=(x + 1).min(width - 1) {
                        if (nx, ny) != (x, row) {
                            neighbours[n] = sample(nx, ny, channel);
                            n += 1;
                        }
                    }
                }
                is_outlier(sample(x, row, channel), &neighbours[..n], threshold)
            });
            if outlier {
                counts[r * width + x] += 1;
            }
        }
    }
    Ok(())
}

/// The median of an ascending slice; the mean of the two middle values when
/// its length is even.
fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// Whether `centre` differs from the median `m` of `neighbours` by more than
/// `threshold` times their spread, in either direction. The spread is the
/// median of `|n - m|`, at least [`MIN_SPREAD`]. No neighbours, no outlier.
fn is_outlier(centre: u16, neighbours: &[u16], threshold: f64) -> bool {
    if neighbours.is_empty() {
        return false;
    }
    let mut sorted = [0f64; 8];
    let sorted = &mut sorted[..neighbours.len()];
    for (slot, &n) in sorted.iter_mut().zip(neighbours) {
        *slot = f64::from(n);
    }
    sorted.sort_by(f64::total_cmp);
    let m = median(sorted);
    let distance = (f64::from(centre) - m).abs();
    // The spread is at least MIN_SPREAD, so a distance within that bound
    // cannot be an outlier whatever the neighbours' deviations are.
    if distance <= threshold * MIN_SPREAD {
        return false;
    }
    for slot in sorted.iter_mut() {
        *slot = (*slot - m).abs();
    }
    sorted.sort_by(f64::total_cmp);
    distance > threshold * median(sorted).max(MIN_SPREAD)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEVER: &(dyn Fn() -> bool + Sync) = &|| false;

    /// Frames held in memory, interleaved channels, row-major.
    struct Frames {
        frames: Vec<Vec<u16>>,
        width: u32,
        spp: u32,
    }

    impl TileReader for Frames {
        fn read_tile_into(
            &self,
            frame: usize,
            rect: Rect,
            out: &mut [u16],
        ) -> Result<(), AlgorithmError> {
            let spp = self.spp as usize;
            let mut at = 0;
            for y in rect.y..rect.y + rect.height {
                for x in rect.x..rect.x + rect.width {
                    let px = (y * self.width + x) as usize;
                    out[at..at + spp].copy_from_slice(&self.frames[frame][px * spp..][..spp]);
                    at += spp;
                }
            }
            Ok(())
        }
    }

    fn flat(width: u32, height: u32, spp: u32, frames: usize, value: u16) -> Frames {
        Frames {
            frames: vec![vec![value; (width * height * spp) as usize]; frames],
            width,
            spp,
        }
    }

    fn set(stack: &mut Frames, frame: usize, x: u32, y: u32, channel: u32, value: u16) {
        let at = ((y * stack.width + x) * stack.spp + channel) as usize;
        stack.frames[frame][at] = value;
    }

    fn mask_of(stack: &Frames, height: u32, threshold: f64) -> DefectMask {
        detect_in_strips(
            stack,
            stack.frames.len(),
            (stack.width, height),
            stack.spp,
            threshold,
            height,
            1,
            &AtomicUsize::new(0),
            NEVER,
        )
        .unwrap()
    }

    #[test]
    fn outlier_threshold() {
        // Eight neighbours, median 1350 (mean of 1300 and 1400), spread 200.
        let eight = [1000, 1100, 1200, 1300, 1400, 1500, 1600, 1700];
        assert!(
            !is_outlier(2150, &eight, 4.0),
            "800 above is not more than 800"
        );
        assert!(is_outlier(2151, &eight, 4.0), "just above k x spread");
        assert!(
            !is_outlier(550, &eight, 4.0),
            "800 below is not more than 800"
        );
        assert!(
            is_outlier(549, &eight, 4.0),
            "just below, the other direction"
        );
        assert!(!is_outlier(1350, &eight, 4.0), "the median itself");

        // Five neighbours (an edge pixel): median 1200, spread 100.
        let five = [1000, 1100, 1200, 1300, 1400];
        assert!(!is_outlier(1600, &five, 4.0));
        assert!(is_outlier(1601, &five, 4.0));

        // Three neighbours (a corner pixel): median 2000, spread 1000.
        let three = [1000, 2000, 3000];
        assert!(!is_outlier(4000, &three, 2.0));
        assert!(is_outlier(4001, &three, 2.0));

        // A flat neighbourhood or a spread under the floor divides by the floor.
        let flat = [30_000; 8];
        assert!(!is_outlier(30_256, &flat, 4.0), "4 x MIN_SPREAD above");
        assert!(is_outlier(30_257, &flat, 4.0));
        let noisy = [
            30_000, 30_000, 30_000, 30_000, 30_010, 30_010, 30_010, 30_010,
        ];
        assert!(
            !is_outlier(30_261, &noisy, 4.0),
            "256 above the median 30005"
        );
        assert!(is_outlier(30_262, &noisy, 4.0));
        assert!(
            !is_outlier(30_100, &noisy, 4.0),
            "100 against a spread of 5 is noise"
        );

        assert!(!is_outlier(65_535, &[], 4.0), "no neighbours, no outlier");

        // Border and corner pixels use only the neighbours they have: a
        // zero-padded neighbourhood would call the flat frame's border out.
        let uniform = flat_frame(4, 3, 1);
        assert_eq!(mask_of(&uniform, 3, 4.0).count(), 0, "uniform frame");
        let mut edge = flat_frame(4, 3, 1);
        set(&mut edge, 0, 1, 0, 0, 0);
        assert_eq!(mask_of(&edge, 3, 4.0).positions(), [(1, 0)], "edge spike");
        let mut corner = flat_frame(4, 3, 1);
        set(&mut corner, 0, 3, 2, 0, 0);
        assert_eq!(
            mask_of(&corner, 3, 4.0).positions(),
            [(3, 2)],
            "corner spike"
        );

        // One channel of three is enough to vote.
        let mut one_channel = flat_frame(4, 3, 3);
        set(&mut one_channel, 0, 2, 1, 1, 0);
        assert_eq!(mask_of(&one_channel, 3, 4.0).positions(), [(2, 1)]);
    }

    /// A single frame of 40000.
    fn flat_frame(width: u32, height: u32, spp: u32) -> Frames {
        flat(width, height, spp, 1, 40_000)
    }

    /// Five 37x23 RGB frames of a mild texture, with defects in the first
    /// and last row and column, in the interior, in more and in fewer than
    /// half the frames.
    fn textured_stack() -> (Frames, Vec<(u32, u32)>) {
        let (width, height, spp) = (37u32, 23u32, 3u32);
        let mut frames = Frames {
            frames: Vec::new(),
            width,
            spp,
        };
        for _ in 0..5 {
            let mut samples = Vec::new();
            for y in 0..height {
                for x in 0..width {
                    for c in 0..spp {
                        samples.push((20_000 + (x * 7 + y * 13) % 40 + c * 100) as u16);
                    }
                }
            }
            frames.frames.push(samples);
        }
        // (x, y, value, frames it is in)
        let defects: [(u32, u32, u16, &[usize]); 6] = [
            (0, 0, 65_535, &[0, 1, 2, 3, 4]),
            (36, 0, 0, &[0, 1, 2]),
            (20, 11, 65_535, &[0, 1, 2, 3, 4]),
            (0, 22, 65_535, &[1, 2, 3, 4]),
            (36, 22, 0, &[2, 3, 4]),
            (18, 0, 65_535, &[0, 1]),
        ];
        for (x, y, value, in_frames) in defects {
            for &frame in in_frames {
                for channel in 0..spp {
                    set(&mut frames, frame, x, y, channel, value);
                }
            }
        }
        // Row-major; (18, 0) is in two of five frames only.
        (frames, vec![(0, 0), (36, 0), (20, 11), (0, 22), (36, 22)])
    }

    #[test]
    fn mask_independent_of_tiling_and_workers() {
        let (stack, expected) = textured_stack();
        let frame = (37, 23);
        let units = hot_pixel_units(5, frame, 3);
        for strip_rows in [1, 7, 64] {
            for workers in [1, 4] {
                let done = AtomicUsize::new(0);
                let mask =
                    detect_in_strips(&stack, 5, frame, 3, 6.0, strip_rows, workers, &done, NEVER)
                        .unwrap();
                assert_eq!(
                    mask.positions(),
                    expected,
                    "{workers} workers, {strip_rows} rows"
                );
                assert_eq!(
                    done.load(Ordering::SeqCst),
                    5 * 23usize.div_ceil(strip_rows as usize),
                    "{workers} workers, {strip_rows} rows"
                );
            }
        }
        let done = AtomicUsize::new(0);
        let mask = detect(&stack, 5, frame, 3, 6.0, 4, &done, NEVER).unwrap();
        assert_eq!(mask.positions(), expected, "detect");
        assert_eq!(
            done.load(Ordering::SeqCst),
            units,
            "detect credits hot_pixel_units"
        );

        // A noisy texture and a low threshold flag a share of its pixels
        // whose neighbours sit across every strip seam; each cut finds them all.
        let mut noisy = flat(37, 23, 1, 3, 0);
        let mut state = 0x2545_f491_u32;
        for sample in &mut noisy.frames[0] {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *sample = 20_000 + (state >> 18) as u16 % 6000;
        }
        let first = noisy.frames[0].clone();
        noisy.frames[1..].fill(first);
        let whole =
            detect_in_strips(&noisy, 3, frame, 1, 2.0, 23, 1, &AtomicUsize::new(0), NEVER).unwrap();
        assert!(
            whole.count() > 20,
            "the texture has pixels to find: {}",
            whole.count()
        );
        for strip_rows in [1, 7, 64] {
            for workers in [1, 4] {
                let cut = detect_in_strips(
                    &noisy,
                    3,
                    frame,
                    1,
                    2.0,
                    strip_rows,
                    workers,
                    &AtomicUsize::new(0),
                    NEVER,
                )
                .unwrap();
                assert_eq!(cut, whole, "noisy: {workers} workers, {strip_rows} rows");
            }
        }
    }

    #[test]
    fn cancels() {
        let stack = flat(8, 8, 1, 3, 1000);
        let done = AtomicUsize::new(0);
        let at_once = || true;
        let err = detect_in_strips(&stack, 3, (8, 8), 1, 6.0, 2, 1, &done, &at_once).unwrap_err();
        assert!(matches!(err, AlgorithmError::Cancelled), "{err:?}");
        assert_eq!(
            done.load(Ordering::SeqCst),
            0,
            "no strip is read once cancelled"
        );

        let polls = AtomicUsize::new(0);
        let after_two = || polls.fetch_add(1, Ordering::SeqCst) >= 2;
        let err = detect_in_strips(&stack, 3, (8, 8), 1, 6.0, 2, 1, &done, &after_two).unwrap_err();
        assert!(matches!(err, AlgorithmError::Cancelled), "{err:?}");
        assert_eq!(
            done.load(Ordering::SeqCst),
            2,
            "the two strips before the request were read"
        );
    }

    /// The mask of a fixture's frames at `threshold`, cut into strips of `strip_rows`.
    fn fixture_mask(name: &str, threshold: f64, strip_rows: u32) -> DefectMask {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures")
            .join(name)
            .join("frames");
        let paths = crate::io::frame_paths(&dir).expect("the fixture frames list");
        let set = crate::io::open_frame_set(
            &paths,
            crate::tile::HostBudget::host_derived(&crate::host::snapshot()),
        )
        .expect("the fixture opens");
        let reader = set.reader.as_ref();
        detect_in_strips(
            reader,
            reader.len(),
            reader.dimensions(),
            reader.frame(0).samples_per_pixel,
            threshold,
            strip_rows,
            4,
            &AtomicUsize::new(0),
            NEVER,
        )
        .unwrap()
    }

    /// The sidecar's positions carried by more than half the frames, row by row.
    fn majority_positions() -> Vec<(u32, u32)> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/mantis-512-hot-pixels/hot-pixels.json");
        let entries: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("the sidecar exists"))
                .expect("the sidecar is JSON");
        let mut positions: Vec<(u32, u32)> = entries
            .as_array()
            .expect("an array of defects")
            .iter()
            .filter(|entry| 2 * entry["frames"].as_array().expect("frames").len() > 8)
            .map(|entry| {
                (
                    entry["x"].as_u64().expect("x") as u32,
                    entry["y"].as_u64().expect("y") as u32,
                )
            })
            .collect();
        positions.sort_unstable_by_key(|&(x, y)| (y, x));
        positions
    }

    #[test]
    fn detects_majority_defects_on_fixture() {
        let mask = fixture_mask(
            "mantis-512-hot-pixels",
            crate::project::HOT_PIXEL_THRESHOLD_DEFAULT,
            48,
        );
        assert_eq!(mask.positions(), majority_positions());
    }

    /// Detection over every full frame of the reference stack, strip by
    /// strip. Prints the count, the wall time and the peak resident memory.
    #[test]
    #[ignore = "reads the 16 GB reference stack"]
    fn reference_stack_detection() {
        let Some(dir) = std::env::var_os("FOCUSFORGE_REFERENCE_STACK")
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_dir())
        else {
            println!("FOCUSFORGE_REFERENCE_STACK not set");
            return;
        };
        let paths: Vec<_> = crate::io::frame_paths(&dir)
            .expect("the stack directory enumerates")
            .into_iter()
            // The directory also holds the stacked result, which is not a source frame.
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains("IMG_"))
            })
            .collect();
        let stack = crate::io::TiffStack::open_files(&paths).expect("the stack opens");
        let frame = stack.dimensions();
        let spp = stack.header(0).samples_per_pixel;
        let workers = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        let done = AtomicUsize::new(0);
        let started = std::time::Instant::now();
        let mask = detect(
            &stack,
            stack.len(),
            frame,
            spp,
            crate::project::HOT_PIXEL_THRESHOLD_DEFAULT,
            workers,
            &done,
            NEVER,
        )
        .expect("detection runs");
        let elapsed = started.elapsed();
        let peak_kib = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|l| l.strip_prefix("VmHWM:"))
                    .map(|v| v.trim().to_string())
            });
        println!(
            "reference stack: {} frames of {}x{}, {} hot pixels, {:.1} s, {workers} workers, peak resident {}",
            stack.len(),
            frame.0,
            frame.1,
            mask.count(),
            elapsed.as_secs_f64(),
            peak_kib.as_deref().unwrap_or("unknown"),
        );
        assert_eq!(
            done.load(Ordering::SeqCst),
            hot_pixel_units(stack.len(), frame, spp)
        );
        let pixels = frame.0 as usize * frame.1 as usize;
        assert!(
            (mask.count() as f64) < pixels as f64 * 1e-4,
            "{} of {pixels} pixels flagged",
            mask.count()
        );
    }

    #[test]
    fn clean_fixture_has_no_defects() {
        let mask = fixture_mask(
            "mantis-512",
            crate::project::HOT_PIXEL_THRESHOLD_DEFAULT,
            48,
        );
        assert_eq!(mask.count(), 0, "{:?}", mask.positions());
    }
}
