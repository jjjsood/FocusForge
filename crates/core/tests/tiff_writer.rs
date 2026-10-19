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
