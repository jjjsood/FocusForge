// SPDX-License-Identifier: GPL-3.0-or-later

//! The Laplacian fusion path in WGSL: one kernel per CPU twin in
//! `crates/core/src/fuse/{pyramid,laplacian}.rs`, composed by
//! [`fuse_tile_gpu`] into the control flow of
//! `focusforge_core::fuse::laplacian::Laplacian::fuse`. Each kernel is a
//! sibling `.wgsl` file; [`convert`], [`pyramid`] and [`select`] wrap them
//! with plain `&wgpu::Buffer` + dimension arguments so parity tests call them.
//!
//! **Frame 0 is not special-cased.** `best`'s score starts at
//! `f32::NEG_INFINITY` and the residual sum at `0.0`, and every frame runs
//! the same select/accumulate dispatches. Frame 0's score is finite, so
//! select installs it by copy, and `0.0 + x == x` in `f32`: both are
//! bit-identical to the CPU twin's "seed from frame 0, then fold in the rest".

pub mod convert;
pub mod pyramid;
pub mod select;

use std::collections::HashMap;
use std::sync::OnceLock;

use focusforge_core::algorithm::{AlgorithmError, TileOutput, TileReader};
use focusforge_core::fuse::laplacian::{EligibleMasks, score_radius};
use focusforge_core::tile::{Rect, Tile};
use focusforge_core::timing::{
    TimingCounter, gpu_tile_scope, gpu_tile_stage, measurement_scope, record_timing,
};
use focusforge_core::zmap::{NO_SOURCE, validate_tile};

use crate::device::SharedDevice;
use crate::pack::{packed_len, unpack_samples};
use crate::pipeline::{
    Pipeline, build_pipeline, record_pass, run_pass, storage_entry, uniform_entry, upload_params,
};
use crate::transfer::{create_storage_buffer, download_buffer, upload_storage, upload_tile};

/// Shape A: one storage buffer read, one storage buffer read/write, one
/// uniform -- `plane_from_u16`, `plane_to_u16`, `reduce::{horizontal,
/// vertical}`, `expand::{horizontal, vertical}`, `score::{rows, cols}`,
/// `eligibility::exclude`.
fn shape_a() -> Vec<wgpu::BindGroupLayoutEntry> {
    vec![
        storage_entry(0, true),
        storage_entry(1, false),
        uniform_entry(2),
    ]
}

/// Shape B: two storage buffers read, one storage buffer read/write, one
/// uniform -- `combine::{sub, add}`.
fn shape_b() -> Vec<wgpu::BindGroupLayoutEntry> {
    vec![
        storage_entry(0, true),
        storage_entry(1, true),
        storage_entry(2, false),
        uniform_entry(3),
    ]
}

/// Shape C: two storage buffers read, two read/write, one uniform --
/// `select::main`, exactly the four-storage-buffer `downlevel_defaults()`
/// limit. The largest fusion plane (352x352x3 at the 448/5 geometry) is
/// 1,486,848 bytes; the largest dispatch (`plane_from_u16`/`plane_to_u16`
/// there) is `ceil(352*352*3/2/64) = 2,904` workgroups.
fn shape_c() -> Vec<wgpu::BindGroupLayoutEntry> {
    vec![
        storage_entry(0, true),
        storage_entry(1, true),
        storage_entry(2, false),
        storage_entry(3, false),
        uniform_entry(4),
    ]
}

/// Shape E: shape A plus one more storage buffer read/write (the
/// compensation term) -- `residual::accumulate`,
/// `eligibility::mean_by_count`. Three storage buffers, inside the four-buffer
/// `downlevel_defaults()` limit.
fn shape_e() -> Vec<wgpu::BindGroupLayoutEntry> {
    let mut entries = shape_a();
    entries.push(storage_entry(3, false));
    entries
}

