// SPDX-License-Identifier: GPL-3.0-or-later
//! GPU prototype of the three sharpness stages: detail level 0 through the
//! product's pyramid kernels, the product's score kernel, then the peak, bin
//! and map kernels of this package.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use focusforge_core::algorithm::TileReader;
use focusforge_core::tile::{PlannedTile, Rect, TilePlan};
use focusforge_gpu::device::SharedDevice;
use focusforge_gpu::fuse::convert::plane_from_u16;
use focusforge_gpu::fuse::pyramid::{expand, reduce, sub};
use focusforge_gpu::fuse::{FusionPipelines, TileBatch};
use rayon::prelude::*;
use wgpu::util::DeviceExt;

pub const SCORE_RADIUS: u32 = 4;
pub const BIN_SHIFT: u32 = 12;
pub const BINS: usize = ((f32::MAX.to_bits() >> BIN_SHIFT) + 1) as usize;

const SCORE_WGSL: &str = include_str!("../../../crates/gpu/src/fuse/score.wgsl");
const FIXED_WGSL: &str = include_str!("score_fixed.wgsl");
const PEAK_WGSL: &str = include_str!("peak.wgsl");
const MAP_WGSL: &str = include_str!("map.wgsl");

/// Host time summed over all worker threads, by sub-stage.
#[derive(Default)]
pub struct Times {
    pub read: AtomicU64,
    pub upload: AtomicU64,
    pub compute: AtomicU64,
    pub readback: AtomicU64,
    pub wait: AtomicU64,
    pub host: AtomicU64,
    /// Edge re-check: candidate selection plus the exact CPU recompute.
    pub recheck: AtomicU64,
}

impl Times {
    pub fn add(slot: &AtomicU64, since: Instant) {
        slot.fetch_add(since.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    pub fn reset(&self) {
        for slot in [
            &self.read,
            &self.upload,
            &self.compute,
            &self.readback,
            &self.wait,
            &self.host,
            &self.recheck,
        ] {
            slot.store(0, Ordering::Relaxed);
        }
    }

    /// Milliseconds per sub-stage as `read upload compute readback wait host recheck`.
    pub fn line(&self) -> String {
        let ms = |slot: &AtomicU64| {
            Duration::from_nanos(slot.load(Ordering::Relaxed)).as_secs_f64() * 1e3
        };
        format!(
            "read_ms={:.1} upload_ms={:.1} compute_ms={:.1} readback_ms={:.1} wait_ms={:.1} host_ms={:.1} recheck_ms={:.1}",
            ms(&self.read),
            ms(&self.upload),
            ms(&self.compute),
            ms(&self.readback),
            ms(&self.wait),
            ms(&self.host),
            ms(&self.recheck)
        )
    }
}

pub struct Gpu {
    pub dev: &'static SharedDevice,
    pub times: Times,
    /// Scores with the fixed-point kernel instead of the product's `f32` one.
    pub fixed: AtomicBool,
    pipes: FusionPipelines,
    rows: OnceLock<wgpu::ComputePipeline>,
    cols: OnceLock<wgpu::ComputePipeline>,
    rows_fx: OnceLock<wgpu::ComputePipeline>,
    cols_fx: OnceLock<wgpu::ComputePipeline>,
    fold: OnceLock<wgpu::ComputePipeline>,
    bin: OnceLock<wgpu::ComputePipeline>,
    map: OnceLock<wgpu::ComputePipeline>,
    workers: Mutex<Vec<Worker>>,
}

fn pipeline(device: &wgpu::Device, source: &str, entry: &str) -> wgpu::ComputePipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(entry),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(entry),
        layout: None,
        module: &module,
        entry_point: Some(entry),
        compilation_options: Default::default(),
        cache: None,
    })
}

