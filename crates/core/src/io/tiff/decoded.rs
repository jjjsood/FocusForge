// SPDX-License-Identifier: GPL-3.0-or-later

//! A TIFF frame reader backed by a bounded decoded-segment cache.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use memmap2::Mmap;

use super::super::error::TiffError;
use super::super::frame::{FrameDesc, FrameReader};
use super::header::TiffHeader;
use super::segment::{DecodedSegment, SegmentInfo, SegmentOrigin, decode_segment, segment_layout};
use super::stack::{check_uniform, frame_paths, map_file};
use crate::algorithm::{AlgorithmError, TileReader};
use crate::tile::{HostBudget, Rect, Tile};
use crate::timing::{TimingCounter, measurement_scope, record_timing};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
/// Cache counters for decoded TIFF segments.
pub struct CacheStats {
    /// Bytes reserved by resident and loading segments.
    pub resident_bytes: u64,
    /// Greatest observed resident byte count.
    pub peak_resident_bytes: u64,
    /// Number of cache hits.
    pub hits: u64,
    /// Number of cache misses that started a decode.
    pub misses: u64,
    /// Number of decode attempts.
    pub decodes: u64,
    /// Number of resident entries evicted to make room.
    pub evictions: u64,
}

#[derive(Debug)]
struct Frame {
    header: TiffHeader,
    desc: FrameDesc,
    segments: Vec<SegmentInfo>,
    map: OnceLock<Mmap>,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct Key {
    frame: usize,
    segment: usize,
}

#[derive(Debug)]
struct Entry {
    bytes: u64,
    used_at: u64,
    segment: Option<Arc<DecodedSegment>>,
}

#[derive(Debug, Default)]
struct CacheState {
    entries: HashMap<Key, Entry>,
    stats: CacheStats,
    clock: u64,
    #[cfg(test)]
    waiters: usize,
}

#[derive(Debug, Default)]
struct SegmentCache {
    state: Mutex<CacheState>,
    changed: Condvar,
    #[cfg(test)]
    before_wait: Mutex<Option<Arc<std::sync::Barrier>>>,
}

impl SegmentCache {
    fn next_tick(state: &mut CacheState) -> u64 {
        state.clock = state.clock.wrapping_add(1);
        state.clock
    }