/// One `OnceLock<Pipeline>` per kernel entry point, built on first use.
#[derive(Default)]
pub struct FusionPipelines {
    plane_from_u16: OnceLock<Pipeline>,
    plane_to_u16: OnceLock<Pipeline>,
    reduce_horizontal: OnceLock<Pipeline>,
    reduce_vertical: OnceLock<Pipeline>,
    expand_horizontal: OnceLock<Pipeline>,
    expand_vertical: OnceLock<Pipeline>,
    combine_sub: OnceLock<Pipeline>,
    combine_add: OnceLock<Pipeline>,
    score_rows: OnceLock<Pipeline>,
    score_cols: OnceLock<Pipeline>,
    select_main: OnceLock<Pipeline>,
    residual_accumulate: OnceLock<Pipeline>,
    eligibility_exclude: OnceLock<Pipeline>,
    eligibility_mean_by_count: OnceLock<Pipeline>,
}

macro_rules! pipeline_getter {
    ($name:ident, $wgsl:literal, $label:literal, $entry:literal, $shape:expr) => {
        fn $name(&self, device: &wgpu::Device) -> &Pipeline {
            self.$name.get_or_init(|| {
                build_pipeline(device, include_str!($wgsl), $label, $entry, &$shape())
            })
        }
    };
}

impl FusionPipelines {
    pub fn new() -> Self {
        Self::default()
    }

    pipeline_getter!(
        plane_from_u16,
        "plane_from_u16.wgsl",
        "focusforge-gpu-fuse-plane-from-u16",
        "main",
        shape_a
    );
    pipeline_getter!(
        plane_to_u16,
        "plane_to_u16.wgsl",
        "focusforge-gpu-fuse-plane-to-u16",
        "main",
        shape_a
    );
    pipeline_getter!(
        reduce_horizontal,
        "reduce.wgsl",
        "focusforge-gpu-fuse-reduce-horizontal",
        "horizontal",
        shape_a
    );
    pipeline_getter!(
        reduce_vertical,
        "reduce.wgsl",
        "focusforge-gpu-fuse-reduce-vertical",
        "vertical",
        shape_a
    );
    pipeline_getter!(
        expand_horizontal,
        "expand.wgsl",
        "focusforge-gpu-fuse-expand-horizontal",
        "horizontal",
        shape_a
    );
    pipeline_getter!(
        expand_vertical,
        "expand.wgsl",
        "focusforge-gpu-fuse-expand-vertical",
        "vertical",
        shape_a
    );
    pipeline_getter!(
        combine_sub,
        "combine.wgsl",
        "focusforge-gpu-fuse-combine-sub",
        "sub",
        shape_b
    );
    pipeline_getter!(
        combine_add,
        "combine.wgsl",
        "focusforge-gpu-fuse-combine-add",
        "add",
        shape_b
    );
    pipeline_getter!(
        score_rows,
        "score.wgsl",
        "focusforge-gpu-fuse-score-rows",
        "rows",
        shape_a
    );
    pipeline_getter!(
        score_cols,
        "score.wgsl",
        "focusforge-gpu-fuse-score-cols",
        "cols",
        shape_a
    );
    pipeline_getter!(
        select_main,
        "select.wgsl",
        "focusforge-gpu-fuse-select",
        "main",
        shape_c
    );
    pipeline_getter!(
        residual_accumulate,
        "residual.wgsl",
        "focusforge-gpu-fuse-residual-accumulate",
        "accumulate",
        shape_e
    );
    pipeline_getter!(
        eligibility_exclude,
        "eligibility.wgsl",
        "focusforge-gpu-fuse-eligibility-exclude",
        "exclude",
        shape_a
    );
    pipeline_getter!(
        eligibility_mean_by_count,
        "eligibility.wgsl",
        "focusforge-gpu-fuse-eligibility-mean-by-count",
        "mean_by_count",
        shape_e
    );
}