impl Gpu {
    pub fn new(dev: &'static SharedDevice) -> &'static Gpu {
        Box::leak(Box::new(Gpu {
            dev,
            times: Times::default(),
            fixed: AtomicBool::new(false),
            pipes: FusionPipelines::new(),
            rows: OnceLock::new(),
            cols: OnceLock::new(),
            rows_fx: OnceLock::new(),
            cols_fx: OnceLock::new(),
            fold: OnceLock::new(),
            bin: OnceLock::new(),
            map: OnceLock::new(),
            workers: Mutex::new(Vec::new()),
        }))
    }

    fn get<'a>(
        &self,
        cell: &'a OnceLock<wgpu::ComputePipeline>,
        source: &str,
        entry: &str,
    ) -> &'a wgpu::ComputePipeline {
        cell.get_or_init(|| pipeline(self.dev.device(), source, entry))
    }

    fn uniform(&self, bytes: &[u8]) -> wgpu::Buffer {
        self.dev
            .device()
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("sharpness-gpu-params"),
                contents: bytes,
                usage: wgpu::BufferUsages::UNIFORM,
            })
    }

    fn bind(
        &self,
        pipe: &wgpu::ComputePipeline,
        entries: &[(u32, &wgpu::Buffer)],
    ) -> wgpu::BindGroup {
        let entries: Vec<_> = entries
            .iter()
            .map(|(binding, buffer)| wgpu::BindGroupEntry {
                binding: *binding,
                resource: buffer.as_entire_binding(),
            })
            .collect();
        self.dev
            .device()
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &pipe.get_bind_group_layout(0),
                entries: &entries,
            })
    }

    fn storage(&self, bytes: u64) -> wgpu::Buffer {
        self.dev.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("sharpness-gpu-storage"),
            size: bytes,
            usage: wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::COPY_DST
                | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        })
    }

    fn take_worker(&'static self) -> Worker {
        let pooled = self.workers.lock().unwrap().pop();
        pooled.unwrap_or_else(|| Worker::new(self))
    }

    fn give_worker(&self, worker: Worker) {
        self.workers.lock().unwrap().push(worker);
    }

    /// One frame tile through a pooled worker, for diagnostics.
    pub fn probe(
        &'static self,
        reader: &dyn TileReader,
        frame: usize,
        read: Rect,
        spp: u32,
        tail: &Tail,
    ) -> Result<Vec<f32>, String> {
        let mut worker = self.take_worker();
        let out = worker.run(reader, frame, read, spp, tail);
        self.give_worker(worker);
        out
    }

    /// Submits `encoder`, optionally copying `copy`'s bytes to a staging
    /// buffer, waits for exactly this submission and returns the staged
    /// `f32`s. Time goes into compute, wait and readback.
    fn finish(
        &self,
        mut encoder: wgpu::CommandEncoder,
        copy: Option<(&wgpu::Buffer, u64)>,
    ) -> Vec<f32> {
        let device = self.dev.device();
        let t = Instant::now();
        let staging = copy.map(|(src, bytes)| {
            let staging = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("sharpness-gpu-staging"),
                size: bytes,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            encoder.copy_buffer_to_buffer(src, 0, &staging, 0, bytes);
            staging
        });
        let submission = self.dev.queue().submit(Some(encoder.finish()));
        Times::add(&self.times.compute, t);
        let t = Instant::now();
        device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: None,
            })
            .expect("device.poll failed while waiting for a submission");
        Times::add(&self.times.wait, t);
        let Some(staging) = staging else {
            return Vec::new();
        };
        let t = Instant::now();
        let (tx, rx) = std::sync::mpsc::channel();
        staging
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
        loop {
            device
                .poll(wgpu::PollType::Poll)
                .expect("device.poll failed while mapping");
            match rx.try_recv() {
                Ok(result) => {
                    result.expect("failed to map the staging buffer");
                    break;
                }
                Err(_) => std::thread::yield_now(),
            }
        }
        let out = bytemuck::cast_slice::<u8, f32>(&staging.slice(..).get_mapped_range()).to_vec();
        staging.unmap();
        Times::add(&self.times.readback, t);
        out
    }

    fn dispatch(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        pipe: &wgpu::ComputePipeline,
        group: &wgpu::BindGroup,
        invocations: u32,
    ) {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: None,
            timestamp_writes: None,
        });
        pass.set_pipeline(pipe);
        pass.set_bind_group(0, group, &[]);
        pass.dispatch_workgroups(invocations.div_ceil(64), 1, 1);
    }

    /// Bytes of device buffers one worker holds for a `width` x `height` read rect.
    pub fn bytes_per_worker(width: u32, height: u32, spp: u32) -> u64 {
        let (w, h, s) = (u64::from(width), u64::from(height), u64::from(spp));
        let (hw, hh) = (w.div_ceil(2), h.div_ceil(2));
        let samples = w * h * s * 4;
        // packed u16, base, reduce-h, reduce-v, expand-h, expand-v, detail, rows, scores,
        // interior map, staging
        samples / 2 + samples + hw * h * s * 4 + hw * hh * s * 4 + w * h * s * 4 * 3 + w * h * 4 * 4
    }

    /// An all-zero peaks buffer for a read rect, cleared again per tile.
    pub fn peaks_buffer(&self, read: Rect) -> wgpu::Buffer {
        self.storage(u64::from(read.width) * u64::from(read.height) * 4)
    }

    pub fn clear(&self, buffer: &wgpu::Buffer) {
        let mut encoder = self
            .dev
            .device()
            .create_command_encoder(&Default::default());
        encoder.clear_buffer(buffer, 0, None);
        self.dev.queue().submit(Some(encoder.finish()));
    }

    /// Per-pixel peaks of the read rect, read back.
    pub fn download_peaks(&self, peaks: &wgpu::Buffer, read: Rect) -> Vec<f32> {
        let encoder = self
            .dev
            .device()
            .create_command_encoder(&Default::default());
        let bytes = u64::from(read.width) * u64::from(read.height) * 4;
        self.finish(encoder, Some((peaks, bytes)))
    }

    /// Bins the interior peaks of `peaks` into `hist` on the device.
    pub fn bin_peaks(&self, peaks: &wgpu::Buffer, hist: &wgpu::Buffer, read: Rect, interior: Rect) {
        let params = self.uniform(&peak_params(read, interior));
        let pipe = self.get(&self.bin, PEAK_WGSL, "bin");
        let group = self.bind(pipe, &[(1, peaks), (2, &params), (3, hist)]);
        let mut encoder = self
            .dev
            .device()
            .create_command_encoder(&Default::default());
        self.dispatch(&mut encoder, pipe, &group, interior.width * interior.height);
        self.finish(encoder, None);
    }

    pub fn hist_buffer(&self) -> wgpu::Buffer {
        self.storage(BINS as u64 * 4)
    }

    pub fn download_hist(&self, hist: &wgpu::Buffer) -> Vec<u64> {
        let encoder = self
            .dev
            .device()
            .create_command_encoder(&Default::default());
        let words = self.finish(encoder, Some((hist, BINS as u64 * 4)));
        words.iter().map(|w| u64::from(w.to_bits())).collect()
    }
}

