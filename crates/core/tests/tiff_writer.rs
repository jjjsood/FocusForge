// SPDX-License-Identifier: GPL-3.0-or-later

//! Fixture round trip through `TiffWriter`, checked against the reader and
//! against two independent oracles (`tools/tiff-diff.py` for pixels,
//! `tools/lib/rawtiff.py` for layout). Unit-level writer tests (header
//! layout, `write_tile` validation, `finish`) live in
//! `crates/core/src/io/tiff/writer.rs`'s own `mod tests` instead.

mod common;

use std::path::PathBuf;

use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter};

fn mantis_512_dir() -> PathBuf {
    common::fixtures_dir().join("mantis-512")
}

/// Read frame 0 of `fixtures/mantis-512` in 200px tiles (below the 512px
/// crop) and write it back out through `TiffWriter`. Returns the written
/// path.
fn round_trip_frame_0() -> PathBuf {
    let stack = TiffStack::open(mantis_512_dir()).expect("mantis-512 must open");
    let (width, height) = stack.dimensions();

    let out_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("tiff_writer_round_trip.tif");
    let mut writer = TiffWriter::create(
        &out_path,
        TiffSpec {
            width,
            height,
            samples_per_pixel: 3,
            description: None,
        },
    )
    .expect("create must succeed");

    let tile_size = 200u32;
    let mut y = 0u32;
    while y < height {
        let h = tile_size.min(height - y);
        let mut x = 0u32;
        while x < width {
            let w = tile_size.min(width - x);
            let rect = Rect {
                x,
                y,
                width: w,
                height: h,
            };
            let tile = stack.read_tile(0, rect).expect("tile read must succeed");
            writer
                .write_tile(rect, &tile.samples)
                .expect("write_tile must succeed");
            x += w;
        }
        y += h;
    }
    writer
        .finish()
        .expect("finish must succeed: frame is fully covered");
    out_path
}

#[test]
fn round_trip() {
    let out_path = round_trip_frame_0();

    // (a) reopen through TiffStack and compare samples against the reader.
    let original = TiffStack::open(mantis_512_dir()).expect("mantis-512 must open");
    let written = TiffStack::open_files(std::slice::from_ref(&out_path))
        .expect("written file must reopen through TiffStack");
    assert_eq!(written.dimensions(), original.dimensions());

    let full = Rect {
        x: 0,
        y: 0,
        width: 512,
        height: 512,
    };
    let original_tile = original
        .read_tile(0, full)
        .expect("original full-frame read");
    let written_tile = written.read_tile(0, full).expect("written full-frame read");
    assert_eq!(
        written_tile.samples, original_tile.samples,
        "round-tripped samples must equal the original frame's samples"
    );

    let frame_path = mantis_512_dir().join("frames").join("00.tif");

    // (b) an independent tool: tools/tiff-diff.py --tol 0.
    let tools_dir = common::workspace_root().join("tools");
    let diff_script = tools_dir.join("tiff-diff.py");
    match std::process::Command::new("python3")
        .arg(&diff_script)
        .arg(&out_path)
        .arg(&frame_path)
        .arg("--tol")
        .arg("0")
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            println!("{stdout}");
            assert!(
                output.status.success(),
                "tiff-diff.py --tol 0 must report exit 0: stdout={stdout} stderr={}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(_) => {
            eprintln!("skip: python3 not on PATH, skipping tiff-diff.py comparison");
        }
    }

    // (c) an independent tool: tools/lib/rawtiff.py's layout report.
    let rawtiff_script = tools_dir.join("lib").join("rawtiff.py");
    match std::process::Command::new("python3")
        .arg(&rawtiff_script)
        .arg(&out_path)
        .output()
    {
        Ok(output) => {
            assert!(
                output.status.success(),
                "rawtiff.py must parse the written file: stderr={}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains("512x512 3ch 16-bit II compression=1 planar=1 photometric=2"),
                "unexpected rawtiff.py layout line: {stdout}"
            );
            assert!(
                stdout.contains("rows_per_strip=1 strips=512"),
                "unexpected rawtiff.py strip line: {stdout}"
            );
        }
        Err(_) => {
            eprintln!("skip: python3 not on PATH, skipping rawtiff.py layout check");
        }
    }
}

// ---- float staged composites ----

use focusforge_core::algorithm::TileReader;
use focusforge_core::io::{StripCompression, StripSpec, StripWriter, TiffError};
use focusforge_core::tile::SampleFormat;

const F32_WIDTH: u32 = 37;
const F32_HEIGHT: u32 = 23;

fn f32_spec(spp: u32) -> TiffSpec {
    TiffSpec {
        width: F32_WIDTH,
        height: F32_HEIGHT,
        samples_per_pixel: spp,
        description: None,
    }
}