/// One `f32` pyramid level's buffer plus the dimensions needed to dispatch
/// a kernel over it -- an internal bookkeeping convenience for
/// [`fuse_tile_gpu`]'s per-frame gaussian list, mirroring
/// `focusforge_core::fuse::pyramid::Plane`'s shape with the data replaced
/// by a device buffer. Not exposed to [`convert`], [`pyramid`] or
/// [`select`]: those take plain buffers and dimensions so a per-kernel
/// parity test in a different crate can call them without this type.
pub(crate) struct GpuPlane {
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
    spp: u32,
}

type ScratchKey = (&'static str, u64);

/// One tile frame's commands, recorded into one encoder and submitted once.
/// [`scratch`](Self::scratch) pools storage buffers by label and size, reused
/// by each later batch and never zeroed; a buffer that must start at zero is a
/// fresh [`create_storage_buffer`]. The host never writes a pooled buffer.
pub struct TileBatch<'a> {
    device: &'a wgpu::Device,
    queue: &'a wgpu::Queue,
    encoder: Option<wgpu::CommandEncoder>,
    scratch: HashMap<ScratchKey, Vec<wgpu::Buffer>>,
    taken: HashMap<ScratchKey, usize>,
    params: HashMap<Vec<u8>, wgpu::Buffer>,
    retained: Vec<wgpu::Buffer>,
}

impl<'a> TileBatch<'a> {
    pub fn new(device: &'a wgpu::Device, queue: &'a wgpu::Queue) -> Self {
        Self {
            device,
            queue,
            encoder: None,
            scratch: HashMap::new(),
            taken: HashMap::new(),
            params: HashMap::new(),
            retained: Vec::new(),
        }
    }

    pub fn device(&self) -> &'a wgpu::Device {
        self.device
    }

    /// A pooled storage buffer of `byte_len` bytes, distinct from every other
    /// buffer handed out since the last [`submit`](Self::submit).
    pub(crate) fn scratch(&mut self, byte_len: u64, label: &'static str) -> wgpu::Buffer {
        let key = (label, byte_len);
        let taken = self.taken.entry(key).or_default();
        let pool = self.scratch.entry(key).or_default();
        if *taken == pool.len() {
            pool.push(create_storage_buffer(self.device, byte_len, label));
        }
        let buffer = pool[*taken].clone();
        *taken += 1;
        buffer
    }

    /// A uniform buffer holding `bytes`, created once per distinct content and
    /// shared by every dispatch that binds it.
    pub(crate) fn params(&mut self, bytes: &[u8]) -> wgpu::Buffer {
        if let Some(buffer) = self.params.get(bytes) {
            return buffer.clone();
        }
        let buffer = upload_params(self.device, self.queue, bytes);
        self.params.insert(bytes.to_vec(), buffer.clone());
        buffer
    }

    /// A uniform buffer for contents that do not repeat, held only until the
    /// next [`submit`](Self::submit), never cached.
    pub(crate) fn params_once(&mut self, bytes: &[u8]) -> wgpu::Buffer {
        let buffer = upload_params(self.device, self.queue, bytes);
        self.retain(buffer)
    }

    /// Keeps `buffer` alive until the next [`submit`](Self::submit), for a
    /// buffer the host wrote that this batch's commands read.
    pub(crate) fn retain(&mut self, buffer: wgpu::Buffer) -> wgpu::Buffer {
        self.retained.push(buffer.clone());
        buffer
    }

    /// Records one 1-D dispatch; see [`record_pass`].
    pub(crate) fn record(
        &mut self,
        pipeline: &Pipeline,
        entries: &[wgpu::BindGroupEntry],
        invocation_count: u32,
        label: &str,
    ) {
        let device = self.device;
        let encoder = self.encoder.get_or_insert_with(|| {
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("focusforge-gpu-fuse-batch"),
            })
        });
        record_pass(device, encoder, pipeline, entries, invocation_count, label);
    }

    /// Submits everything recorded since the last submit as one command
    /// buffer; a batch with nothing recorded submits nothing.
    pub fn submit(&mut self) {
        let _scope = measurement_scope("gpu_cpu_submit");
        if let Some(encoder) = self.encoder.take() {
            self.queue.submit(Some(encoder.finish()));
            record_timing(TimingCounter::CpuSubmissions, 1);
        }
        self.retained.clear();
        self.taken.clear();
    }
}

