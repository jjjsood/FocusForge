// SPDX-License-Identifier: GPL-3.0-or-later

//! The whole-tile pipeline: warp every frame's contribution to `rect` on the
//! GPU and fuse the result, with every intermediate kept on the device.
//! No per-frame readback: the two `download_buffer` calls at the tail of
//! `crate::fuse::fuse_planes_gpu` (composite, Z-Map) are the only reads
//! back to the host on this path.

use focusforge_core::algorithm::{AlgorithmError, TileOutput, TileReader};
use focusforge_core::align::Transform;
use focusforge_core::tile::{Rect, SampleFormat, Tile};
use focusforge_core::timing::{TimingCounter, gpu_tile_stage};

use crate::device::SharedDevice;
use crate::fuse::{
    FrameSource, FusedSample, FusionPipelines, fuse_planes_gpu, plane_from_f32, plane_from_packed,
};
use crate::transfer::{upload_f32, upload_tile};
use crate::warp::{WarpPipelines, warp_tile_dispatch};

/// Reads `rect` of `frame_index` as `f32` and uploads it unchanged: an
/// identity frame's base plane on the `f32` path.
pub fn upload_frame_f32(
    shared: &SharedDevice,
    reader: &dyn TileReader,
    frame_index: usize,
    rect: Rect,
    samples_per_pixel: u32,
) -> Result<wgpu::Buffer, AlgorithmError> {
    let mut samples =
        vec![0f32; rect.width as usize * rect.height as usize * samples_per_pixel as usize];
    {
        let _stage = gpu_tile_stage(TimingCounter::GpuTileReadNs);
        reader.read_tile_f32_into(frame_index, rect, &mut samples)?;
    }
    Ok(upload_f32(shared.device(), shared.queue(), &samples))
}

/// Drives the whole tile through both stages in one call. Takes no budget:
/// the caller holds a `VramBudget` permit around it.
///
/// `transforms[i]` is frame `i`'s reference-to-frame transform, the same
/// index-is-frame convention [`focusforge_core::align::WarpingReader`]
/// already carries. An identity entry is the identical short-circuit "read
/// `rect` directly" -- no dispatch, just an upload and a
/// `plane_from_packed` join, exactly the same join a warped frame gets.
///
/// A reader whose `sample_format()` is `F32` is read and warped as `f32`:
/// the uploaded or warped buffer is the frame's base plane itself, with no
/// pack, no conversion kernel and no clamp. The fused tile is read back as
/// `O`: quantized to `u16` once at the end of fusion, or left `f32`.
///
/// A warped frame with uncovered pixels hands its coverage mask to fusion,
/// which excludes it where the Laplacian reach touches those pixels.
#[allow(clippy::too_many_arguments)]
pub fn warp_fuse_tile_gpu<O: FusedSample>(
    shared: &SharedDevice,
    warp_pipelines: &WarpPipelines,
    fuse_pipelines: &FusionPipelines,
    reader: &dyn TileReader,
    frame: (u32, u32),
    transforms: &[Transform],
    rect: Rect,
    samples_per_pixel: u32,
    levels: u32,
) -> Result<TileOutput<O>, AlgorithmError> {
    let device = shared.device();
    let queue = shared.queue();
    let frame_count = transforms.len();

    let format = reader.sample_format();
    let mut source = |i: usize| -> Result<FrameSource, AlgorithmError> {
        let t = &transforms[i];
        let (buffer, covered) = if *t == Transform::identity() {
            match format {
                SampleFormat::U16 => {
                    let mut samples =
                        vec![
                            0u16;
                            rect.width as usize * rect.height as usize * samples_per_pixel as usize
                        ];
                    {
                        let _stage = gpu_tile_stage(TimingCounter::GpuTileReadNs);
                        reader.read_tile_into(i, rect, &mut samples)?;
                    }
                    let tile = Tile {
                        rect,
                        samples_per_pixel,
                        samples,
                    };
                    (upload_tile(device, queue, &tile), None)
                }
                SampleFormat::F32 => (
                    upload_frame_f32(shared, reader, i, rect, samples_per_pixel)?,
                    None,
                ),
            }
        } else {
            let (buffer, covered) = warp_tile_dispatch(
                shared,
                warp_pipelines,
                reader,
                format,
                i,
                frame,
                samples_per_pixel,
                rect,
                t,
            )
            .map_err(|source| AlgorithmError::Failed {
                algorithm: "warp".to_string(),
                detail: source.to_string(),
            })?;
            (buffer, covered.contains(&false).then_some(covered))
        };
        let plane = match format {
            SampleFormat::U16 => plane_from_packed(
                device,
                queue,
                fuse_pipelines,
                &buffer,
                rect.width,
                rect.height,
                samples_per_pixel,
            ),
            SampleFormat::F32 => plane_from_f32(buffer, rect.width, rect.height, samples_per_pixel),
        };
        Ok((plane, covered))
    };

    fuse_planes_gpu::<O>(
        shared,
        fuse_pipelines,
        frame_count,
        rect,
        samples_per_pixel,
        levels,
        &mut source,
    )
}