    fn evict_one(state: &mut CacheState) -> bool {
        let candidate = state
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry
                    .segment
                    .as_ref()
                    .is_some_and(|segment| Arc::strong_count(segment) == 1)
            })
            .min_by_key(|(_, entry)| entry.used_at)
            .map(|(key, _)| *key);
        let Some(key) = candidate else {
            return false;
        };
        let entry = state
            .entries
            .remove(&key)
            .expect("selected cache entry exists");
        state.stats.resident_bytes -= entry.bytes;
        state.stats.evictions += 1;
        true
    }

    fn get_or_decode(
        &self,
        key: Key,
        bytes: u64,
        capacity: u64,
        decode: impl FnOnce() -> Result<DecodedSegment, TiffError>,
    ) -> Result<Arc<DecodedSegment>, TiffError> {
        let mut decode = Some(decode);
        'retry: loop {
            let mut state = self.state.lock().expect("cache mutex is not poisoned");
            if let Some(segment) = state
                .entries
                .get(&key)
                .and_then(|entry| entry.segment.as_ref().map(Arc::clone))
            {
                let tick = Self::next_tick(&mut state);
                state
                    .entries
                    .get_mut(&key)
                    .expect("resident cache entry exists")
                    .used_at = tick;
                state.stats.hits += 1;
                record_timing(TimingCounter::CacheHits, 1);
                return Ok(segment);
            }
            if state.entries.contains_key(&key) {
                #[cfg(test)]
                {
                    state.waiters += 1;
                }
                state = self
                    .changed
                    .wait(state)
                    .expect("cache mutex is not poisoned");
                #[cfg(test)]
                {
                    state.waiters -= 1;
                }
                drop(state);
                continue 'retry;
            }
            while state.stats.resident_bytes.saturating_add(bytes) > capacity {
                if !Self::evict_one(&mut state) {
                    #[cfg(test)]
                    if let Some(barrier) = self
                        .before_wait
                        .lock()
                        .expect("test hook mutex is not poisoned")
                        .as_ref()
                        .cloned()
                    {
                        barrier.wait();
                    }
                    #[cfg(test)]
                    {
                        state.waiters += 1;
                    }
                    state = self
                        .changed
                        .wait(state)
                        .expect("cache mutex is not poisoned");
                    #[cfg(test)]
                    {
                        state.waiters -= 1;
                    }
                    drop(state);
                    continue 'retry;
                }
            }
            state.stats.resident_bytes += bytes;
            state.stats.peak_resident_bytes = state
                .stats
                .peak_resident_bytes
                .max(state.stats.resident_bytes);
            state.stats.misses += 1;
            state.stats.decodes += 1;
            record_timing(TimingCounter::CacheMisses, 1);
            let tick = Self::next_tick(&mut state);
            state.entries.insert(
                key,
                Entry {
                    bytes,
                    used_at: tick,
                    segment: None,
                },
            );
            drop(state);

            let decoded = decode.take().expect("one decode per cache miss")();
            let mut state = self.state.lock().expect("cache mutex is not poisoned");
            match decoded {
                Ok(decoded) => {
                    record_timing(
                        TimingCounter::DecodedBytes,
                        decoded
                            .samples
                            .len()
                            .saturating_mul(std::mem::size_of::<u16>())
                            as u64,
                    );
                    let segment = Arc::new(decoded);
                    let tick = Self::next_tick(&mut state);
                    let entry = state.entries.get_mut(&key).expect("loading entry exists");
                    entry.used_at = tick;
                    entry.segment = Some(Arc::clone(&segment));
                    self.changed.notify_all();
                    return Ok(segment);
                }
                Err(error) => {
                    let entry = state.entries.remove(&key).expect("loading entry exists");
                    state.stats.resident_bytes -= entry.bytes;
                    self.changed.notify_all();
                    return Err(error);
                }
            }
        }
    }

    fn stats(&self) -> CacheStats {
        self.state
            .lock()
            .expect("cache mutex is not poisoned")
            .stats
    }

    fn release(&self) {
        let _state = self.state.lock().expect("cache mutex is not poisoned");
        self.changed.notify_all();
    }

    #[cfg(test)]
    fn waiter_count(&self) -> usize {
        self.state
            .lock()
            .expect("cache mutex is not poisoned")
            .waiters
    }

    #[cfg(test)]
    fn set_before_wait_barrier(&self, barrier: Arc<std::sync::Barrier>) {
        *self
            .before_wait
            .lock()
            .expect("test hook mutex is not poisoned") = Some(barrier);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    fn segment() -> DecodedSegment {
        DecodedSegment {
            origin: SegmentOrigin::Strip { y: 0 },
            stored_width: 1,
            stored_height: 1,
            valid_width: 1,
            valid_height: 1,
            samples: vec![7],
        }
    }

    #[test]
    fn capacity_waiter_finishes_after_reader_releases_pin() {
        let cache = Arc::new(SegmentCache::default());
        let first = cache
            .get_or_decode(
                Key {
                    frame: 0,
                    segment: 0,
                },
                2,
                2,
                || Ok(segment()),
            )
            .unwrap();
        assert_eq!(cache.stats().resident_bytes, 2);
        let waiting_cache = Arc::clone(&cache);
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let result = waiting_cache.get_or_decode(
                Key {
                    frame: 0,
                    segment: 1,
                },
                2,
                2,
                || Ok(segment()),
            );
            done_tx.send(result.is_ok()).unwrap();
        });
        for _ in 0..10_000 {
            if cache.waiter_count() == 1 {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(
            cache.waiter_count(),
            1,
            "second read must wait for capacity"
        );
        drop(first);
        cache.release();
        assert!(done_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        waiter.join().unwrap();
    }

    #[test]
    fn release_after_capacity_check_wakes_the_waiter() {
        let cache = Arc::new(SegmentCache::default());
        let first = cache
            .get_or_decode(
                Key {
                    frame: 0,
                    segment: 0,
                },
                2,
                2,
                || Ok(segment()),
            )
            .unwrap();
        let checked_capacity = Arc::new(std::sync::Barrier::new(2));
        cache.set_before_wait_barrier(Arc::clone(&checked_capacity));
        let waiting_cache = Arc::clone(&cache);
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let result = waiting_cache.get_or_decode(
                Key {
                    frame: 0,
                    segment: 1,
                },
                2,
                2,
                || Ok(segment()),
            );
            done_tx.send(result.is_ok()).unwrap();
        });
        checked_capacity.wait();
        drop(first);
        cache.release();
        assert!(done_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        waiter.join().unwrap();
    }

    #[test]
    fn failed_load_wakes_same_key_waiter_and_releases_reservation() {
        let cache = Arc::new(SegmentCache::default());
        let (started_tx, started_rx) = mpsc::channel();
        let fail = Arc::new(std::sync::Barrier::new(2));
        let loading_cache = Arc::clone(&cache);
        let loading_fail = Arc::clone(&fail);
        let loader = std::thread::spawn(move || {
            loading_cache.get_or_decode(
                Key {
                    frame: 0,
                    segment: 0,
                },
                2,
                2,
                || {
                    started_tx.send(()).unwrap();
                    loading_fail.wait();
                    Err(TiffError::SegmentStream {
                        path: PathBuf::new(),
                        index: 0,
                    })
                },
            )
        });
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let waiting_cache = Arc::clone(&cache);
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let result = waiting_cache.get_or_decode(
                Key {
                    frame: 0,
                    segment: 0,
                },
                2,
                2,
                || Ok(segment()),
            );
            done_tx.send(result.is_ok()).unwrap();
        });
        for _ in 0..10_000 {
            if cache.waiter_count() == 1 {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(cache.waiter_count(), 1, "same key must wait for the load");
        fail.wait();
        assert!(matches!(
            loader.join().unwrap(),
            Err(TiffError::SegmentStream { .. })
        ));
        assert!(done_rx.recv_timeout(Duration::from_secs(1)).unwrap());
        waiter.join().unwrap();
        assert_eq!(cache.stats().resident_bytes, 2);
    }
}

