// SPDX-License-Identifier: GPL-3.0-or-later

//! The focus score of one frame tile on the device: detail level 0 as
//! `base - expand(reduce(base))` through the fusion pyramid kernels, then the
//! windowed score. The CPU twin is `focusforge_core::sharp_areas`'s tile
//! score (`fuse::sharpness::detail_plane` then `fuse::laplacian::score_plane`).
//!
//! [`fold_tile`] maxes the score into a tile's retained peaks and [`bin_tile`]
//! counts those peaks into a histogram, both without a readback; their CPU
//! twins are `focusforge_core::sharpness_map::{fold_peaks, bin_peaks}`.

use focusforge_core::algorithm::{AlgorithmError, TileReader};
use focusforge_core::sharp_areas::SCORE_RADIUS;
use focusforge_core::tile::Rect;

use crate::device::{GpuError, SharedDevice};
use crate::fuse::convert::plane_from_u16;
use crate::fuse::pyramid::{expand, reduce, sub};
use crate::fuse::select;
use crate::fuse::{FusionPipelines, TileBatch};
use crate::pack::{pack_samples, packed_len};
use crate::transfer::{download_f32, upload_storage};

/// Records the score of `frame` over `read` into a new batch and returns the
/// batch with the buffer the scores land in.
fn score_chain<'a>(
    device: &'a SharedDevice,
    pipelines: &FusionPipelines,
    reader: &dyn TileReader,
    frame: usize,
    read: Rect,
    spp: u32,
) -> Result<(TileBatch<'a>, wgpu::Buffer), AlgorithmError> {
    let (gpu, queue) = (device.device(), device.queue());
    let (width, height) = (read.width, read.height);
    let count = width as usize * height as usize * spp as usize;
    let mut samples = vec![0u16; count];
    reader.read_tile_into(frame, read, &mut samples)?;

    let packed = upload_storage(
        gpu,
        queue,
        &pack_samples(&samples),
        "focusforge-gpu-sharpness-upload",
    );
    let base = plane_from_u16(
        gpu,
        queue,
        pipelines,
        &packed,
        (packed_len(count) / 4) as u32,
        count as u32,
    );
    let mut batch = TileBatch::new(gpu, queue);
    let (reduced, reduced_width, reduced_height) =
        reduce(&mut batch, pipelines, &base, width, height, spp);
    let blurred = expand(
        &mut batch,
        pipelines,
        &reduced,
        reduced_width,
        reduced_height,
        spp,
        width,
        height,
    );
    let detail = sub(&mut batch, pipelines, &base, &blurred, count as u32);
    let scores = select::score(
        &mut batch,
        pipelines,
        &detail,
        width,
        height,
        spp,
        SCORE_RADIUS,
    );
    Ok((batch, scores))
}

/// The focus score of `frame` over `read`, one value per pixel of `read`
/// (channels summed), read back from the device.
pub(crate) fn score_tile(
    device: &SharedDevice,
    pipelines: &FusionPipelines,
    reader: &dyn TileReader,
    frame: usize,
    read: Rect,
    spp: u32,
) -> Result<Vec<f32>, AlgorithmError> {
    let (mut batch, scores) = score_chain(device, pipelines, reader, frame, read, spp)?;
    batch.submit();
    Ok(download_f32(
        device.device(),
        device.queue(),
        &scores,
        read.width as usize * read.height as usize,
    )?)
}

/// Uniform contents of the peak kernels: the read rect's size and the
/// interior's origin and size relative to it.
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
    let mut bytes = [0u8; 32];
    for (chunk, word) in bytes.chunks_exact_mut(4).zip(words) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    bytes
}

fn bind(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

/// Maxes the score of `frame` over `read` into `peaks` (one `u32` of `f32`
/// bits per pixel of `read`) and returns once the device has finished.
pub(crate) fn fold_tile(
    device: &SharedDevice,
    pipelines: &FusionPipelines,
    reader: &dyn TileReader,
    frame: usize,
    read: Rect,
    spp: u32,
    peaks: &wgpu::Buffer,
) -> Result<(), AlgorithmError> {
    let (mut batch, scores) = score_chain(device, pipelines, reader, frame, read, spp)?;
    let gpu = device.device();
    let params = batch.params(&peak_params(read, read));
    batch.record(
        pipelines.peak_fold(gpu),
        &[bind(0, &scores), bind(1, peaks), bind(2, &params)],
        read.width * read.height,
        "focusforge-gpu-sharpness-peak-fold-pass",
    );
    batch.submit();
    // The batch's buffers live until the device is done with them, so the
    // wait bounds the device memory the frame workers hold.
    gpu.poll(wgpu::PollType::wait_indefinitely())
        .map_err(|err| GpuError::Readback {
            detail: format!("device.poll failed while waiting for the peak fold: {err}"),
        })?;
    Ok(())
}

/// Records the count of `interior`'s peaks, inside `peaks` over `read`, into
/// `histogram` (`BINS` `u32` counters).
pub(crate) fn bin_tile(
    batch: &mut TileBatch,
    pipelines: &FusionPipelines,
    peaks: &wgpu::Buffer,
    histogram: &wgpu::Buffer,
    read: Rect,
    interior: Rect,
) {
    let params = batch.params(&peak_params(read, interior));
    batch.record(
        pipelines.peak_bin(batch.device()),
        &[bind(1, peaks), bind(2, &params), bind(3, histogram)],
        interior.width * interior.height,
        "focusforge-gpu-sharpness-peak-bin-pass",
    );
}
