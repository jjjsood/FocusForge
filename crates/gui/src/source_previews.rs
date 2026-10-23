// SPDX-License-Identifier: GPL-3.0-or-later

//! Bounded, full-source reductions for source-image views.

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use focusforge_core::display::{self, Downsampler};
use focusforge_core::io::open_frame_set;
use focusforge_core::tile::{HostBudget, Rect};

/// No source preview exceeds this edge.  The UI aspect-fits the resulting
/// image, so preserving both source edges matters more than filling a card.
pub const MAX_EDGE: u32 = 256;
/// Cached preview pixels, including resident and conversion buffers, never
/// exceed this independent cap. Decoder and worker scratch reserve against
/// the caller's host allowance separately.
pub const MAX_CACHE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewPixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A source identity never uses its transient virtual-row index. A tab id
/// plus content generation rejects late work after a replace or tab switch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PreviewRequest {
    pub tab_id: u64,
    pub generation: u64,
    pub path: std::path::PathBuf,
}

#[derive(Debug, Clone)]
pub struct PreviewOutcome {
    pub request: PreviewRequest,
    pub result: Result<Arc<PreviewPixels>, PreviewError>,
    pub reservation: Option<PreviewReservation>,
}

enum WorkerCommand {
    Idle,
    Set {
        requests: Vec<PreviewRequest>,
        budget: HostBudget,
        cancel: Arc<AtomicBool>,
    },
    Shutdown,
}

type PreviewRenderer = dyn Fn(&Path, HostBudget, &AtomicBool) -> Result<(PreviewPixels, PreviewReservation), PreviewError>
    + Send
    + Sync;
type WorkerExit = dyn Fn() + Send + Sync;

/// One persistent decoder worker. One admitted worker stays within the
/// product ceiling of two and makes cancellation non-blocking for the UI.
pub struct PreviewWorker {
    mailbox: Arc<(Mutex<WorkerCommand>, Condvar)>,
    current_cancel: Mutex<Arc<AtomicBool>>,
    outcome: Arc<Mutex<Vec<PreviewOutcome>>>,
    active: Arc<(Mutex<bool>, Condvar)>,
    peak_workers: Arc<AtomicU64>,
    outcome_bytes: Arc<AtomicU64>,
    peak_outcome_bytes: Arc<AtomicU64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewMetrics {
    pub active_workers: u64,
    pub peak_workers: u64,
    pub outcome_bytes: u64,
    pub peak_outcome_bytes: u64,
}

/// A background-only wait for a cancelled decoder to leave its bounded read.
pub struct PreviewQuiescence(Arc<(Mutex<bool>, Condvar)>);

impl PreviewQuiescence {
    pub fn wait(self) {
        let (active, wake) = &*self.0;
        let mut active = active.lock().expect("preview activity lock");
        while *active {
            active = wake.wait(active).expect("preview activity lock");
        }
    }
}

impl PreviewWorker {
    pub fn new() -> Self {
        Self::with_renderer(Arc::new(render_cancellable), None)
    }

