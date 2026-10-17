// SPDX-License-Identifier: GPL-3.0-or-later

//! Bounded, full-source reductions for source-image views.

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
    outcome: Arc<(Mutex<Option<PreviewOutcome>>, Condvar)>,
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
        let outcome = Arc::new((Mutex::new(None), Condvar::new()));
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
        let (outcome, outcome_wake) = &*self.outcome;
        outcome.lock().expect("preview outcome lock").take();
        self.outcome_bytes.store(0, Ordering::Release);
        outcome_wake.notify_all();
        let (state, wake) = &*self.mailbox;
        *state.lock().expect("preview mailbox lock") = WorkerCommand::Set {
            requests,
            budget,
            cancel,
        };
        wake.notify_one();
    }

    pub fn drain(&self) -> Vec<PreviewOutcome> {
        let (outcome, wake) = &*self.outcome;
        let result = outcome
            .lock()
            .expect("preview outcome lock")
            .take()
            .into_iter()
            .collect();
        self.outcome_bytes.store(0, Ordering::Release);
        wake.notify_one();
        result
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
        let (slot, wake) = &*self.outcome;
        slot.lock().expect("preview outcome lock").take();
        self.outcome_bytes.store(0, Ordering::Release);
        wake.notify_all();
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
        let (outcome, outcome_wake) = &*self.outcome;
        outcome.lock().expect("preview outcome lock").take();
        self.outcome_bytes.store(0, Ordering::Release);
        outcome_wake.notify_all();
    }
}

fn worker_loop(
    mailbox: Arc<(Mutex<WorkerCommand>, Condvar)>,
    outcome: Arc<(Mutex<Option<PreviewOutcome>>, Condvar)>,
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
                mut requests,
                budget,
                cancel,
            } => {
                if let Some(request) = requests.pop()
                    && !cancel.load(Ordering::Acquire)
                {
                    let rendered = renderer(&request.path, budget, &cancel);
                    if !cancel.load(Ordering::Acquire) {
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
                                "source preview render reservation"
                            );
                        }
                        let reservation =
                            rendered.as_ref().ok().map(|(_, reservation)| *reservation);
                        let result = rendered.map(|(pixels, _)| Arc::new(pixels));
                        let (slot, wake) = &*outcome;
                        let mut slot = slot.lock().expect("preview outcome lock");
                        while slot.is_some() && !cancel.load(Ordering::Acquire) {
                            slot = wake.wait(slot).expect("preview outcome lock");
                        }
                        if !cancel.load(Ordering::Acquire) {
                            let bytes = reservation.map_or(0, |reservation| reservation.total);
                            outcome_bytes.store(bytes, Ordering::Release);
                            peak_outcome_bytes.fetch_max(bytes, Ordering::AcqRel);
                            *slot = Some(PreviewOutcome {
                                request,
                                result,
                                reservation,
                            });
                        }
                    }
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
    let bytes_per_row = u64::from(frame.width)
        .saturating_mul(u64::from(spp))
        .saturating_mul(2);
    let mut band_rows = 64u32.min(frame.height.max(1));
    let reservation = loop {
        let band = bytes_per_row.saturating_mul(u64::from(band_rows));
        let read_scratch = reader.read_scratch_bytes(frame.width, band_rows);
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
            };
        }
        if band_rows == 1 {
            return Err(PreviewError::OverBudget);
        }
        band_rows = (band_rows / 2).max(1);
    };

    let mut reducer =
        Downsampler::new_full_source(frame.width, frame.height, spp, target_width, target_height);
    let mut band = vec![0u16; (bytes_per_row / 2 * u64::from(reservation.band_rows)) as usize];
    let mut y = 0;
    while y < frame.height {
        if cancelled.load(Ordering::Acquire) {
            return Err(PreviewError::Cancelled);
        }
        let rows = reservation.band_rows.min(frame.height - y);
        let rect = Rect {
            x: 0,
            y,
            width: frame.width,
            height: rows,
        };
        let samples = rect.width as usize * rect.height as usize * spp as usize;
        reader
            .read_tile_into(0, rect, &mut band[..samples])
            .map_err(|error| PreviewError::Read(error.to_string()))?;
        reducer.add_band(&band[..samples], rect);
        y += rows;
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
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, mpsc};

    use focusforge_core::tile::HostBudget;

    use super::{
        PreviewError, PreviewPixels, PreviewRequest, PreviewReservation, PreviewWorker,
        target_dimensions, tonemap_source,
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
    fn dropping_a_worker_wakes_a_full_outcome_slot_and_releases_its_thread() {
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

        exited_rx
            .recv()
            .expect("drop wakes the worker from its full outcome slot");
    }
}