/// Converts an already-resident packed-`u32` buffer (`packed_len(width *
/// height * spp)` bytes, two `u16` per `u32`, low half first, chunky sample
/// order -- `crate::pack`, `crate::warp::warp_dispatch`'s own destination
/// buffer) into a [`GpuPlane`] via [`convert::plane_from_u16`]. This is the
/// no-repack join the fused pipeline needs at the warp -> fusion boundary:
/// the warp kernel's destination buffer is byte-for-byte what this function
/// consumes, so nothing here ever downloads or re-uploads a sample.
pub(crate) fn plane_from_packed(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipelines: &FusionPipelines,
    packed: &wgpu::Buffer,
    width: u32,
    height: u32,
    spp: u32,
) -> GpuPlane {
    let _stage = gpu_tile_stage(TimingCounter::GpuTilePyramidNs);
    let sample_count = width as usize * height as usize * spp as usize;
    let word_count = (packed_len(sample_count) / 4) as u32;
    let buffer = convert::plane_from_u16(
        device,
        queue,
        pipelines,
        packed,
        word_count,
        sample_count as u32,
    );
    GpuPlane {
        buffer,
        width,
        height,
        spp,
    }
}

/// Uploads `tile`'s samples through the packed-`u32` transport plus
/// [`plane_from_packed`] -- the only place `u16` samples cross the bus in
/// this module for a tile that has not already been dispatched on the GPU
/// (see [`crate::stack_tile::warp_fuse_tile_gpu`] for the identity-transform
/// case, which reads and uploads exactly this way).
fn upload_plane(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipelines: &FusionPipelines,
    tile: &Tile,
) -> GpuPlane {
    let packed = upload_tile(device, queue, tile);
    plane_from_packed(
        device,
        queue,
        pipelines,
        &packed,
        tile.rect.width,
        tile.rect.height,
        tile.samples_per_pixel,
    )
}

/// Builds the Gaussian pyramid `G_0..G_N` of `base` on the GPU -- `levels`
/// [`pyramid::reduce`] dispatches, `G_0` being `base` itself (no dispatch),
/// mirroring `focusforge_core::fuse::pyramid::gaussian_levels`.
fn gaussian_levels_gpu(
    batch: &mut TileBatch,
    pipelines: &FusionPipelines,
    base: GpuPlane,
    levels: u32,
) -> Vec<GpuPlane> {
    let mut gaussians = Vec::with_capacity(levels as usize + 1);
    gaussians.push(base);
    for _ in 0..levels {
        let (src_width, src_height, spp) = {
            let src = gaussians.last().expect("gaussians is never empty");
            (src.width, src.height, src.spp)
        };
        let src_buffer = &gaussians.last().expect("gaussians is never empty").buffer;
        let (buffer, out_width, out_height) =
            pyramid::reduce(batch, pipelines, src_buffer, src_width, src_height, spp);
        gaussians.push(GpuPlane {
            buffer,
            width: out_width,
            height: out_height,
            spp,
        });
    }
    gaussians
}

/// One frame's resident base plane and, when the frame has uncovered pixels,
/// its row-major coverage mask over the tile rect.
pub(crate) type FrameSource = (GpuPlane, Option<Vec<bool>>);

fn upload_u32(device: &wgpu::Device, queue: &wgpu::Queue, values: &[u32]) -> wgpu::Buffer {
    let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    upload_storage(device, queue, &bytes, "focusforge-gpu-fuse-eligibility")
}