    fn with_renderer(renderer: Arc<PreviewRenderer>, on_exit: Option<Arc<WorkerExit>>) -> Self {
        let mailbox = Arc::new((Mutex::new(WorkerCommand::Idle), Condvar::new()));
        let current_cancel = Mutex::new(Arc::new(AtomicBool::new(false)));
        let outcome = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new((Mutex::new(false), Condvar::new()));
        let peak_workers = Arc::new(AtomicU64::new(0));
        let outcome_bytes = Arc::new(AtomicU64::new(0));
        let peak_outcome_bytes = Arc::new(AtomicU64::new(0));
        std::thread::spawn({
            let mailbox = Arc::clone(&mailbox);
            let outcome = Arc::clone(&outcome);
            let active = Arc::clone(&active);
            let peak_workers = Arc::clone(&peak_workers);
            let outcome_bytes = Arc::clone(&outcome_bytes);
            let peak_outcome_bytes = Arc::clone(&peak_outcome_bytes);
            move || {
                worker_loop(
                    mailbox,
                    outcome,
                    active,
                    peak_workers,
                    outcome_bytes,
                    peak_outcome_bytes,
                    renderer,
                );
                if let Some(on_exit) = on_exit {
                    on_exit();
                }
            }
        });
        Self {
            mailbox,
            current_cancel,
            outcome,
            active,
            peak_workers,
            outcome_bytes,
            peak_outcome_bytes,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_renderer(renderer: Arc<PreviewRenderer>) -> Self {
        Self::with_renderer(renderer, None)
    }

    #[cfg(test)]
    fn new_with_exit_probe(renderer: Arc<PreviewRenderer>, on_exit: Arc<WorkerExit>) -> Self {
        Self::with_renderer(renderer, Some(on_exit))
    }

    /// Replaces queued work with current visible identities. This call does
    /// not open a reader or admit memory.
    pub fn set_visible(&self, requests: Vec<PreviewRequest>, budget: HostBudget) {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut previous = self
            .current_cancel
            .lock()
            .expect("preview cancellation lock");
        previous.store(true, Ordering::Release);
        *previous = Arc::clone(&cancel);
        clear_outcomes(&self.outcome, &self.outcome_bytes);
        let (state, wake) = &*self.mailbox;
        *state.lock().expect("preview mailbox lock") = WorkerCommand::Set {
            requests,
            budget,
            cancel,
        };
        wake.notify_one();
    }

    /// Every outcome parked since the last drain, in render order.
    pub fn drain(&self) -> Vec<PreviewOutcome> {
        let mut parked = self.outcome.lock().expect("preview outcome lock");
        self.outcome_bytes.store(0, Ordering::Release);
        std::mem::take(&mut *parked)
    }

    pub fn metrics(&self) -> PreviewMetrics {
        PreviewMetrics {
            active_workers: u64::from(*self.active.0.lock().expect("preview activity lock")),
            peak_workers: self.peak_workers.load(Ordering::Acquire),
            outcome_bytes: self.outcome_bytes.load(Ordering::Acquire),
            peak_outcome_bytes: self.peak_outcome_bytes.load(Ordering::Acquire),
        }
    }

    /// Cancels current work and drops any unconsumed result before another
    /// background job begins. The returned wait object is never used on UI.
    pub fn quiesce(&self) -> PreviewQuiescence {
        self.current_cancel
            .lock()
            .expect("preview cancellation lock")
            .store(true, Ordering::Release);
        let (state, mailbox_wake) = &*self.mailbox;
        *state.lock().expect("preview mailbox lock") = WorkerCommand::Idle;
        mailbox_wake.notify_one();
        clear_outcomes(&self.outcome, &self.outcome_bytes);
        PreviewQuiescence(Arc::clone(&self.active))
    }
}

impl Default for PreviewWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PreviewWorker {
    fn drop(&mut self) {
        self.current_cancel
            .lock()
            .expect("preview cancellation lock")
            .store(true, Ordering::Release);
        let (state, wake) = &*self.mailbox;
        *state.lock().expect("preview mailbox lock") = WorkerCommand::Shutdown;
        wake.notify_one();
        clear_outcomes(&self.outcome, &self.outcome_bytes);
    }
}

/// Drops every parked outcome and its byte count under the outcome lock, the
/// lock the worker holds while it publishes.
fn clear_outcomes(outcome: &Mutex<Vec<PreviewOutcome>>, outcome_bytes: &AtomicU64) {
    let mut parked = outcome.lock().expect("preview outcome lock");
    parked.clear();
    outcome_bytes.store(0, Ordering::Release);
}

fn worker_loop(
    mailbox: Arc<(Mutex<WorkerCommand>, Condvar)>,
    outcome: Arc<Mutex<Vec<PreviewOutcome>>>,
    active: Arc<(Mutex<bool>, Condvar)>,
    peak_workers: Arc<AtomicU64>,
    outcome_bytes: Arc<AtomicU64>,
    peak_outcome_bytes: Arc<AtomicU64>,
    renderer: Arc<PreviewRenderer>,
) {
    loop {
        let command = {
            let (state, wake) = &*mailbox;
            let mut state = state.lock().expect("preview mailbox lock");
            while matches!(&*state, WorkerCommand::Idle) {
                state = wake.wait(state).expect("preview mailbox lock");
            }
            let command = std::mem::replace(&mut *state, WorkerCommand::Idle);
            if matches!(&command, WorkerCommand::Set { requests, .. } if !requests.is_empty()) {
                *active.0.lock().expect("preview activity lock") = true;
                peak_workers.fetch_max(1, Ordering::AcqRel);
            }
            command
        };
        match command {
            WorkerCommand::Shutdown => {
                *active.0.lock().expect("preview activity lock") = false;
                active.1.notify_all();
                break;
            }
            WorkerCommand::Idle => continue,
            WorkerCommand::Set {
                requests,
                budget,
                cancel,
            } => {
                for request in requests {
                    if cancel.load(Ordering::Acquire) {
                        break;
                    }
                    // Parked outcomes still count against the allowance.
                    let admitted = HostBudget::explicit(
                        budget
                            .bytes()
                            .saturating_sub(outcome_bytes.load(Ordering::Acquire)),
                    );
                    let rendered = renderer(&request.path, admitted, &cancel);
                    if let Ok((_, reservation)) = &rendered {
                        tracing::debug!(
                            path = %request.path.display(),
                            configured_cap = budget.bytes(),
                            decoder_cache = reservation.decoder_cache,
                            read_scratch = reservation.read_scratch,
                            band = reservation.band,
                            accumulator = reservation.accumulator,
                            reduced_samples = reservation.reduced_samples,
                            result = reservation.result,
                            resident_and_conversion = reservation.resident_and_conversion,
                            total = reservation.total,
                            step = reservation.step,
                            "source preview render reservation"
                        );
                    }
                    let reservation = rendered.as_ref().ok().map(|(_, reservation)| *reservation);
                    let result = rendered.map(|(pixels, _)| Arc::new(pixels));
                    let mut parked = outcome.lock().expect("preview outcome lock");
                    if cancel.load(Ordering::Acquire) {
                        break;
                    }
                    let bytes = reservation.map_or(0, |reservation| reservation.total);
                    let total = outcome_bytes.fetch_add(bytes, Ordering::AcqRel) + bytes;
                    peak_outcome_bytes.fetch_max(total, Ordering::AcqRel);
                    parked.push(PreviewOutcome {
                        request,
                        result,
                        reservation,
                    });
                }
                *active.0.lock().expect("preview activity lock") = false;
                active.1.notify_all();
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewReservation {
    pub decoder_cache: u64,
    pub read_scratch: u64,
    pub band: u64,
    pub accumulator: u64,
    pub reduced_samples: u64,
    pub result: u64,
    pub resident_and_conversion: u64,
    pub total: u64,
    pub band_rows: u32,
    /// Row and column stride of the sampled grid read from frame 0.
    pub step: u32,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum PreviewError {
    #[error("source preview exceeds its available memory allowance")]
    OverBudget,
    #[error("source preview exceeds its cache allowance")]
    CacheLimit,
    #[error("source preview has unsupported samples-per-pixel {0}")]
    UnsupportedSamples(u32),
    #[error("source preview was cancelled")]
    Cancelled,
    #[error("source preview could not read {0}")]
    Read(String),
}

fn target_dimensions(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height).max(1);
    let scale = (MAX_EDGE as f64 / f64::from(longest)).min(1.0);
    (
        (f64::from(width) * scale).round().max(1.0) as u32,
        (f64::from(height) * scale).round().max(1.0) as u32,
    )
}

fn tonemap_source(samples: &[u16], width: u32, height: u32, spp: u32, rgba: &mut [u8]) {
    if spp != 1 {
        display::tonemap_cpu(samples, width, height, spp, 0, rgba);
        return;
    }
    for (index, pixel) in rgba.chunks_exact_mut(4).enumerate() {
        let x = index as u32 % width;
        let y = index as u32 / width;
        let value = display::reduce16to8(samples[index], x, y);
        pixel.copy_from_slice(&[value, value, value, 255]);
    }
}

/// The largest stride whose sampled grid is still at least `target_width x
/// target_height`, so the final reduction averages real samples in every cell.
fn sample_step(width: u32, height: u32, target_width: u32, target_height: u32) -> u32 {
    let along = |length: u32, target: u32| {
        if target <= 1 {
            length
        } else {
            (length - 1) / (target - 1)
        }
    };
    along(width, target_width)
        .min(along(height, target_height))
        .max(1)
}

/// Renders one original source on the caller's worker thread.  This function
/// opens and reads only here; callers on the UI thread only enqueue identities.
pub fn render(
    path: &Path,
    host_budget: HostBudget,
) -> Result<(PreviewPixels, PreviewReservation), PreviewError> {
    render_cancellable(path, host_budget, &AtomicBool::new(false))
}

/// [`render`]'s cancellation-aware worker entry point.  Cancellation is
/// checked between bounded bands, never by a UI caller.
pub fn render_cancellable(
    path: &Path,
    host_budget: HostBudget,
    cancelled: &AtomicBool,
) -> Result<(PreviewPixels, PreviewReservation), PreviewError> {
    render_with_max_step(path, host_budget, cancelled, u32::MAX)
}

fn render_with_max_step(
    path: &Path,
    host_budget: HostBudget,
    cancelled: &AtomicBool,
    max_step: u32,
) -> Result<(PreviewPixels, PreviewReservation), PreviewError> {
    let cap = host_budget.bytes();
    let frame_set = open_frame_set(&[path.to_path_buf()], HostBudget::explicit(cap))
        .map_err(|error| PreviewError::Read(error.to_string()))?;
    let kind = frame_set.kind;
    let reader = frame_set.reader;
    let frame = reader.frame(0);
    let spp = frame.samples_per_pixel;
    if !matches!(spp, 1 | 3) {
        return Err(PreviewError::UnsupportedSamples(spp));
    }
    let (target_width, target_height) = target_dimensions(frame.width, frame.height);
    let target_cells = u64::from(target_width) * u64::from(target_height);
    let accumulator = target_cells
        .saturating_mul(u64::from(spp))
        .saturating_mul(8)
        .saturating_add(target_cells.saturating_mul(4));
    let reduced_samples = target_cells
        .saturating_mul(u64::from(spp))
        .saturating_mul(2);
    let result = target_cells.saturating_mul(4);
    // One worker owns the raw result while the UI conversion and resident
    // image coexist, so reserve all three before decoding.
    let resident_and_conversion = result.saturating_mul(2);
    if result.saturating_add(resident_and_conversion) > MAX_CACHE_BYTES {
        return Err(PreviewError::CacheLimit);
    }
    let decoder_cache = match kind {
        focusforge_core::io::ReaderKind::Mmap => 0,
        focusforge_core::io::ReaderKind::Decoded | focusforge_core::io::ReaderKind::Raw => cap / 4,
    };
    let base = decoder_cache
        .saturating_add(accumulator)
        .saturating_add(reduced_samples)
        .saturating_add(result)
        .saturating_add(resident_and_conversion);
    let step =
        sample_step(frame.width, frame.height, target_width, target_height).min(max_step.max(1));
    let nz_step = NonZeroU32::new(step).expect("the sampling step is at least 1");
    let grid_width = frame.width.div_ceil(step);
    let grid_height = frame.height.div_ceil(step);
    let bytes_per_row = u64::from(grid_width)
        .saturating_mul(u64::from(spp))
        .saturating_mul(2);
    // A stepped read of a row-at-a-time reader holds one full-width source row.
    let step_row = if step > 1 {
        u64::from(frame.width)
            .saturating_mul(u64::from(spp))
            .saturating_mul(2)
    } else {
        0
    };
    let mut band_rows = 64u32.min(grid_height.max(1));
    let reservation = loop {
        let band = bytes_per_row.saturating_mul(u64::from(band_rows));
        let read_scratch = if step > 1 {
            reader
                .read_scratch_bytes(frame.width, 1)
                .saturating_add(step_row)
        } else {
            reader.read_scratch_bytes(frame.width, band_rows)
        };
        let total = base.saturating_add(band).saturating_add(read_scratch);
        if total <= cap {
            break PreviewReservation {
                decoder_cache,
                read_scratch,
                band,
                accumulator,
                reduced_samples,
                result,
                resident_and_conversion,
                total,
                band_rows,
                step,
            };
        }
        if band_rows == 1 {
            return Err(PreviewError::OverBudget);
        }
        band_rows = (band_rows / 2).max(1);
    };

    let mut reducer =
        Downsampler::new_full_source(grid_width, grid_height, spp, target_width, target_height);
    let mut band = vec![0u16; (bytes_per_row / 2 * u64::from(reservation.band_rows)) as usize];
    let mut grid_row = 0;
    while grid_row < grid_height {
        if cancelled.load(Ordering::Acquire) {
            return Err(PreviewError::Cancelled);
        }
        let rows = reservation.band_rows.min(grid_height - grid_row);
        let source_y = grid_row * step;
        let source = Rect {
            x: 0,
            y: source_y,
            width: frame.width,
            height: (rows * step).min(frame.height - source_y),
        };
        let grid = Rect {
            x: 0,
            y: grid_row,
            width: grid_width,
            height: rows,
        };
        let samples = grid.width as usize * grid.height as usize * spp as usize;
        reader
            .read_tile_step_into(0, source, nz_step, spp, &mut band[..samples])
            .map_err(|error| PreviewError::Read(error.to_string()))?;
        reducer.add_band(&band[..samples], grid);
        grid_row += rows;
    }
    let samples = reducer.finish();
    let mut rgba = vec![0; result as usize];
    tonemap_source(&samples, target_width, target_height, spp, &mut rgba);
    Ok((
        PreviewPixels {
            width: target_width,
            height: target_height,
            rgba,
        },
        reservation,
    ))
}

#[cfg(test)]
#[path = "../../core/tests/common/raw_dng.rs"]
mod raw_dng_fixture;

#[cfg(test)]
#[path = "../../core/tests/common/reference_stack.rs"]
mod reference_stack_fixture;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};

    use focusforge_core::tile::HostBudget;

    use super::{
        PreviewError, PreviewOutcome, PreviewPixels, PreviewRequest, PreviewReservation,
        PreviewWorker, target_dimensions, tonemap_source,
    };

    fn request(name: &str) -> PreviewRequest {
        PreviewRequest {
            tab_id: 1,
            generation: 1,
            path: PathBuf::from(name),
        }
    }

    fn reservation() -> PreviewReservation {
        PreviewReservation {
            decoder_cache: 0,
            read_scratch: 0,
            band: 0,
            accumulator: 0,
            reduced_samples: 0,
            result: 4,
            resident_and_conversion: 8,
            total: 12,
            band_rows: 1,
            step: 1,
        }
    }

    #[test]
    fn target_dimensions_keep_original_aspect_under_the_edge_limit() {
        assert_eq!(target_dimensions(800, 200), (256, 64));
        assert_eq!(target_dimensions(200, 800), (64, 256));
        assert_eq!(target_dimensions(1, 1), (1, 1));
    }

    #[test]
    fn mono_sources_use_neutral_display_values_not_zmap_sentinel_handling() {
        let mut rgba = [0u8; 12];
        tonemap_source(&[0, u16::MAX, 32_768], 3, 1, 1, &mut rgba);
        assert_eq!(&rgba[0..4], &[0, 0, 0, 255]);
        assert_eq!(&rgba[4..8], &[255, 255, 255, 255]);
        assert!(rgba[8] > 0 && rgba[8] < 255);
        assert_eq!(rgba[8], rgba[9]);
        assert_eq!(rgba[9], rgba[10]);
    }

    #[test]
    fn real_tiff_previews_keep_full_edges_for_both_readers_and_small_sources() {
        use focusforge_core::io::{
            ReaderKind, StripCompression, StripSpec, StripWriter, open_frame_set,
        };

        let directory = tempfile::tempdir().unwrap();
        let cap = 32 * 1024 * 1024;
        for (width, height, spp, expected_size) in [
            (1u32, 1u32, 1u32, (1, 1)),
            (3, 2, 3, (3, 2)),
            (513, 127, 3, (256, 63)),
            (127, 513, 1, (63, 256)),
        ] {
            let mut bytes = Vec::new();
            for y in 0..height {
                for x in 0..width {
                    let edge = if width > height {
                        x < 4 || x >= width.saturating_sub(4)
                    } else {
                        y < 4 || y >= height.saturating_sub(4)
                    };
                    for channel in 0..spp {
                        let value = if edge && (spp == 1 || channel == 1) {
                            u16::MAX
                        } else {
                            0
                        };
                        bytes.extend_from_slice(&value.to_le_bytes());
                    }
                }
            }
            let mut previous = None;
            for (compression, kind) in [
                (StripCompression::None, ReaderKind::Mmap),
                (StripCompression::Deflate, ReaderKind::Decoded),
            ] {
                let path = directory.path().join("source.tif");
                let mut writer = StripWriter::create(
                    &path,
                    StripSpec {
                        width,
                        height,
                        samples_per_pixel: spp,
                        bits_per_sample: 16,
                        compression,
                        description: None,
                        metadata: Default::default(),
                    },
                )
                .unwrap();
                writer.write_rows(&bytes).unwrap();
                writer.finish().unwrap();
                let original = std::fs::read(&path).unwrap();
                assert_eq!(
                    open_frame_set(std::slice::from_ref(&path), HostBudget::explicit(cap))
                        .unwrap()
                        .kind,
                    kind
                );
                let (preview, reservation) = super::render(&path, HostBudget::explicit(cap))
                    .expect("supported original TIFF renders within its host allowance");
                assert_eq!((preview.width, preview.height), expected_size);
                let edge_pixel = if spp == 1 {
                    [255, 255, 255, 255]
                } else {
                    [0, 255, 0, 255]
                };
                assert_eq!(&preview.rgba[..4], &edge_pixel);
                assert_eq!(&preview.rgba[preview.rgba.len() - 4..], &edge_pixel);
                if width > 8 || height > 8 {
                    let center =
                        ((preview.height / 2 * preview.width + preview.width / 2) * 4) as usize;
                    assert_eq!(&preview.rgba[center..center + 4], &[0, 0, 0, 255]);
                }
                assert!(reservation.total <= cap);
                assert_eq!(reservation.decoder_cache == 0, kind == ReaderKind::Mmap);
                if width == 513 && kind == ReaderKind::Mmap {
                    let (smaller_band, smaller_reservation) =
                        super::render(&path, HostBudget::explicit(800_000)).unwrap();
                    assert!(smaller_reservation.band_rows < reservation.band_rows);
                    assert!(smaller_reservation.total <= 800_000);
                    assert_eq!(smaller_band, preview);
                    assert!(matches!(
                        super::render(&path, HostBudget::explicit(1)),
                        Err(PreviewError::OverBudget)
                    ));
                }
                assert_eq!(std::fs::read(&path).unwrap(), original);
                if let Some(previous) = previous.replace(preview.clone()) {
                    assert_eq!(
                        preview, previous,
                        "compression must not change preview pixels"
                    );
                }
            }
        }
    }

    fn write_gradient_tiff(
        path: &Path,
        width: u32,
        height: u32,
        spp: u32,
        compression: focusforge_core::io::StripCompression,
    ) {
        let mut bytes = Vec::new();
        for y in 0..height {
            for x in 0..width {
                for channel in 0..spp {
                    let value = ((x * 7 + y * 13 + channel * 1000) % 60_000) as u16;
                    bytes.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
        write_tiff(path, width, height, spp, compression, &bytes);
    }

    fn write_tiff(
        path: &Path,
        width: u32,
        height: u32,
        spp: u32,
        compression: focusforge_core::io::StripCompression,
        bytes: &[u8],
    ) {
        use focusforge_core::io::{StripSpec, StripWriter};

        let mut writer = StripWriter::create(
            path,
            StripSpec {
                width,
                height,
                samples_per_pixel: spp,
                bits_per_sample: 16,
                compression,
                description: None,
                metadata: Default::default(),
            },
        )
        .unwrap();
        writer.write_rows(bytes).unwrap();
        writer.finish().unwrap();
    }

    #[test]
    fn stepped_preview_follows_the_target_in_both_axes() {
        use focusforge_core::io::{ReaderKind, StripCompression, open_frame_set};

        let directory = tempfile::tempdir().unwrap();
        let cap = 32 * 1024 * 1024;
        // (width, height, step, grid width, grid height)
        for (width, height, step, grid_width, grid_height) in [
            (1000u32, 37u32, 3u32, 334u32, 13u32),
            (37, 1000, 3, 13, 334),
            (200, 120, 1, 200, 120),
        ] {
            for spp in [1u32, 3] {
                for (compression, kind) in [
                    (StripCompression::None, ReaderKind::Mmap),
                    (StripCompression::Deflate, ReaderKind::Decoded),
                ] {
                    let path = directory.path().join("source.tif");
                    write_gradient_tiff(&path, width, height, spp, compression);
                    assert_eq!(
                        open_frame_set(std::slice::from_ref(&path), HostBudget::explicit(cap))
                            .unwrap()
                            .kind,
                        kind
                    );
                    let label = format!("{width}x{height} spp {spp} {kind:?}");
                    let (preview, reservation) =
                        super::render(&path, HostBudget::explicit(cap)).expect(&label);
                    assert_eq!(
                        (preview.width, preview.height),
                        target_dimensions(width, height),
                        "{label}"
                    );
                    assert_eq!(reservation.step, step, "{label}");
                    assert_eq!(
                        reservation.band,
                        u64::from(grid_width)
                            * u64::from(spp)
                            * 2
                            * u64::from(reservation.band_rows),
                        "{label}: the band is sized from the sampled grid width"
                    );
                    assert_eq!(reservation.band_rows, 64.min(grid_height), "{label}");
                    assert!(reservation.total <= cap, "{label}");
                    assert!(
                        preview.rgba.chunks_exact(4).all(|pixel| pixel[3] == 255),
                        "{label}"
                    );
                }
            }
        }
    }

    #[test]
    fn stepped_preview_keeps_a_flat_source_flat() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("flat.tif");
        let (width, height) = (1500u32, 900u32);
        let bytes: Vec<u8> =
            std::iter::repeat_n(40_000u16.to_le_bytes(), (width * height * 3) as usize)
                .flatten()
                .collect();
        write_tiff(
            &path,
            width,
            height,
            3,
            focusforge_core::io::StripCompression::None,
            &bytes,
        );

        let cap = 32 * 1024 * 1024;
        let (stepped, stepped_reservation) =
            super::render(&path, HostBudget::explicit(cap)).unwrap();
        let (full, full_reservation) = super::render_with_max_step(
            &path,
            HostBudget::explicit(cap),
            &AtomicBool::new(false),
            1,
        )
        .unwrap();
        assert!(stepped_reservation.step > 1);
        assert_eq!(full_reservation.step, 1);
        assert_eq!(
            stepped, full,
            "a flat source reduces to the same pixels at any step"
        );
    }

    /// Per-thumbnail time of the full read (stride 1) against the stepped read on
    /// one reference-stack frame: `FOCUSFORGE_REFERENCE_STACK=<subject dir>`.
    #[test]
    #[ignore = "reads a reference-stack frame; set FOCUSFORGE_REFERENCE_STACK"]
    fn reference_thumbnail_timing() {
        let Some(directory) = super::reference_stack_fixture::reference_stack_dir() else {
            return;
        };
        let mut frames: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "tif"))
            .collect();
        frames.sort();
        let frame = frames
            .first()
            .expect("the reference directory holds a .tif");
        let budget = HostBudget::explicit(512 * 1024 * 1024);
        let time = |max_step: u32| {
            let cancelled = AtomicBool::new(false);
            let (_, reservation) =
                super::render_with_max_step(frame, budget, &cancelled, max_step).unwrap();
            let mut runs: Vec<_> = (0..5)
                .map(|_| {
                    let start = std::time::Instant::now();
                    super::render_with_max_step(frame, budget, &cancelled, max_step).unwrap();
                    start.elapsed()
                })
                .collect();
            runs.sort();
            (runs[runs.len() / 2], reservation)
        };
        let (full, full_reservation) = time(1);
        let (stepped, stepped_reservation) = time(u32::MAX);
        println!(
            "thumbnail of {}: step {} {:.1} ms, step {} {:.1} ms",
            frame.display(),
            full_reservation.step,
            full.as_secs_f64() * 1e3,
            stepped_reservation.step,
            stepped.as_secs_f64() * 1e3,
        );
        assert!(
            stepped < full,
            "the stepped read is faster than the full read"
        );
    }

    #[test]
    fn supported_raw_preview_is_bounded_colored_and_leaves_original_untouched() {
        use super::raw_dng_fixture::{DngSpec, write_dng};

        let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], Vec::new());
        spec.width = 32;
        spec.height = 16;
        spec.samples = (0..16)
            .flat_map(|y| (0..32).map(move |x| [2048, 1024, 1024, 512][(y % 2) * 2 + x % 2]))
            .collect();
        let source = write_dng(&spec, false);
        let original = std::fs::read(source.path()).unwrap();
        let cap = 4 * 1024 * 1024;
        let (preview, reservation) = super::render(source.path(), HostBudget::explicit(cap))
            .expect("supported Bayer DNG renders with bounded development");
        assert_eq!((preview.width, preview.height), (32, 16));
        assert_eq!(
            preview.rgba.len(),
            (preview.width * preview.height * 4) as usize
        );
        assert!(preview.rgba.chunks_exact(4).all(|pixel| pixel[3] == 255));
        assert!(
            preview
                .rgba
                .chunks_exact(4)
                .any(|pixel| pixel[0] != pixel[1])
        );
        assert!(reservation.decoder_cache > 0);
        assert!(reservation.total <= cap);
        assert_eq!(std::fs::read(source.path()).unwrap(), original);
    }

    fn one_pixel() -> PreviewPixels {
        PreviewPixels {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 255],
        }
    }

    /// Drains until `count` outcomes arrived or ten seconds passed.
    fn collect_outcomes(worker: &PreviewWorker, count: usize) -> Vec<PreviewOutcome> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut outcomes = Vec::new();
        while outcomes.len() < count && std::time::Instant::now() < deadline {
            outcomes.extend(worker.drain());
            std::thread::yield_now();
        }
        outcomes
    }

