// SPDX-License-Identifier: GPL-3.0-or-later

//! The packed-`u32` upload/readback round trip, bit-identical for both an
//! even and an odd sample count.

use focusforge_core::algorithm::TileReader;
use focusforge_core::io::TiffStack;
use focusforge_core::tile::{Rect, Tile};
use focusforge_gpu::device::shared_device_or_skip;
use focusforge_gpu::pack::{packed_len, unpack_samples};
use focusforge_gpu::transfer::{download_buffer, upload_tile};

use crate::common;

#[test]
fn tile_roundtrip_is_bit_identical() {
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    let device = shared.device();
    let queue = shared.queue();

    // Case 1: a full 512x512x3 frame tile, read through the real fixture
    // reader.
    let stack = TiffStack::open(common::mantis_512_dir()).expect("open mantis-512 fixture");
    let spp = stack.header(0).samples_per_pixel;
    let rect = Rect {
        x: 0,
        y: 0,
        width: 512,
        height: 512,
    };
    let mut samples = vec![0u16; (rect.width * rect.height * spp) as usize];
    let reader: &dyn TileReader = &stack;
    reader
        .read_tile_into(0, rect, &mut samples)
        .expect("read fixture tile");
    assert_roundtrip(device, queue, spp, rect, &samples);

    // Case 2: a 5-sample tile whose packed length is 12 bytes -- a multiple
    // of 4, not of 8, exercising the odd trailing `u16` `map_async` still
    // allows.
    let odd_rect = Rect {
        x: 0,
        y: 0,
        width: 5,
        height: 1,
    };
    let odd_samples: Vec<u16> = vec![1, 2, 3, 4, 65535];
    assert_eq!(packed_len(odd_samples.len()), 12);
    assert_roundtrip(device, queue, 1, odd_rect, &odd_samples);
}

fn assert_roundtrip(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    samples_per_pixel: u32,
    rect: Rect,
    samples: &[u16],
) {
    let tile = Tile {
        rect,
        samples_per_pixel,
        samples: samples.to_vec(),
    };

    let buffer = upload_tile(device, queue, &tile);
    let byte_len = packed_len(tile.samples.len()) as u64;
    let bytes = download_buffer(device, queue, &buffer, byte_len).expect("download_buffer");
    let round_tripped = unpack_samples(&bytes, tile.samples.len());

    assert_eq!(
        round_tripped, samples,
        "packed-u32 storage-buffer round trip must be bit-identical"
    );
}