fn f32_rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// Distinct values above 65535, below 0 and fractional, so no sample can pass
/// by accident.
fn f32_samples(spp: u32) -> Vec<f32> {
    (0..F32_WIDTH * F32_HEIGHT * spp)
        .map(|i| match i % 4 {
            0 => 70_000.5 + i as f32,
            1 => -12.25 - i as f32,
            2 => i as f32 * 0.37,
            _ => 65_535.0 + i as f32 * 3.0,
        })
        .collect()
}

fn scratch(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// Writes `rows` of `samples` through `writer` in 16x8 tiles.
fn write_f32_rows(
    writer: &mut TiffWriter<std::fs::File>,
    spp: u32,
    samples: &[f32],
    rows: std::ops::Range<u32>,
) {
    for y in rows.clone().step_by(8) {
        let h = 8.min(rows.end - y);
        for x in (0..F32_WIDTH).step_by(16) {
            let w = 16.min(F32_WIDTH - x);
            let mut tile = Vec::new();
            for row in y..y + h {
                let start = ((row * F32_WIDTH + x) * spp) as usize;
                tile.extend_from_slice(&samples[start..start + (w * spp) as usize]);
            }
            writer
                .write_tile_f32(f32_rect(x, y, w, h), &tile)
                .expect("write_tile_f32");
        }
    }
}

fn read_all_f32(stack: &TiffStack, spp: u32) -> Vec<f32> {
    let mut out = vec![0f32; (F32_WIDTH * F32_HEIGHT * spp) as usize];
    stack
        .read_tile_f32_into(0, f32_rect(0, 0, F32_WIDTH, F32_HEIGHT), &mut out)
        .expect("f32 read");
    out
}

fn bits(samples: &[f32]) -> Vec<u32> {
    samples.iter().map(|s| s.to_bits()).collect()
}

#[test]
fn f32_staged_round_trips_bit_for_bit() {
    for spp in [1, 3] {
        let path = scratch(&format!("f32_staged_round_trip_{spp}.tif"));
        let samples = f32_samples(spp);
        let mut writer = TiffWriter::create_f32(&path, f32_spec(spp)).expect("create_f32");
        write_f32_rows(&mut writer, spp, &samples, 0..F32_HEIGHT);
        writer.finish().expect("finish");

        let stack = TiffStack::open_staged(&path).expect("open_staged");
        assert_eq!(stack.sample_format(), SampleFormat::F32);
        assert_eq!(stack.dimensions(), (F32_WIDTH, F32_HEIGHT));
        assert_eq!(
            bits(&read_all_f32(&stack, spp)),
            bits(&samples),
            "spp {spp}"
        );

        let mut window = vec![0f32; (9 * 5 * spp) as usize];
        stack
            .read_tile_f32_into(0, f32_rect(11, 7, 9, 5), &mut window)
            .expect("window read");
        let expect: Vec<f32> = (7..12)
            .flat_map(|row| {
                let start = ((row * F32_WIDTH + 11) * spp) as usize;
                samples[start..start + (9 * spp) as usize].to_vec()
            })
            .collect();
        assert_eq!(bits(&window), bits(&expect), "window, spp {spp}");
    }
}

#[test]
fn f32_staged_survives_reopen_after_a_partial_write() {
    let spp = 3;
    let path = scratch("f32_staged_reopen.tif");
    let samples = f32_samples(spp);
    let mut writer = TiffWriter::create_f32(&path, f32_spec(spp)).expect("create_f32");
    write_f32_rows(&mut writer, spp, &samples, 0..11);
    drop(writer);

    let mut writer = TiffWriter::reopen_f32(&path, f32_spec(spp)).expect("reopen_f32");
    write_f32_rows(&mut writer, spp, &samples, 0..F32_HEIGHT);
    writer.finish().expect("finish");

    let stack = TiffStack::open_staged(&path).expect("open_staged");
    assert_eq!(bits(&read_all_f32(&stack, spp)), bits(&samples));
}

#[test]
fn f32_staged_writers_refuse_the_other_format() {
    let path = scratch("f32_staged_cross_format.tif");
    let rect = f32_rect(0, 0, 1, 1);

    let mut float = TiffWriter::create_f32(&path, f32_spec(1)).expect("create_f32");
    assert!(float.write_tile(rect, &[1u16]).is_err(), "u16 into f32");
    drop(float);
    assert!(
        TiffWriter::reopen(&path, f32_spec(1)).is_err(),
        "u16 reopen of a float file"
    );

    let mut int = TiffWriter::create(&path, f32_spec(1)).expect("create");
    assert!(int.write_tile_f32(rect, &[1.0f32]).is_err(), "f32 into u16");
    drop(int);
    assert!(
        TiffWriter::reopen_f32(&path, f32_spec(1)).is_err(),
        "f32 reopen of a u16 file"
    );
}

#[test]
fn f32_staged_is_refused_by_source_frame_opens_only() {
    let path = scratch("f32_staged_refusals.tif");
    let samples = f32_samples(1);
    let mut writer = TiffWriter::create_f32(&path, f32_spec(1)).expect("create_f32");
    write_f32_rows(&mut writer, 1, &samples, 0..F32_HEIGHT);
    writer.finish().expect("finish");

    match TiffStack::open_files(std::slice::from_ref(&path)) {
        Err(TiffError::Unsupported { .. }) => {}
        other => panic!("open_files must refuse a float file, got {other:?}"),
    }

    for (name, bits_per_sample, compression) in [
        ("f32_staged_lzw.tif", 16, StripCompression::Lzw),
        ("f32_staged_8bit.tif", 8, StripCompression::None),
    ] {
        let path = scratch(name);
        let mut writer = StripWriter::create(
            &path,
            StripSpec {
                width: 4,
                height: 4,
                samples_per_pixel: 1,
                bits_per_sample,
                compression,
                description: None,
                metadata: Default::default(),
            },
        )
        .expect("strip writer");
        writer
            .write_rows(&vec![0u8; 4 * 4 * (bits_per_sample as usize / 8)])
            .expect("rows");
        writer.finish().expect("finish");
        match TiffStack::open_staged(&path) {
            Err(TiffError::Unsupported { .. }) => {}
            other => panic!("open_staged must refuse {name}, got {other:?}"),
        }
    }
}

#[test]
fn f32_staged_u16_reads_narrow() {
    let spp = 3;
    let path = scratch("f32_staged_narrow.tif");
    let samples = f32_samples(spp);
    let mut writer = TiffWriter::create_f32(&path, f32_spec(spp)).expect("create_f32");
    write_f32_rows(&mut writer, spp, &samples, 0..F32_HEIGHT);
    writer.finish().expect("finish");
    let stack = TiffStack::open_staged(&path).expect("open_staged");

    let narrowed: Vec<u16> = samples
        .iter()
        .map(|&s| (s.clamp(0.0, 65_535.0) + 0.5).floor() as u16)
        .collect();
    assert!(narrowed.contains(&0) && narrowed.contains(&65_535));
    let full = f32_rect(0, 0, F32_WIDTH, F32_HEIGHT);

    let mut inherent = vec![0u16; narrowed.len()];
    stack
        .read_tile_into(0, full, &mut inherent)
        .expect("inherent u16 read");
    assert_eq!(inherent, narrowed);

    let reader: &dyn TileReader = &stack;
    assert_eq!(reader.sample_format(), SampleFormat::F32);
    let mut through_trait = vec![0u16; narrowed.len()];
    reader
        .read_tile_into(0, full, &mut through_trait)
        .expect("trait u16 read");
    assert_eq!(through_trait, narrowed);

    let mut native = vec![0f32; samples.len()];
    reader
        .read_tile_f32_into(0, full, &mut native)
        .expect("trait f32 read");
    assert_eq!(bits(&native), bits(&samples));

    let mut stepped = vec![0u16; (F32_WIDTH.div_ceil(2) * F32_HEIGHT.div_ceil(2) * spp) as usize];
    stack
        .read_tile_step_into(0, full, std::num::NonZeroU32::new(2).unwrap(), &mut stepped)
        .expect("stepped u16 read");
    let expect: Vec<u16> = (0..F32_HEIGHT)
        .step_by(2)
        .flat_map(|y| {
            (0..F32_WIDTH)
                .step_by(2)
                .flat_map(move |x| (0..spp).map(move |c| ((y * F32_WIDTH + x) * spp + c) as usize))
        })
        .map(|i| narrowed[i])
        .collect();
    assert_eq!(stepped, expect);
}

#[test]
fn f32_staged_accepts_the_16_bit_layout_and_widens_on_an_f32_read() {
    let path = scratch("f32_staged_u16_file.tif");
    let mut writer = TiffWriter::create(&path, f32_spec(1)).expect("create");
    let samples: Vec<u16> = (0..F32_WIDTH * F32_HEIGHT)
        .map(|i| (i * 71) as u16)
        .collect();
    writer
        .write_tile(f32_rect(0, 0, F32_WIDTH, F32_HEIGHT), &samples)
        .expect("write");
    writer.finish().expect("finish");

    let stack = TiffStack::open_staged(&path).expect("open_staged accepts the 16-bit layout");
    assert_eq!(stack.sample_format(), SampleFormat::U16);
    let widened: Vec<f32> = samples.iter().map(|&s| f32::from(s)).collect();
    assert_eq!(bits(&read_all_f32(&stack, 1)), bits(&widened));
}