    fn wait_until_idle(worker: &PreviewWorker) {
        while worker.metrics().active_workers != 0 {
            std::thread::yield_now();
        }
    }

    #[test]
    fn one_request_renders_visible_rows_first() {
        let rendered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let worker = PreviewWorker::new_with_renderer(Arc::new({
            let rendered = Arc::clone(&rendered);
            move |path, _, _| {
                rendered.lock().unwrap().push(path.to_path_buf());
                Ok((one_pixel(), reservation()))
            }
        }));
        let names = [
            "visible-0",
            "visible-1",
            "prefetch-0",
            "prefetch-1",
            "prefetch-2",
        ];
        worker.set_visible(
            names.iter().map(|name| request(name)).collect(),
            HostBudget::explicit(1024),
        );

        let outcomes = collect_outcomes(&worker, names.len());
        assert_eq!(
            outcomes
                .iter()
                .map(|outcome| outcome.request.clone())
                .collect::<Vec<_>>(),
            names.iter().map(|name| request(name)).collect::<Vec<_>>(),
            "one set_visible yields every request, in request order"
        );
        assert_eq!(
            *rendered.lock().unwrap(),
            names.iter().map(PathBuf::from).collect::<Vec<_>>()
        );
        assert_eq!(worker.metrics().peak_workers, 1);
        assert!(worker.metrics().peak_outcome_bytes >= 12);
    }

