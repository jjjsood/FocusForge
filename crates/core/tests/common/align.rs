// SPDX-License-Identifier: GPL-3.0-or-later

//! Fixtures, readers and metrics shared by the alignment integration tests.
//!
//! Every test binary that declares `mod common;` compiles this module, and
//! not every binary uses every helper.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use focusforge_core::algorithm::{
    AlgorithmError, AlignAlgorithm, AlignContext, AlignOutcome, AlignStatus, TileReader,
};
use focusforge_core::align::warp::warp_from_tile;
use focusforge_core::align::{AlignPyramid, FrameAlignment, PyramidalEcc, Transform};
use focusforge_core::io::{FrameError, TiffError, TiffStack};
use focusforge_core::params::{self, ParamValue, ParamValues, StackContext};
use focusforge_core::tile::{Rect, Tile};

/// The sharpest `fixtures/mantis-512` frame, the one the ground-truth tests
/// warp.
const FRAME_3: usize = 3;

pub fn open_fixture() -> TiffStack {
    TiffStack::open(super::mantis_512_dir()).expect("opening fixtures/mantis-512")
}

/// Opens the fixture tree `name` under `fixtures/` with the [`StackContext`]
/// its first frame describes.
pub fn open_stack_context(name: &str) -> (TiffStack, StackContext) {
    let stack = TiffStack::open(super::fixtures_dir().join(name))
        .unwrap_or_else(|err| panic!("{name} must open: {err}"));
    let ctx = stack_context(&stack);
    (stack, ctx)
}

/// The [`StackContext`] `stack`'s first frame describes.
pub fn stack_context(stack: &TiffStack) -> StackContext {
    let (width, height) = stack.dimensions();
    StackContext::new(
        stack.len(),
        width,
        height,
        stack.header(0).samples_per_pixel,
    )
}

