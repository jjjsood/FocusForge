// SPDX-License-Identifier: GPL-3.0-or-later

//! The whole-tile pipeline: warp every frame's contribution to `rect` on the
//! GPU and fuse the result, with every intermediate kept on the device.
//! No per-frame readback: the two `download_buffer` calls at the tail of
//! `crate::fuse::fuse_planes_gpu` (composite, Z-Map) are the only reads
//! back to the host on this path.

use focusforge_core::algorithm::{AlgorithmError, TileOutput, TileReader};
use focusforge_core::align::Transform;
use focusforge_core::tile::{Rect, Tile};
use focusforge_core::timing::{TimingCounter, gpu_tile_stage};

use crate::device::SharedDevice;
use crate::fuse::{FrameSource, FusionPipelines, fuse_planes_gpu, plane_from_packed};
use crate::transfer::upload_tile;
use crate::warp::{WarpPipelines, warp_tile_dispatch};

/// Drives the whole tile through both stages in one call. Takes no budget:
/// the caller holds a `VramBudget` permit around it.
///
/// `transforms[i]` is frame `i`'s reference-to-frame transform, the same
/// index-is-frame convention [`focusforge_core::align::WarpingReader`]
/// already carries. An identity entry is the identical short-circuit "read
/// `rect` directly" -- no dispatch, just an upload and a
/// `plane_from_packed` join, exactly the same join a warped frame gets.
///
/// A warped frame with uncovered pixels hands its coverage mask to fusion,
/// which excludes it where the Laplacian reach touches those pixels.
#[allow(clippy::too_many_arguments)]
pub fn warp_fuse_tile_gpu(
    shared: &SharedDevice,
    warp_pipelines: &WarpPipelines,
    fuse_pipelines: &FusionPipelines,
    reader: &dyn TileReader,
    frame: (u32, u32),
    transforms: &[Transform],
    rect: Rect,
    samples_per_pixel: u32,
    levels: u32,
) -> Result<TileOutput, AlgorithmError> {
    let device = shared.device();
    let queue = shared.queue();
    let frame_count = transforms.len();

    let mut source = |i: usize| -> Result<FrameSource, AlgorithmError> {
        let t = &transforms[i];
        let (packed, covered) = if *t == Transform::identity() {
            let mut samples =
                vec![0u16; rect.width as usize * rect.height as usize * samples_per_pixel as usize];
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
        } else {
            let (buffer, covered) = warp_tile_dispatch(
                shared,
                warp_pipelines,
                reader,
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
        let plane = plane_from_packed(
            device,
            queue,
            fuse_pipelines,
            &packed,
            rect.width,
            rect.height,
            samples_per_pixel,
        );
        Ok((plane, covered))
    };

    fuse_planes_gpu(
        shared,
        fuse_pipelines,
        frame_count,
        rect,
        samples_per_pixel,
        levels,
        &mut source,
    )
}