fn peak_params(read: Rect, interior: Rect) -> [u8; 32] {
    let words = [
        read.width,
        read.height,
        interior.x - read.x,
        interior.y - read.y,
        interior.width,
        interior.height,
        0,
        0,
    ];
    bytemuck::cast(words)
}

/// What a worker does with a frame tile's scores.
pub enum Tail<'a> {
    /// `atomicMax` them into the tile's peaks buffer; nothing comes back.
    Fold { peaks: &'a wgpu::Buffer },
    /// Divide the interior by `scale` and read the map back.
    Map { scale: f32, interior: Rect },
    /// Read the whole read rect's scores back.
    Scores,
    /// Read the detail plane back.
    Detail,
}

#[derive(Clone)]
struct Buffers {
    dims: (u32, u32, u32),
    packed: wgpu::Buffer,
    rows: wgpu::Buffer,
    scores: wgpu::Buffer,
    map: wgpu::Buffer,
}

/// One thread's reusable read buffer, device buffers and tile batch.
pub struct Worker {
    gpu: &'static Gpu,
    batch: TileBatch<'static>,
    samples: Vec<u16>,
    buffers: Option<Buffers>,
    params: HashMap<Vec<u8>, wgpu::Buffer>,
}

impl Worker {
    fn new(gpu: &'static Gpu) -> Worker {
        Worker {
            gpu,
            batch: TileBatch::new(gpu.dev.device(), gpu.dev.queue()),
            samples: Vec::new(),
            buffers: None,
            params: HashMap::new(),
        }
    }