/// A TIFF frame set that decodes compressed strips and tiles on demand.
#[derive(Debug)]
pub struct DecodedTiffStack {
    frames: Vec<Frame>,
    cache: SegmentCache,
    cache_bytes: u64,
    /// Heap one segment decode allocates: the decoded byte buffer plus the
    /// unconverted sample vector, both sized by the largest stored segment.
    decode_scratch_bytes: u64,
}

impl DecodedTiffStack {
    /// Opens TIFF frames from `dir` with decoded samples bounded by
    /// `cache_bytes`.
    pub fn open(dir: impl AsRef<Path>, cache_bytes: u64) -> Result<Self, TiffError> {
        let paths = frame_paths(dir.as_ref())?;
        Self::open_files(&paths, cache_bytes)
    }

    /// Opens exactly `paths`, retaining their supplied order.
    pub fn open_files(paths: &[PathBuf], cache_bytes: u64) -> Result<Self, TiffError> {
        if paths.is_empty() {
            return Err(TiffError::NoFrames {
                path: PathBuf::new(),
            });
        }
        let mut frames: Vec<Frame> = Vec::with_capacity(paths.len());
        let mut required = None;
        let mut largest_stored = 0u64;
        for path in paths {
            let file = File::open(path).map_err(|source| TiffError::Io {
                path: path.clone(),
                source,
            })?;
            let header = TiffHeader::parse(path, &file)?;
            if let Some(first) = frames.first() {
                check_uniform(&first.header, &header)?;
            }
            let segments = segment_layout(&header)?;
            for segment in &segments {
                largest_stored = largest_stored.max(stored_segment_bytes(&header, *segment)?);
                let bytes = segment_bytes(&header, *segment)?;
                if required
                    .as_ref()
                    .is_none_or(|(_, largest)| bytes > *largest)
                {
                    required = Some((path.clone(), bytes));
                }
            }
            frames.push(Frame {
                desc: FrameDesc::from(&header),
                header,
                segments,
                map: OnceLock::new(),
            });
        }
        let (path, required) = required.expect("nonempty validated TIFF has a segment");
        if cache_bytes < required {
            return Err(TiffError::DecodedCacheTooSmall {
                path,
                bound: cache_bytes,
                required,
            });
        }
        Ok(Self {
            frames,
            cache: SegmentCache::default(),
            cache_bytes,
            decode_scratch_bytes: 2 * largest_stored + 1,
        })
    }