pub fn full_rect(width: u32, height: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

/// Opens `fixtures/mantis-512` and reads frame 3's full tile, returning it
/// with the frame's width, height and samples per pixel.
pub fn frame_3_tile() -> (Tile, u32, u32, u32) {
    let stack = open_fixture();
    let (width, height) = stack.dimensions();
    let spp = stack.header(FRAME_3).samples_per_pixel;
    let rect = full_rect(width, height);
    let tile = stack
        .read_tile(FRAME_3, rect)
        .expect("reading frame 3's tile");
    (tile, width, height, spp)
}

/// A rotation+scale [`Transform`] about `(cx, cy)`.
pub fn similarity_about(scale: f64, radians: f64, cx: f64, cy: f64) -> Transform {
    let to_origin = Transform::translation(-cx, -cy);
    let rot_scale = Transform::similarity(scale, radians, 0.0, 0.0);
    let back = Transform::translation(cx, cy);
    back.compose(&rot_scale).compose(&to_origin)
}

/// The region the ground-truth tests solve over: a 64px inset of the 512px
/// frame, equal to the rect `PyramidalEcc::solve_region` computes for a
/// 512px level and far enough from the edges that no clamp-to-edge rim
/// pixel of a warp enters the accumulation.
pub fn default_region() -> Rect {
    Rect {
        x: 64,
        y: 64,
        width: 384,
        height: 384,
    }
}

/// The maximum, over `region`'s four corners, of the Euclidean distance
/// between where `a` and `b` map that corner -- the same "corner
/// displacement" measure the solver's own termination criterion uses.
pub fn corner_error(a: &Transform, b: &Transform, region: Rect) -> f64 {
    let corners = [
        (region.x as f64, region.y as f64),
        ((region.x + region.width - 1) as f64, region.y as f64),
        (region.x as f64, (region.y + region.height - 1) as f64),
        (
            (region.x + region.width - 1) as f64,
            (region.y + region.height - 1) as f64,
        ),
    ];
    corners
        .iter()
        .map(|&(x, y)| {
            let (ax, ay) = a.map_point(x, y).expect("a stays in domain");
            let (bx, by) = b.map_point(x, y).expect("b stays in domain");
            ((ax - bx).powi(2) + (ay - by).powi(2)).sqrt()
        })
        .fold(0.0f64, f64::max)
}

/// Fully resolved defaults for `PyramidalEcc` against `ctx`: what a caller
/// hands `align_frame`, per the "fully resolved `ParamValues`" contract
/// `StackAlgorithm::fuse` documents.
pub fn resolved_defaults(ctx: &StackContext) -> ParamValues {
    let resolved = params::resolve(PyramidalEcc.params(), &ParamValues::new(), ctx)
        .expect("defaults resolve cleanly");
    params::resolved_values(&resolved)
}

/// [`resolved_defaults`] under the anchor reference policy.
pub fn anchor_defaults(ctx: &StackContext) -> ParamValues {
    let mut cfg = resolved_defaults(ctx);
    cfg.insert(
        "reference-policy".to_string(),
        ParamValue::Enum("anchor".to_string()),
    );
    cfg
}

/// [`anchor_defaults`] with `max-iterations` lowered from the shipped 400 to
/// 40, which an 8-frame fixture align needs far less than.
pub fn lowered_max_iterations(ctx: &StackContext) -> ParamValues {
    let mut cfg = anchor_defaults(ctx);
    cfg.insert("max-iterations".to_string(), ParamValue::Int(40));
    cfg
}

/// `PyramidalEcc::align_frame` called directly over a plain
/// [`AlignContext::new`] with nothing prepared: the baseline a cached run must
/// match bit for bit.
pub fn uncached_outcomes(
    stack_ctx: StackContext,
    reader: &dyn TileReader,
    reference: usize,
    cfg: &ParamValues,
) -> Vec<AlignOutcome> {
    let ctx = AlignContext::new(stack_ctx, reference, reader);
    (0..stack_ctx.frame_count)
        .map(|i| {
            PyramidalEcc
                .align_frame(&ctx, i, cfg)
                .expect("uncached align_frame must not error")
        })
        .collect()
}

/// The off-centre patches of a 512 px frame: a 3x3 grid without its middle.
pub const PATCH_COUNT: usize = 8;

/// Reads one registration makes of a frame's own pixels up to its solve: the
/// level-1 build reads each input row once, the first band in one read and
/// every later band of `AlignPyramid::BAND_ROWS` output rows in one read of
/// its new rows (none once the frame's last row is in), without the level-0
/// window or patches.
pub fn streamed_band_reads(stack: &StackContext) -> usize {
    let band = AlignPyramid::BAND_ROWS;
    let (height, output_rows) = (stack.height, stack.height.div_ceil(2));
    let later = (1..)
        .map(|k| k * band)
        .take_while(|&start| start < output_rows)
        .filter(|&start| 2 * start + 3 < height)
        .count();
    1 + later
}

/// The reference frame is read as bands, one level-0 template window and one
/// window per patch template -- once per run, whatever the registration count.
pub fn reference_reads(stack: &StackContext) -> usize {
    streamed_band_reads(stack) + 1 + PATCH_COUNT
}

/// Reads one escalation adds for a frame no larger than the detector's 512 px
/// proxy: the whole frame at level 0. The reference's is made by the first
/// escalation of a run and shared by every later one.
pub const ESCALATION_READS: usize = 1;

/// Deterministic xorshift32 PRNG, reproducible across runs.
pub struct XorShift32(pub u32);

impl XorShift32 {
    pub fn next(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
}

/// A synthetic in-memory [`TileReader`] over an arbitrary number of
/// same-sized frames, honouring the same exact-length and in-bounds
/// contracts as [`TiffStack::read_tile_into`].
pub struct FramesReader {
    pub width: u32,
    pub height: u32,
    pub spp: u32,
    pub frames: Vec<Vec<u16>>,
}

impl TileReader for FramesReader {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        let Some(src) = self.frames.get(frame) else {
            return Err(AlgorithmError::FrameOutOfRange {
                frame,
                frame_count: self.frames.len(),
            });
        };
        let expected = rect.width as usize * rect.height as usize * self.spp as usize;
        if out.len() != expected {
            return Err(AlgorithmError::Frame(FrameError::Tiff(
                TiffError::BufferSize {
                    expected,
                    got: out.len(),
                },
            )));
        }
        let in_bounds = rect
            .x
            .checked_add(rect.width)
            .is_some_and(|xw| xw <= self.width)
            && rect
                .y
                .checked_add(rect.height)
                .is_some_and(|yh| yh <= self.height);
        if !in_bounds {
            return Err(AlgorithmError::Frame(FrameError::Tiff(
                TiffError::OutOfBounds {
                    path: PathBuf::from("<memory>"),
                    requested: rect,
                    frame: (self.width, self.height),
                },
            )));
        }
        let stride = self.width as usize * self.spp as usize;
        let row_len = rect.width as usize * self.spp as usize;
        let mut off = 0;
        for r in rect.y..(rect.y + rect.height) {
            let row_start = r as usize * stride + rect.x as usize * self.spp as usize;
            out[off..off + row_len].copy_from_slice(&src[row_start..row_start + row_len]);
            off += row_len;
        }
        Ok(())
    }
}

/// A two-frame reader: frame 0 is `reference`, frame 1 is `moving`.
pub fn two_frame_reader(
    width: u32,
    height: u32,
    spp: u32,
    reference: Vec<u16>,
    moving: Vec<u16>,
) -> FramesReader {
    FramesReader {
        width,
        height,
        spp,
        frames: vec![reference, moving],
    }
}

/// Hashes a lattice point to `[-1, 1]`.
fn lattice(ix: i64, iy: i64, seed: u32) -> f64 {
    let mut h = (ix as u32).wrapping_mul(0x9E37_79B1)
        ^ (iy as u32).wrapping_mul(0x85EB_CA77)
        ^ seed.wrapping_mul(0xC2B2_AE3D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^= h >> 15;
    f64::from(h) / f64::from(u32::MAX) * 2.0 - 1.0
}

/// Smooth value noise of wavelength `period` px, in `[-1, 1]`, defined at
/// every real coordinate.
pub fn value_noise(x: f64, y: f64, period: f64, seed: u32) -> f64 {
    let (u, v) = (x / period, y / period);
    let (iu, iv) = (u.floor(), v.floor());
    let fade = |t: f64| t * t * t * (t * (t * 6.0 - 15.0) + 10.0);
    let (fu, fv) = (fade(u - iu), fade(v - iv));
    let (ix, iy) = (iu as i64, iv as i64);
    let top = lattice(ix, iy, seed) * (1.0 - fu) + lattice(ix + 1, iy, seed) * fu;
    let bottom = lattice(ix, iy + 1, seed) * (1.0 - fu) + lattice(ix + 1, iy + 1, seed) * fu;
    top * (1.0 - fv) + bottom * fv
}

/// A multi-scale texture with detail at every pyramid level, in `[-1, 1]`,
/// as a camera focused `sigma` px off the surface sees it: each octave of
/// wavelength `p` keeps the share `exp(-2 pi^2 sigma^2 / p^2)` of its amplitude.
pub fn blurred_scene(x: f64, y: f64, sigma: f64) -> f64 {
    [
        (256.0, 1.0),
        (96.0, 0.8),
        (40.0, 0.6),
        (18.0, 0.4),
        (9.0, 0.25),
    ]
    .iter()
    .enumerate()
    .map(|(i, &(period, weight))| {
        let kept = (-2.0 * std::f64::consts::PI.powi(2) * sigma * sigma / (period * period)).exp();
        weight * kept * value_noise(x, y, period, 11 + i as u32)
    })
    .sum::<f64>()
        / 2.2
}

/// How far frame `i` of a drifting stack has moved from the reference.
pub struct Drift {
    pub shift: (f64, f64),
    pub scale: f64,
}

/// `count` one-sample frames of a scene whose depth ramps from left to right;
/// frame `i` is focused on the column `i / (count - 1)` of the way across and
/// blurred in proportion to each column's distance from it. Each frame is
/// magnified and shifted by `drift` for every step it lies from the middle
/// frame. Returns the frames and, per frame, the transform that maps the
/// middle frame onto it.
pub fn drifting_focus_stack(
    (width, height): (u32, u32),
    count: usize,
    drift: &Drift,
    blur_px: f64,
) -> (FramesReader, Vec<Transform>) {
    let middle = (count - 1) / 2;
    let (cx, cy) = (f64::from(width) / 2.0, f64::from(height) / 2.0);
    let truth: Vec<Transform> = (0..count)
        .map(|i| {
            let steps = i as f64 - middle as f64;
            Transform::translation(drift.shift.0 * steps, drift.shift.1 * steps)
                .compose(&similarity_about(1.0 + drift.scale * steps, 0.0, cx, cy))
        })
        .collect();
    let mut rng = XorShift32(0xF0C5_0001);
    let frames = truth
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let inverse = w.inverse().expect("a similarity is invertible");
            let focus = i as f64 / (count - 1) as f64;
            let mut samples = Vec::with_capacity((width * height) as usize);
            for y in 0..height {
                for x in 0..width {
                    let (sx, sy) = inverse
                        .map_point(f64::from(x), f64::from(y))
                        .expect("a similarity maps every pixel");
                    let sigma = blur_px * (sx / f64::from(width) - focus).abs();
                    let noise = f64::from(rng.next() % 321) - 160.0;
                    let value = 0.5 + 0.4 * blurred_scene(sx, sy, sigma);
                    samples.push((value * 65_535.0 + noise).clamp(0.0, 65_535.0) as u16);
                }
            }
            samples
        })
        .collect();
    (
        FramesReader {
            width,
            height,
            spp: 1,
            frames,
        },
        truth,
    )
}