    fn params(&mut self, bytes: &[u8]) -> wgpu::Buffer {
        self.params
            .entry(bytes.to_vec())
            .or_insert_with(|| self.gpu.uniform(bytes))
            .clone()
    }

    fn buffers(&mut self, read: Rect, spp: u32) -> Buffers {
        let dims = (read.width, read.height, spp);
        if let Some(buffers) = self.buffers.as_ref().filter(|b| b.dims == dims) {
            return buffers.clone();
        }
        let pixels = u64::from(read.width) * u64::from(read.height);
        let samples = pixels * u64::from(spp);
        let buffers = Buffers {
            dims,
            packed: self.gpu.storage((samples + 1) / 2 * 4),
            rows: self.gpu.storage(pixels * 8),
            scores: self.gpu.storage(pixels * 4),
            map: self.gpu.storage(pixels * 4),
        };
        self.buffers = Some(buffers.clone());
        buffers
    }

    /// Reads frame `frame` of `read`, scores it on the device and applies `tail`.
    pub fn run(
        &mut self,
        reader: &dyn TileReader,
        frame: usize,
        read: Rect,
        spp: u32,
        tail: &Tail,
    ) -> Result<Vec<f32>, String> {
        let gpu = self.gpu;
        let (device, queue) = (gpu.dev.device(), gpu.dev.queue());
        let (w, h) = (read.width, read.height);
        let count = (w as usize) * (h as usize) * spp as usize;

        let t = Instant::now();
        self.samples.resize(count + (count & 1), 0);
        reader
            .read_tile_into(frame, read, &mut self.samples[..count])
            .map_err(|e| e.to_string())?;
        Times::add(&gpu.times.read, t);

        let t = Instant::now();
        let buffers = self.buffers(read, spp);
        queue.write_buffer(&buffers.packed, 0, bytemuck::cast_slice(&self.samples));
        Times::add(&gpu.times.upload, t);

        let t = Instant::now();
        let words = self.samples.len() as u32 / 2;
        let base = plane_from_u16(
            device,
            queue,
            &gpu.pipes,
            &buffers.packed,
            words,
            count as u32,
        );
        let (g1, w1, h1) = reduce(&mut self.batch, &gpu.pipes, &base, w, h, spp);
        let blurred = expand(&mut self.batch, &gpu.pipes, &g1, w1, h1, spp, w, h);
        let detail = sub(&mut self.batch, &gpu.pipes, &base, &blurred, count as u32);
        self.batch.submit();

        let score_params = self.params(bytemuck::cast_slice(&[w, h, spp, SCORE_RADIUS]));
        let (rows, cols, rows_group, cols_group) = if gpu.fixed.load(Ordering::Relaxed) {
            let rows = gpu.get(&gpu.rows_fx, FIXED_WGSL, "rows");
            let cols = gpu.get(&gpu.cols_fx, FIXED_WGSL, "cols");
            let rows_group = gpu.bind(
                rows,
                &[(0, &detail), (1, &buffers.rows), (2, &score_params)],
            );
            let cols_group = gpu.bind(
                cols,
                &[(1, &buffers.rows), (2, &score_params), (3, &buffers.scores)],
            );
            (rows, cols, rows_group, cols_group)
        } else {
            let rows = gpu.get(&gpu.rows, SCORE_WGSL, "rows");
            let cols = gpu.get(&gpu.cols, SCORE_WGSL, "cols");
            let rows_group = gpu.bind(
                rows,
                &[(0, &detail), (1, &buffers.rows), (2, &score_params)],
            );
            let cols_group = gpu.bind(
                cols,
                &[(0, &buffers.rows), (1, &buffers.scores), (2, &score_params)],
            );
            (rows, cols, rows_group, cols_group)
        };
        let mut encoder = device.create_command_encoder(&Default::default());
        gpu.dispatch(&mut encoder, rows, &rows_group, w * h);
        gpu.dispatch(&mut encoder, cols, &cols_group, w * h);
        let pixels_bytes = u64::from(w) * u64::from(h) * 4;
        let out = match tail {
            Tail::Fold { peaks } => {
                let params = self.params(&peak_params(read, read));
                let fold = gpu.get(&gpu.fold, PEAK_WGSL, "fold");
                let group = gpu.bind(fold, &[(0, &buffers.scores), (1, peaks), (2, &params)]);
                gpu.dispatch(&mut encoder, fold, &group, w * h);
                Times::add(&gpu.times.compute, t);
                gpu.finish(encoder, None)
            }
            Tail::Map { scale, interior } => {
                let mut bytes: [u32; 8] = [
                    w,
                    interior.x - read.x,
                    interior.y - read.y,
                    interior.width,
                    interior.height,
                    0,
                    0,
                    0,
                ];
                bytes[5] = scale.to_bits();
                let params = self.params(bytemuck::cast_slice(&bytes));
                let map = gpu.get(&gpu.map, MAP_WGSL, "main");
                let group = gpu.bind(
                    map,
                    &[(0, &buffers.scores), (1, &buffers.map), (2, &params)],
                );
                gpu.dispatch(&mut encoder, map, &group, interior.width * interior.height);
                Times::add(&gpu.times.compute, t);
                let bytes = u64::from(interior.width) * u64::from(interior.height) * 4;
                gpu.finish(encoder, Some((&buffers.map, bytes)))
            }
            Tail::Scores => {
                Times::add(&gpu.times.compute, t);
                gpu.finish(encoder, Some((&buffers.scores, pixels_bytes)))
            }
            Tail::Detail => {
                Times::add(&gpu.times.compute, t);
                gpu.finish(
                    encoder,
                    Some((&detail, u64::from(w) * u64::from(h) * u64::from(spp) * 4)),
                )
            }
        };
        Ok(out)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Peaks read back per tile, binned on the host.
    HostBin,
    /// Peaks binned with atomics on the device, one histogram read back at the end.
    DeviceBin,
}

/// Runs `body` for every frame of one tile in parallel, each on a pooled worker.
fn frames_of_tile<A: Send>(
    gpu: &'static Gpu,
    pool: &rayon::ThreadPool,
    rows: &mut [A],
    body: impl Fn(&mut Worker, usize, &mut A) -> Result<(), String> + Sync,
) -> Result<(), String> {
    pool.install(|| {
        rows.par_iter_mut()
            .enumerate()
            .try_for_each(|(frame, row)| {
                let mut worker = gpu.take_worker();
                let result = body(&mut worker, frame, row);
                gpu.give_worker(worker);
                result
            })
    })
}

/// The interior rows of a read rect's peaks, row-major.
pub fn interior_peaks(all: &[f32], read: Rect, interior: Rect) -> Vec<f32> {
    let mut inner = Vec::with_capacity(interior.width as usize * interior.height as usize);
    for y in interior.y..interior.y + interior.height {
        let start = ((y - read.y) * read.width + (interior.x - read.x)) as usize;
        inner.extend_from_slice(&all[start..start + interior.width as usize]);
    }
    inner
}

/// Stage (a): histogram of per-pixel peaks over `frames` frames. `collect`
/// receives each tile's interior peaks (host binning only). `keep` receives
/// each tile's peaks buffer in plan order instead of dropping it (device
/// binning only).
#[allow(clippy::too_many_arguments)]
pub fn stage_a(
    gpu: &'static Gpu,
    reader: &dyn TileReader,
    plan: &TilePlan,
    frames: usize,
    spp: u32,
    pool: &rayon::ThreadPool,
    variant: Variant,
    mut collect: Option<&mut dyn FnMut(&PlannedTile, &[f32])>,
    mut keep: Option<&mut Vec<wgpu::Buffer>>,
) -> Result<Vec<u64>, String> {
    let mut hist = vec![0u64; BINS];
    let device_hist = (variant == Variant::DeviceBin).then(|| gpu.hist_buffer());
    for tile in plan.tiles() {
        let (read, interior) = (tile.read(), tile.interior());
        let peaks = gpu.peaks_buffer(read);
        gpu.clear(&peaks);
        frames_of_tile(gpu, pool, &mut vec![(); frames], |worker, frame, ()| {
            worker.run(reader, frame, read, spp, &Tail::Fold { peaks: &peaks })?;
            Ok(())
        })?;
        match &device_hist {
            Some(device_hist) => gpu.bin_peaks(&peaks, device_hist, read, interior),
            None => {
                let all = gpu.download_peaks(&peaks, read);
                let t = Instant::now();
                let inner = interior_peaks(&all, read, interior);
                for peak in &inner {
                    hist[(peak.to_bits() >> BIN_SHIFT) as usize] += 1;
                }
                if let Some(collect) = collect.as_deref_mut() {
                    collect(tile, &inner);
                }
                Times::add(&gpu.times.host, t);
            }
        }
        if let Some(keep) = keep.as_deref_mut() {
            keep.push(peaks);
        }
    }
    if let Some(device_hist) = device_hist {
        hist = gpu.download_hist(&device_hist);
    }
    Ok(hist)
}

/// Stage (b): the map of every frame of every tile, handed to `sink`.
#[allow(clippy::too_many_arguments)]
pub fn stage_b(
    gpu: &'static Gpu,
    reader: &dyn TileReader,
    plan: &TilePlan,
    frames: usize,
    spp: u32,
    scale: f32,
    pool: &rayon::ThreadPool,
    sink: &(dyn Fn(usize, &PlannedTile, &[f32]) + Sync),
) -> Result<(), String> {
    for tile in plan.tiles() {
        let (read, interior) = (tile.read(), tile.interior());
        frames_of_tile(gpu, pool, &mut vec![(); frames], |worker, frame, ()| {
            let map = worker.run(reader, frame, read, spp, &Tail::Map { scale, interior })?;
            sink(frame, tile, &map);
            Ok(())
        })?;
    }
    Ok(())
}

/// Stage (c): per-frame, per-cell `f64` score sums from the device scores, in
/// plan order as the CPU count accumulates them.
pub fn stage_c(
    gpu: &'static Gpu,
    reader: &dyn TileReader,
    plan: &TilePlan,
    frames: usize,
    spp: u32,
    cell: u32,
    columns: usize,
    cells: usize,
    pool: &rayon::ThreadPool,
) -> Result<Vec<Vec<f64>>, String> {
    let mut sums = vec![vec![0.0f64; cells]; frames];
    for tile in plan.tiles() {
        let (read, interior) = (tile.read(), tile.interior());
        frames_of_tile(gpu, pool, &mut sums, |worker, frame, frame_sums| {
            let scores = worker.run(reader, frame, read, spp, &Tail::Scores)?;
            let t = Instant::now();
            for y in interior.y..interior.y + interior.height {
                let row = ((y - read.y) * read.width) as usize;
                let cell_row = (y / cell) as usize * columns;
                for x in interior.x..interior.x + interior.width {
                    frame_sums[cell_row + (x / cell) as usize] +=
                        f64::from(scores[row + (x - read.x) as usize]);
                }
            }
            Times::add(&gpu.times.host, t);
            Ok(())
        })?;
    }
    Ok(sums)
}