/// Whether `mask` excludes no pixel, so the exclude pass would change nothing.
fn all_true(mask: &[bool]) -> bool {
    mask.iter().all(|&ok| ok)
}

fn upload_mask(device: &wgpu::Device, queue: &wgpu::Queue, mask: &[bool]) -> wgpu::Buffer {
    let values: Vec<u32> = mask.iter().map(|&ok| u32::from(ok)).collect();
    upload_u32(device, queue, &values)
}

/// The running best for one Laplacian level across frames: its level data
/// (in-place-updated by [`select::select`]) and its interleaved
/// `(score, winner)` plane.
struct BestLevel {
    level: wgpu::Buffer,
    best: wgpu::Buffer,
    width: u32,
    height: u32,
}

/// Fuses `rect` across `frame_count` frames at pyramid depth `levels`,
/// entirely on the GPU -- the WGSL twin of `Laplacian::fuse`. `source`
/// supplies frame `i`'s base [`GpuPlane`], already resident. The two
/// `download_buffer` calls at the tail (composite, Z-Map) are the only
/// readbacks; the module docs cover why frame 0 needs no special case.
pub(crate) fn fuse_planes_gpu(
    shared: &SharedDevice,
    pipelines: &FusionPipelines,
    frame_count: usize,
    rect: Rect,
    samples_per_pixel: u32,
    levels: u32,
    source: &mut dyn FnMut(usize) -> Result<FrameSource, AlgorithmError>,
) -> Result<TileOutput, AlgorithmError> {
    let _tile = gpu_tile_scope();
    let device = shared.device();
    let queue = shared.queue();
    let spp = samples_per_pixel;
    let mut batch = TileBatch::new(device, queue);

    let mut best_levels: Vec<Option<BestLevel>> = (0..levels).map(|_| None).collect();
    let mut residual_sum: Option<(wgpu::Buffer, wgpu::Buffer)> = None;
    let mut residual_dims = (0u32, 0u32);
    // Frames contributing at each residual pixel.
    let mut residual_counts: Vec<u32> = Vec::new();

    for frame in 0..frame_count {
        let (base, covered) = source(frame)?;
        // A frame whose every eligibility mask is all-true takes the unmasked
        // path: no upload, no exclude pass, a plain `+1` residual count.
        let masks = covered
            .as_deref()
            .map(|covered| EligibleMasks::new(covered, rect.width, rect.height, levels))
            .filter(|masks| {
                !masks.levels.iter().all(|m| all_true(m)) || !all_true(&masks.residual)
            });
        let gaussians = {
            let _stage = gpu_tile_stage(TimingCounter::GpuTilePyramidNs);
            gaussian_levels_gpu(&mut batch, pipelines, base, levels)
        };

        for k in 0..levels as usize {
            let (g_k1_buffer, g_k1_width, g_k1_height, g_k1_spp) = {
                let g = &gaussians[k + 1];
                (&g.buffer, g.width, g.height, g.spp)
            };
            let (g_k_width, g_k_height, g_k_spp) = {
                let g = &gaussians[k];
                (g.width, g.height, g.spp)
            };
            let laplacian_len = g_k_width * g_k_height * g_k_spp;
            let laplacian = {
                let _stage = gpu_tile_stage(TimingCounter::GpuTilePyramidNs);
                let expanded = pyramid::expand(
                    &mut batch,
                    pipelines,
                    g_k1_buffer,
                    g_k1_width,
                    g_k1_height,
                    g_k1_spp,
                    g_k_width,
                    g_k_height,
                );
                pyramid::sub(
                    &mut batch,
                    pipelines,
                    &gaussians[k].buffer,
                    &expanded,
                    laplacian_len,
                )
            };

            let _select = gpu_tile_stage(TimingCounter::GpuTileSelectNs);
            let radius = score_radius(levels, k as u32);
            let score = select::score(
                &mut batch, pipelines, &laplacian, g_k_width, g_k_height, g_k_spp, radius,
            );
            if let Some(masks) = masks.as_ref().filter(|masks| !all_true(&masks.levels[k])) {
                let mask = batch.retain(upload_mask(device, queue, &masks.levels[k]));
                select::exclude(
                    &mut batch,
                    pipelines,
                    &score,
                    &mask,
                    g_k_width * g_k_height,
                    1,
                    f32::NEG_INFINITY,
                );
            }

            let slot = best_levels[k].get_or_insert_with(|| BestLevel {
                level: create_storage_buffer(
                    device,
                    u64::from(laplacian_len) * 4,
                    "focusforge-gpu-fuse-best-level",
                ),
                best: {
                    let count = (g_k_width as usize) * (g_k_height as usize);
                    let mut bytes = Vec::with_capacity(count * 8);
                    for _ in 0..count {
                        bytes.extend_from_slice(&f32::NEG_INFINITY.to_le_bytes());
                        bytes.extend_from_slice(&u32::from(NO_SOURCE).to_le_bytes());
                    }
                    upload_storage(device, queue, &bytes, "focusforge-gpu-fuse-best")
                },
                width: g_k_width,
                height: g_k_height,
            });

            select::select(
                &mut batch,
                pipelines,
                &laplacian,
                &score,
                &slot.level,
                &slot.best,
                g_k_width,
                g_k_height,
                g_k_spp,
                frame as u32,
            );
        }

        let _residual = gpu_tile_stage(TimingCounter::GpuTileSelectNs);
        let residual_idx = levels as usize;
        let (residual_buffer, residual_width, residual_height, residual_spp) = {
            let g = &gaussians[residual_idx];
            (&g.buffer, g.width, g.height, g.spp)
        };
        residual_dims = (residual_width, residual_height);
        let residual_len = residual_width * residual_height * residual_spp;
        let (hi, lo) = residual_sum.get_or_insert_with(|| {
            let plane = |label| create_storage_buffer(device, u64::from(residual_len) * 4, label);
            (
                plane("focusforge-gpu-fuse-residual-sum-hi"),
                plane("focusforge-gpu-fuse-residual-sum-lo"),
            )
        });
        if residual_counts.is_empty() {
            residual_counts = vec![0; (residual_width * residual_height) as usize];
        }
        match masks.as_ref().filter(|masks| !all_true(&masks.residual)) {
            Some(masks) => {
                let mask = batch.retain(upload_mask(device, queue, &masks.residual));
                select::exclude(
                    &mut batch,
                    pipelines,
                    residual_buffer,
                    &mask,
                    residual_len,
                    residual_spp,
                    0.0,
                );
                for (count, &ok) in residual_counts.iter_mut().zip(&masks.residual) {
                    *count += u32::from(ok);
                }
            }
            None => residual_counts.iter_mut().for_each(|count| *count += 1),
        }
        select::accumulate(&mut batch, pipelines, hi, lo, residual_buffer, residual_len);
        batch.submit();
    }

    let reconstruct = gpu_tile_stage(TimingCounter::GpuTileReconstructNs);
    let (sum_buffer, lo_buffer) =
        residual_sum.expect("frame_count >= 1 guarantees at least one residual accumulated");
    let residual_len = residual_dims.0 * residual_dims.1 * spp;
    let counts = batch.retain(upload_u32(device, queue, &residual_counts));
    select::mean_by_count(
        &mut batch,
        pipelines,
        &sum_buffer,
        &lo_buffer,
        &counts,
        residual_len,
        spp,
    );

    // Collapse: coarsest to finest, `A_k = L_k + expand(A_{k+1})`.
    let mut current_buffer = sum_buffer;
    let mut current_width = residual_dims.0;
    let mut current_height = residual_dims.1;
    let mut winner_buffer: Option<(wgpu::Buffer, u32, u32)> = None;
    for k in (0..levels as usize).rev() {
        let slot = best_levels[k]
            .take()
            .expect("every level 0..levels was populated by at least frame 0");
        if k == 0 {
            winner_buffer = Some((slot.best, slot.width, slot.height));
        }
        let expanded = pyramid::expand(
            &mut batch,
            pipelines,
            &current_buffer,
            current_width,
            current_height,
            spp,
            slot.width,
            slot.height,
        );
        let combined_len = slot.width * slot.height * spp;
        current_buffer = pyramid::add(&mut batch, pipelines, &slot.level, &expanded, combined_len);
        current_width = slot.width;
        current_height = slot.height;
    }

    batch.submit();

    let sample_count = current_width as usize * current_height as usize * spp as usize;
    let word_count = (packed_len(sample_count) / 4) as u32;
    let packed_out = convert::plane_to_u16(
        device,
        queue,
        pipelines,
        &current_buffer,
        word_count,
        sample_count as u32,
    );
    drop(reconstruct);

    let readback = gpu_tile_stage(TimingCounter::GpuTileReadbackNs);
    let packed_bytes = download_buffer(device, queue, &packed_out, (word_count as u64) * 4)?;
    let samples = unpack_samples(&packed_bytes, sample_count);

    let (winner_best, ww, wh) = winner_buffer.expect("levels >= 1 guarantees a level-0 winner");
    let winner_count = ww as usize * wh as usize;
    let raw = download_buffer(device, queue, &winner_best, (winner_count * 8) as u64)?;
    let mut zmap = vec![0u16; winner_count];
    for (i, chunk) in raw.chunks_exact(8).enumerate() {
        let winner = u32::from_le_bytes(chunk[4..8].try_into().expect("4 bytes"));
        zmap[i] = winner as u16;
    }
    drop(readback);

    validate_tile(&zmap, frame_count as u32).map_err(|source| AlgorithmError::Failed {
        algorithm: "laplacian-gpu".to_string(),
        detail: source.to_string(),
    })?;

    Ok(TileOutput {
        tile: Tile {
            rect,
            samples_per_pixel: spp,
            samples,
        },
        zmap,
    })
}