    /// Opens exactly `paths` with the cache share of an already-capped host
    /// budget.
    pub fn open_files_with_budget(
        paths: &[PathBuf],
        budget: HostBudget,
    ) -> Result<Self, TiffError> {
        Self::open_files(paths, budget.partition_for_decoded_cache().cache.bytes())
    }

    /// Number of frames in the opened set.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether the opened set has no frames.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Shared dimensions of every frame.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.frames[0].header.width, self.frames[0].header.height)
    }

    /// Current decoded-segment cache counters.
    pub fn cache_stats(&self) -> CacheStats {
        self.cache.stats()
    }

    /// Reads a rectangle into a new tile allocation.
    pub fn read_tile(&self, frame: usize, rect: Rect) -> Result<Tile, TiffError> {
        let header = self.frame(frame)?;
        let len = expected_len(rect, header.header.samples_per_pixel)?;
        let mut samples = vec![0; len];
        self.read_tile_into(frame, rect, &mut samples)?;
        Ok(Tile {
            rect,
            samples_per_pixel: header.header.samples_per_pixel,
            samples,
        })
    }

    /// Reads `rect` into `out` without allocating a whole-frame buffer.
    pub fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), TiffError> {
        let _scope = measurement_scope("decoded_tiff_read");
        let frame_data = self.frame(frame)?;
        validate_rect(&frame_data.header, rect)?;
        let expected = expected_len(rect, frame_data.header.samples_per_pixel)?;
        if out.len() != expected {
            return Err(TiffError::BufferSize {
                expected,
                got: out.len(),
            });
        }
        let spp = frame_data.header.samples_per_pixel as usize;
        for (index, info) in frame_data.segments.iter().copied().enumerate() {
            let (x, y) = segment_xy(info.origin);
            if !intersects(rect, x, y, info.valid_width, info.valid_height) {
                continue;
            }
            let bytes = segment_bytes(&frame_data.header, info)?;
            let segment = self.cache.get_or_decode(
                Key {
                    frame,
                    segment: index,
                },
                bytes,
                self.cache_bytes,
                || {
                    let map = Self::mapping(frame_data)?;
                    decode_segment(&frame_data.header, map, info, index)
                },
            )?;
            copy_intersection(rect, x, y, &segment, spp, out);
            drop(segment);
            self.cache.release();
        }
        Ok(())
    }

    fn frame(&self, index: usize) -> Result<&Frame, TiffError> {
        self.frames.get(index).ok_or(TiffError::FrameIndex {
            index,
            count: self.frames.len(),
        })
    }

    fn mapping(frame: &Frame) -> Result<&Mmap, TiffError> {
        if let Some(map) = frame.map.get() {
            return Ok(map);
        }
        let map = map_file(&frame.header.path)?;
        Ok(frame.map.get_or_init(|| map))
    }
}

impl FrameReader for DecodedTiffStack {
    fn len(&self) -> usize {
        DecodedTiffStack::len(self)
    }

    fn dimensions(&self) -> (u32, u32) {
        DecodedTiffStack::dimensions(self)
    }

