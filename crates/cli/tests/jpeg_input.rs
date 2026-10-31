// SPDX-License-Identifier: GPL-3.0-or-later

//! `info` and `stack` over a directory of JPEG frames encoded in memory.

mod common;

use std::path::{Path, PathBuf};

use focusforge_core::io::test_frames::{
    FrameMetadata, ascii_entry, rational_bo, shorts_bo, write_frame_with_metadata,
};
use focusforge_core::io::{ByteOrder, TiffStack, read_source_metadata};
use jpeg_encoder::{ColorType, Encoder};

const WIDTH: u16 = 40;
const HEIGHT: u16 = 24;

fn write_frames(dir: &Path, count: usize) -> Vec<PathBuf> {
    std::fs::create_dir_all(dir).expect("frame directory");
    (0..count)
        .map(|index| {
            let data: Vec<u8> = (0..usize::from(WIDTH) * usize::from(HEIGHT) * 3)
                .map(|i| ((i / 3 * 7 + i % 3 * 60 + index * 5) % 251) as u8)
                .collect();
            let mut bytes = Vec::new();
            Encoder::new(&mut bytes, 90)
                .encode(&data, WIDTH, HEIGHT, ColorType::Rgb)
                .expect("frame encodes");
            let path = dir.join(format!("{index:02}.jpg"));
            std::fs::write(&path, bytes).expect("frame file");
            path
        })
        .collect()
}

fn run(cwd: &Path, args: &[&str]) -> std::process::Output {
    common::cli_bin("jpeg_input")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run CLI")
}