/// Fuses `rect` across `frame_count` frames of `reader` at pyramid depth
/// `levels`, entirely on the GPU, returning the composite tile and the
/// Z-Map -- the WGSL twin of
/// `focusforge_core::fuse::laplacian::Laplacian::fuse`. Reads and uploads
/// every frame directly. `coverage[i]` is frame `i`'s coverage mask over
/// `rect` with `TileCtx::for_each_frame` semantics: missing or `None` means
/// every pixel is covered; [`crate::stack_tile::warp_fuse_tile_gpu`] is the
/// warp-then-fuse twin that keeps a warped frame resident instead.
#[allow(clippy::too_many_arguments)]
pub fn fuse_tile_gpu(
    shared: &SharedDevice,
    reader: &dyn TileReader,
    pipelines: &FusionPipelines,
    frame_count: usize,
    rect: Rect,
    samples_per_pixel: u32,
    levels: u32,
    coverage: &[Option<Vec<bool>>],
) -> Result<TileOutput, AlgorithmError> {
    let device = shared.device();
    let queue = shared.queue();
    let pixel_count = rect.width as usize * rect.height as usize;

    let mut source = |frame: usize| -> Result<FrameSource, AlgorithmError> {
        let mut samples = vec![0u16; pixel_count * samples_per_pixel as usize];
        {
            let _stage = gpu_tile_stage(TimingCounter::GpuTileReadNs);
            reader.read_tile_into(frame, rect, &mut samples)?;
        }
        let tile = Tile {
            rect,
            samples_per_pixel,
            samples,
        };
        let covered = coverage.get(frame).cloned().flatten();
        Ok((upload_plane(device, queue, pipelines, &tile), covered))
    };

    fuse_planes_gpu(
        shared,
        pipelines,
        frame_count,
        rect,
        samples_per_pixel,
        levels,
        &mut source,
    )
}