    fn frame(&self, index: usize) -> &FrameDesc {
        &self.frames[index].desc
    }
}

impl TileReader for DecodedTiffStack {
    fn read_scratch_bytes(&self, _width: u32, _height: u32) -> u64 {
        self.decode_scratch_bytes
    }

    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        DecodedTiffStack::read_tile_into(self, frame, rect, out)
            .map_err(crate::io::FrameError::from)
            .map_err(AlgorithmError::from)
    }
}

fn stored_segment_bytes(header: &TiffHeader, info: SegmentInfo) -> Result<u64, TiffError> {
    u64::from(info.stored_width)
        .checked_mul(u64::from(info.stored_height))
        .and_then(|bytes| bytes.checked_mul(u64::from(header.samples_per_pixel)))
        .and_then(|bytes| bytes.checked_mul(2))
        .ok_or_else(|| TiffError::SegmentLayout {
            path: header.path.clone(),
            detail: "stored segment byte length overflows",
        })
}

fn segment_bytes(header: &TiffHeader, info: SegmentInfo) -> Result<u64, TiffError> {
    u64::from(info.valid_width)
        .checked_mul(u64::from(info.valid_height))
        .and_then(|bytes| bytes.checked_mul(u64::from(header.samples_per_pixel)))
        .and_then(|bytes| bytes.checked_mul(2))
        .ok_or_else(|| TiffError::SegmentLayout {
            path: header.path.clone(),
            detail: "decoded segment byte length overflows",
        })
}

fn expected_len(rect: Rect, spp: u32) -> Result<usize, TiffError> {
    usize::try_from(
        u64::from(rect.width)
            .checked_mul(u64::from(rect.height))
            .and_then(|len| len.checked_mul(u64::from(spp)))
            .ok_or(TiffError::BufferSize {
                expected: usize::MAX,
                got: 0,
            })?,
    )
    .map_err(|_| TiffError::BufferSize {
        expected: usize::MAX,
        got: 0,
    })
}

fn validate_rect(header: &TiffHeader, rect: Rect) -> Result<(), TiffError> {
    let in_bounds = rect.width > 0
        && rect.height > 0
        && rect
            .x
            .checked_add(rect.width)
            .is_some_and(|x| x <= header.width)
        && rect
            .y
            .checked_add(rect.height)
            .is_some_and(|y| y <= header.height);
    if in_bounds {
        Ok(())
    } else {
        Err(TiffError::OutOfBounds {
            path: header.path.clone(),
            requested: rect,
            frame: (header.width, header.height),
        })
    }
}

fn segment_xy(origin: SegmentOrigin) -> (u32, u32) {
    match origin {
        SegmentOrigin::Strip { y } => (0, y),
        SegmentOrigin::Tile { x, y } => (x, y),
    }
}

fn intersects(rect: Rect, x: u32, y: u32, width: u32, height: u32) -> bool {
    rect.x < x + width && x < rect.x + rect.width && rect.y < y + height && y < rect.y + rect.height
}

fn copy_intersection(
    rect: Rect,
    x: u32,
    y: u32,
    segment: &DecodedSegment,
    spp: usize,
    out: &mut [u16],
) {
    let start_x = rect.x.max(x);
    let end_x = (rect.x + rect.width).min(x + segment.valid_width);
    let start_y = rect.y.max(y);
    let end_y = (rect.y + rect.height).min(y + segment.valid_height);
    for source_y in start_y..end_y {
        let source =
            ((source_y - y) as usize * segment.valid_width as usize + (start_x - x) as usize) * spp;
        let destination = ((source_y - rect.y) as usize * rect.width as usize
            + (start_x - rect.x) as usize)
            * spp;
        let len = (end_x - start_x) as usize * spp;
        out[destination..destination + len].copy_from_slice(&segment.samples[source..source + len]);
        record_timing(
            TimingCounter::CopyBytes,
            len.saturating_mul(std::mem::size_of::<u16>()) as u64,
        );
        record_timing(TimingCounter::CopyOperations, 1);
    }
}
