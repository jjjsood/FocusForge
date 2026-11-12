// SPDX-License-Identifier: GPL-3.0-or-later

//! The focus score of one frame tile on the device: detail level 0 as
//! `base - expand(reduce(base))` through the fusion pyramid kernels, then the
//! windowed score. The CPU twin is `focusforge_core::sharp_areas`'s tile
//! score (`fuse::sharpness::detail_plane` then `fuse::laplacian::score_plane`).

use focusforge_core::algorithm::{AlgorithmError, TileReader};
use focusforge_core::sharp_areas::SCORE_RADIUS;
use focusforge_core::tile::Rect;

use crate::device::SharedDevice;
use crate::fuse::convert::plane_from_u16;
use crate::fuse::pyramid::{expand, reduce, sub};
use crate::fuse::select;
use crate::fuse::{FusionPipelines, TileBatch};
use crate::pack::{pack_samples, packed_len};
use crate::transfer::{download_f32, upload_storage};

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
    batch.submit();
    Ok(download_f32(
        gpu,
        queue,
        &scores,
        width as usize * height as usize,
    )?)
}