/// Frame 3 of `fixtures/mantis-512` translated by `dx` columns with the
/// vacated columns zero-filled, with the unwarped frame's tile, width, height
/// and samples per pixel.
pub fn shifted_frame_3(dx: f64) -> (Vec<u16>, Tile, u32, u32, u32) {
    let (tile, width, height, spp) = frame_3_tile();
    let warped = warp_from_tile(
        &tile,
        (width, height),
        full_rect(width, height),
        &Transform::translation(dx, 0.0),
    )
    .expect("the translation stays inside the frame");
    (warped.tile.samples, tile, width, height, spp)
}

/// One alignment result as exact bits and its status, so a recorded run and a
/// later one compare without a tolerance.
fn fingerprint(
    transform: &Transform,
    status: AlignStatus,
    iterations: u32,
    residual: f64,
) -> String {
    let m: Vec<String> = transform
        .m
        .iter()
        .flatten()
        .map(|v| format!("{:016x}", v.to_bits()))
        .collect();
    format!(
        "m=[{}] status={:?} iterations={} residual={:016x}",
        m.join(","),
        status,
        iterations,
        residual.to_bits()
    )
}

pub fn outcome_fingerprint(outcome: &AlignOutcome) -> String {
    fingerprint(
        &outcome.transform,
        outcome.status,
        outcome.iterations,
        outcome.residual,
    )
}