    #[test]
    fn batch_isolates_failed_rows_and_stops_on_replace() {
        let (slow_started_tx, slow_started_rx) = mpsc::channel();
        let (slow_release_tx, slow_release_rx) = mpsc::channel();
        let slow_release_rx = Arc::new(std::sync::Mutex::new(slow_release_rx));
        let worker = PreviewWorker::new_with_renderer(Arc::new(move |path, _, _| {
            if path == Path::new("bad") {
                return Err(PreviewError::Read("unreadable".to_string()));
            }
            if path == Path::new("slow") {
                slow_started_tx
                    .send(())
                    .expect("test waits for the slow row");
                slow_release_rx
                    .lock()
                    .unwrap()
                    .recv()
                    .expect("test releases the slow row");
            }
            Ok((one_pixel(), reservation()))
        }));

        worker.set_visible(
            ["first", "bad", "third", "fourth"].map(request).to_vec(),
            HostBudget::explicit(1024),
        );
        let outcomes = collect_outcomes(&worker, 4);
        assert_eq!(outcomes.len(), 4, "a failed row does not end the batch");
        assert!(matches!(outcomes[1].result, Err(PreviewError::Read(_))));
        assert!(outcomes[1].reservation.is_none());
        for index in [0, 2, 3] {
            assert!(outcomes[index].result.is_ok());
        }

        worker.set_visible(
            ["parked", "slow", "stale-a", "stale-b"]
                .map(request)
                .to_vec(),
            HostBudget::explicit(1024),
        );
        slow_started_rx.recv().expect("the second row is rendering");
        while worker.metrics().outcome_bytes == 0 {
            std::thread::yield_now();
        }
        worker.set_visible(
            ["fresh-a", "fresh-b"].map(request).to_vec(),
            HostBudget::explicit(1024),
        );
        assert_eq!(
            worker.metrics().outcome_bytes,
            0,
            "the old set's parked row is dropped by the replacement"
        );
        slow_release_tx.send(()).expect("release the slow row");

        let outcomes = collect_outcomes(&worker, 2);
        wait_until_idle(&worker);
        let mut requests: Vec<_> = outcomes
            .iter()
            .map(|outcome| outcome.request.clone())
            .collect();
        requests.extend(worker.drain().into_iter().map(|outcome| outcome.request));
        assert_eq!(requests, ["fresh-a", "fresh-b"].map(request).to_vec());
    }

