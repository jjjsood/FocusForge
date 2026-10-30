// SPDX-License-Identifier: GPL-3.0-or-later

//! `info` and `stack` over a directory of JPEG frames encoded in memory.

mod common;

use std::path::{Path, PathBuf};

use focusforge_core::io::TiffStack;
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