pub fn frame_fingerprint(frame: &FrameAlignment) -> String {
    fingerprint(
        &frame.transform,
        frame.status,
        frame.iterations,
        frame.residual,
    )
}

/// A fingerprint without its status token: the transform, iteration count and
/// residual bits alone.
fn without_status(fingerprint: &str) -> String {
    fingerprint
        .split_whitespace()
        .filter(|token| !token.starts_with("status="))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Asserts `now` equals `recorded` frame by frame, solve tokens before the
/// status token, so a changed status cannot hide a changed solve.
pub fn assert_fingerprints_match(input: &str, now: &[String], recorded: &[&str]) {
    assert_eq!(now.len(), recorded.len(), "{input}: frame count");
    for (i, (now, then)) in now.iter().zip(recorded).enumerate() {
        assert_eq!(
            without_status(now),
            without_status(then),
            "{input}: frame {i} transform, iterations, residual"
        );
        assert_eq!(now, then, "{input}: frame {i} full fingerprint with status");
    }
}

/// A 64-bit FNV-1a digest over every sample of every frame in little-endian
/// byte order, stable across toolchains and platforms.
pub fn samples_digest(reader: &FramesReader) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for sample in reader.frames.iter().flatten() {
        for byte in sample.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

/// Wraps another [`TileReader`], counting `read_tile_into` calls per frame
/// index. One `AtomicUsize` per frame, so concurrent registrations count
/// correctly with no lock.
pub struct CountingReader<'a> {
    pub inner: &'a dyn TileReader,
    pub reads: Vec<AtomicUsize>,
}

impl<'a> CountingReader<'a> {
    pub fn new(inner: &'a dyn TileReader, frame_count: usize) -> Self {
        CountingReader {
            inner,
            reads: (0..frame_count).map(|_| AtomicUsize::new(0)).collect(),
        }
    }

    pub fn reads_for(&self, frame: usize) -> usize {
        self.reads[frame].load(Ordering::SeqCst)
    }
}

impl TileReader for CountingReader<'_> {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        self.reads[frame].fetch_add(1, Ordering::SeqCst);
        self.inner.read_tile_into(frame, rect, out)
    }
}

/// Wraps another [`TileReader`], counting every `read_tile_into` call in one
/// counter, for asserting "no read attempted".
pub struct CallCountingReader<'a> {
    pub inner: &'a dyn TileReader,
    pub calls: AtomicUsize,
}

impl TileReader for CallCountingReader<'_> {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.read_tile_into(frame, rect, out)
    }
}