    #[test]
    fn quiesce_waits_for_an_already_claimed_render_without_leaving_active_stuck() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let worker = PreviewWorker::new_with_renderer(Arc::new(move |_, _, cancelled| {
            started_tx
                .send(())
                .expect("the test waits for renderer entry");
            release_rx
                .lock()
                .expect("renderer release lock")
                .recv()
                .expect("the test releases renderer");
            if cancelled.load(Ordering::Acquire) {
                return Err(PreviewError::Cancelled);
            }
            Ok((
                PreviewPixels {
                    width: 1,
                    height: 1,
                    rgba: vec![0, 0, 0, 255],
                },
                reservation(),
            ))
        }));
        worker.set_visible(vec![request("claimed")], HostBudget::explicit(1024));
        started_rx
            .recv()
            .expect("render was claimed before quiesce");
        assert_eq!(worker.metrics().active_workers, 1);
        assert_eq!(worker.metrics().peak_workers, 1);

        let barrier = worker.quiesce();
        let (released_tx, released_rx) = mpsc::channel();
        std::thread::spawn(move || {
            barrier.wait();
            released_tx
                .send(())
                .expect("quiesce completion reaches test");
        });
        assert!(
            released_rx.try_recv().is_err(),
            "render remains active until it observes cancellation"
        );
        release_tx.send(()).expect("renderer release sends");
        released_rx
            .recv()
            .expect("quiesce completes after renderer releases");
        assert_eq!(worker.metrics().outcome_bytes, 0);
    }

