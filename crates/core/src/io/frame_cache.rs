// SPDX-License-Identifier: GPL-3.0-or-later

//! A bounded cache of whole decoded frames for readers whose format has no
//! random access: a frame is decoded once and tiles are cut from it.
//!
//! The cache is generic over the frame it holds: JPEG keeps [`DecodedFrame`]
//! in the `u16` range, RAW keeps its own camera-linear `f32` frame.

use std::sync::{Arc, Mutex};

use crate::tile::Rect;

/// A frame the cache can hold, measured against its bound.
pub(crate) trait CachedFrame {
    /// The bytes this frame keeps alive, or `None` when the count overflows.
    fn retained_bytes(&self) -> Option<u64>;
}

/// One decoded frame, interleaved, in the pipeline's `u16` sample range.
#[derive(Debug)]
pub(crate) struct DecodedFrame {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) samples_per_pixel: usize,
    pub(crate) samples: Vec<u16>,
}

/// Why a tile request cannot be served.
#[derive(Debug)]
pub(crate) enum TileFault {
    OutOfBounds { requested: Rect, frame: (u32, u32) },
    BufferSize { expected: usize, got: usize },
}

/// Why the cache cannot hand out a frame.
#[derive(Debug)]
pub(crate) enum CacheFault<E> {
    /// The bound cannot hold one frame and its decode reservation.
    TooSmall { bound: u64, required: u64 },
    /// A byte count overflowed.
    Overflow,
    /// The reader's own decode failed.
    Decode(E),
}

/// Checks that `rect` lies inside a `frame` of `(width, height)` and that
/// `out_len` is the sample count it needs.
pub(crate) fn check_tile(
    rect: Rect,
    frame: (u32, u32),
    samples_per_pixel: usize,
    out_len: usize,
) -> Result<(), TileFault> {
    let in_bounds = rect.width > 0
        && rect.height > 0
        && rect
            .x
            .checked_add(rect.width)
            .is_some_and(|right| right <= frame.0)
        && rect
            .y
            .checked_add(rect.height)
            .is_some_and(|bottom| bottom <= frame.1);
    if !in_bounds {
        return Err(TileFault::OutOfBounds {
            requested: rect,
            frame,
        });
    }
    let expected = rect.width as usize * rect.height as usize * samples_per_pixel;
    if out_len != expected {
        return Err(TileFault::BufferSize {
            expected,
            got: out_len,
        });
    }
    Ok(())
}

impl CachedFrame for DecodedFrame {
    fn retained_bytes(&self) -> Option<u64> {
        u64::try_from(self.samples.len())
            .ok()?
            .checked_mul(std::mem::size_of::<u16>() as u64)
    }
}

impl DecodedFrame {
    /// Copies the pixels of `rect` into `out`, row by row.
    pub(crate) fn copy_tile(&self, rect: Rect, out: &mut [u16]) -> Result<(), TileFault> {
        check_tile(
            rect,
            (self.width, self.height),
            self.samples_per_pixel,
            out.len(),
        )?;
        let spp = self.samples_per_pixel;
        let stride = self.width as usize * spp;
        let row_len = rect.width as usize * spp;
        for (row, source_y) in (rect.y..rect.y + rect.height).enumerate() {
            let source = source_y as usize * stride + rect.x as usize * spp;
            let destination = row * row_len;
            out[destination..destination + row_len]
                .copy_from_slice(&self.samples[source..source + row_len]);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct State<F> {
    frames: Vec<(usize, Arc<F>, u64)>,
    bytes: u64,
}

impl<F> State<F> {
    /// Drops the oldest frames until `extra` more bytes fit in `bound`.
    fn make_room(&mut self, extra: u64, bound: u64) {
        while self.bytes.saturating_add(extra) > bound && !self.frames.is_empty() {
            let (_, _, evicted) = self.frames.remove(0);
            self.bytes -= evicted;
        }
    }
}

/// Decoded frames keyed by frame index, evicted oldest first, never holding
/// more than `bound` bytes.
#[derive(Debug)]
pub(crate) struct FrameCache<F = DecodedFrame> {
    bound: u64,
    state: Mutex<State<F>>,
    decode_lock: Mutex<()>,
}

impl<F: CachedFrame> FrameCache<F> {
    pub(crate) fn new(bound: u64) -> Self {
        Self {
            bound,
            state: Mutex::new(State {
                frames: Vec::new(),
                bytes: 0,
            }),
            decode_lock: Mutex::new(()),
        }
    }

    /// Returns frame `index`, calling `decode` on a miss. `required` is the
    /// peak byte count of one decode; room for it is made before `decode`
    /// runs, and decodes are serialised.
    pub(crate) fn get_or_decode<E>(
        &self,
        index: usize,
        required: u64,
        decode: impl FnOnce() -> Result<F, E>,
    ) -> Result<Arc<F>, CacheFault<E>> {
        let _decode = self
            .decode_lock
            .lock()
            .expect("decode mutex is not poisoned");
        let mut state = self.state.lock().expect("cache mutex is not poisoned");
        if let Some((_, frame, _)) = state.frames.iter().find(|(cached, _, _)| *cached == index) {
            return Ok(Arc::clone(frame));
        }
        if required > self.bound {
            return Err(CacheFault::TooSmall {
                bound: self.bound,
                required,
            });
        }
        state.make_room(required, self.bound);
        drop(state);
        let decoded = Arc::new(decode().map_err(CacheFault::Decode)?);
        let retained = decoded.retained_bytes().ok_or(CacheFault::Overflow)?;
        let mut state = self.state.lock().expect("cache mutex is not poisoned");
        state.make_room(retained, self.bound);
        state.frames.push((index, Arc::clone(&decoded), retained));
        state.bytes += retained;
        Ok(decoded)
    }
}