fn stdout(output: &std::process::Output) -> String {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn info_reports_jpeg_frames() {
    let cwd = common::tmp_dir("jpeg_input_info");
    let input = cwd.join("input");
    let frames = write_frames(&input, 3);

    let directory = stdout(&run(
        &cwd,
        &["info", input.to_str().expect("UTF-8"), "--json"],
    ));
    let report: serde_json::Value = serde_json::from_str(&directory).expect("JSON report");
    assert_eq!(report["frame_count"], 3);
    assert_eq!(report["unreadable_count"], 0);
    assert!(
        report["uniform_layout"].as_bool().expect("flag"),
        "{report}"
    );
    for file in report["files"].as_array().expect("files") {
        assert_eq!(file["width"], WIDTH);
        assert_eq!(file["height"], HEIGHT);
        assert_eq!(file["bits_per_sample"], serde_json::json!([8, 8, 8]));
        assert_eq!(file["samples_per_pixel"], 3);
        assert_eq!(file["reader"], "JpegStack");
    }

    let single = stdout(&run(&cwd, &["info", frames[0].to_str().expect("UTF-8")]));
    assert!(single.contains("40x24 3ch 8-bit"), "{single}");
    assert!(single.contains("reader: JpegStack"), "{single}");

    let text = stdout(&run(&cwd, &["info", input.to_str().expect("UTF-8")]));
    assert!(text.contains("3 frames (0 TIFF, 3 JPEG)"), "{text}");
}

#[test]
fn stack_writes_a_sixteen_bit_composite_of_the_frame_geometry() {
    let cwd = common::tmp_dir("jpeg_input_stack");
    let input = cwd.join("input");
    let output = cwd.join("output");
    write_frames(&input, 3);
    std::fs::create_dir(&output).expect("output directory");

    let mut args = vec![
        "stack",
        input.to_str().expect("UTF-8"),
        "-o",
        output.to_str().expect("UTF-8"),
        "--algorithm",
        "laplacian",
        "--no-align",
        "--no-crop",
    ];
    if cfg!(feature = "gpu") {
        args.push("--cpu");
    }
    stdout(&run(&cwd, &args));
    let project = output.join("input.ffp");
    assert!(project.is_file());

    let composite = output.join("composite.tif");
    stdout(&run(
        &cwd,
        &[
            "export",
            project.to_str().expect("UTF-8"),
            "-o",
            composite.to_str().expect("UTF-8"),
        ],
    ));
    let stack = TiffStack::open_files(&[composite]).expect("exported composite");
    assert_eq!(stack.dimensions(), (u32::from(WIDTH), u32::from(HEIGHT)));
    assert_eq!(stack.header(0).bits_per_sample.as_ref(), &[16, 16, 16]);
}

/// Frames whose Exif payload is a TIFF carrying orientation 6 and the tags
/// the export must pass through.
fn write_frames_with_exif(dir: &Path, count: usize) {
    let metadata = FrameMetadata {
        ifd0: vec![
            (271, 2, ascii_entry("TestCam")),
            (272, 2, ascii_entry("Model One")),
            (274, 3, shorts_bo(ByteOrder::Little, &[6])),
        ],
        exif: vec![
            (33434, 5, rational_bo(ByteOrder::Little, 1, 250)),
            (36867, 2, ascii_entry("2024:03:17 08:00:00")),
        ],
        ..FrameMetadata::default()
    };
    let tiff = dir.join("exif-source.tif");
    std::fs::create_dir_all(dir).expect("frame directory");
    write_frame_with_metadata(&tiff, 8, 4, &metadata).expect("EXIF source");
    let blob = std::fs::read(&tiff).expect("EXIF bytes");
    std::fs::remove_file(&tiff).expect("EXIF source removed");
    for index in 0..count {
        let data: Vec<u8> = (0..usize::from(WIDTH) * usize::from(HEIGHT) * 3)
            .map(|i| ((i / 3 * 7 + i % 3 * 60 + index * 5) % 251) as u8)
            .collect();
        let mut bytes = Vec::new();
        let mut encoder = Encoder::new(&mut bytes, 90);
        encoder.add_exif_metadata(&blob).expect("EXIF segment fits");
        encoder
            .encode(&data, WIDTH, HEIGHT, ColorType::Rgb)
            .expect("frame encodes");
        std::fs::write(dir.join(format!("{index:02}.jpg")), bytes).expect("frame file");
    }
}

#[test]
fn export_carries_the_jpeg_source_exif() {
    let cwd = common::tmp_dir("jpeg_input_exif");
    let input = cwd.join("input");
    let output = cwd.join("output");
    write_frames_with_exif(&input, 3);
    std::fs::create_dir(&output).expect("output directory");

    let mut args = vec![
        "stack",
        input.to_str().expect("UTF-8"),
        "-o",
        output.to_str().expect("UTF-8"),
        "--algorithm",
        "laplacian",
        "--no-align",
        "--no-crop",
    ];
    if cfg!(feature = "gpu") {
        args.push("--cpu");
    }
    stdout(&run(&cwd, &args));
    let project = output.join("input.ffp");

    let tif = output.join("out.tif");
    let jpg = output.join("out.jpg");
    for (path, extra) in [(&tif, None), (&jpg, Some(["--format", "jpeg"]))] {
        let mut args = vec![
            "export",
            project.to_str().expect("UTF-8"),
            "-o",
            path.to_str().expect("UTF-8"),
        ];
        args.extend(extra.into_iter().flatten());
        stdout(&run(&cwd, &args));

        let metadata = read_source_metadata(path);
        let name = path.display();
        assert!(
            metadata.warnings.is_empty(),
            "{name}: {:?}",
            metadata.warnings
        );
        let ifd0 = |tag: u16| {
            metadata
                .ifd0
                .iter()
                .find(|e| e.tag == tag)
                .map(|e| &e.value)
        };
        let exif = |tag: u16| {
            metadata
                .exif
                .iter()
                .find(|e| e.tag == tag)
                .map(|e| &e.value)
        };
        assert_eq!(
            ifd0(271).map(Vec::as_slice),
            Some(&b"TestCam\0"[..]),
            "{name}"
        );
        assert_eq!(
            ifd0(272).map(Vec::as_slice),
            Some(&b"Model One\0"[..]),
            "{name}"
        );
        assert_eq!(
            ifd0(274).map(Vec::as_slice),
            Some(&1u16.to_le_bytes()[..]),
            "{name}"
        );
        let software = ifd0(305).expect("Software");
        assert!(software.starts_with(b"FocusForge"), "{name}");
        assert_eq!(
            exif(36867).map(Vec::as_slice),
            Some(&b"2024:03:17 08:00:00\0"[..]),
            "{name}"
        );
        assert!(exif(33434).is_some(), "{name}: ExposureTime");
    }
    let stack = TiffStack::open_files(&[tif]).expect("exported composite");
    assert_eq!(stack.dimensions(), (u32::from(HEIGHT), u32::from(WIDTH)));
}