    #[test]
    fn replacing_a_visible_set_drops_the_parked_result_before_new_decode() {
        let (first_published_tx, first_published_rx) = mpsc::channel();
        let (second_started_tx, second_started_rx) = mpsc::channel();
        let (second_release_tx, second_release_rx) = mpsc::channel();
        let second_release_rx = Arc::new(std::sync::Mutex::new(second_release_rx));
        let worker = PreviewWorker::new_with_renderer(Arc::new(move |path, _, _| {
            if path == Path::new("first") {
                first_published_tx.send(()).expect("first renderer entered");
            } else {
                second_started_tx.send(()).expect("second renderer entered");
                second_release_rx
                    .lock()
                    .expect("second renderer release lock")
                    .recv()
                    .expect("test releases second renderer");
            }
            Ok((
                PreviewPixels {
                    width: 1,
                    height: 1,
                    rgba: vec![0, 0, 0, 255],
                },
                reservation(),
            ))
        }));
        worker.set_visible(vec![request("first")], HostBudget::explicit(1024));
        first_published_rx.recv().expect("first render completes");
        while worker.metrics().outcome_bytes == 0 {
            std::thread::yield_now();
        }

        worker.set_visible(vec![request("second")], HostBudget::explicit(1024));
        second_started_rx
            .recv()
            .expect("second decode starts only after replacement");
        assert_eq!(
            worker.metrics().outcome_bytes,
            0,
            "the parked first result was dropped before second admission"
        );
        second_release_tx.send(()).expect("release second renderer");
        while worker.metrics().outcome_bytes == 0 {
            std::thread::yield_now();
        }
        let outcomes = worker.drain();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].request, request("second"));
    }

    #[test]
    fn dropping_a_worker_with_a_parked_outcome_releases_its_thread() {
        let (rendered_tx, rendered_rx) = mpsc::channel();
        let (exited_tx, exited_rx) = mpsc::channel();
        let worker = PreviewWorker::new_with_exit_probe(
            Arc::new(move |_, _, _| {
                rendered_tx.send(()).expect("renderer finishes before drop");
                Ok((
                    PreviewPixels {
                        width: 1,
                        height: 1,
                        rgba: vec![0, 0, 0, 255],
                    },
                    reservation(),
                ))
            }),
            Arc::new(move || exited_tx.send(()).expect("worker exit reaches test")),
        );
        worker.set_visible(vec![request("parked")], HostBudget::explicit(1024));
        rendered_rx.recv().expect("renderer completed");
        while worker.metrics().outcome_bytes == 0 {
            std::thread::yield_now();
        }

        drop(worker);

        exited_rx.recv().expect("drop releases the worker thread");
    }
}
