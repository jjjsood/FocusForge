// SPDX-License-Identifier: GPL-3.0-or-later

//! `info` and `stack` over a directory of TIFF frames tagged Orientation 6.

mod common;

use std::path::{Path, PathBuf};

use focusforge_core::io::{
    MetadataTags, StripCompression, StripSpec, StripWriter, TagEntry, TiffStack,
};

const WIDTH: u32 = 48;
const HEIGHT: u32 = 32;

fn write_frames(dir: &Path, count: usize) -> Vec<PathBuf> {
    std::fs::create_dir_all(dir).expect("frame directory");
    (0..count)
        .map(|index| {
            let path = dir.join(format!("{index:02}.tif"));
            let mut writer = StripWriter::create(
                &path,
                StripSpec {
                    width: WIDTH,
                    height: HEIGHT,
                    samples_per_pixel: 3,
                    bits_per_sample: 16,
                    compression: StripCompression::None,
                    description: None,
                    metadata: MetadataTags {
                        ifd0: vec![TagEntry {
                            tag: 274,
                            field_type: 3,
                            count: 1,
                            value: 6_u16.to_le_bytes().to_vec(),
                        }],
                        ..Default::default()
                    },
                },
            )
            .expect("frame writer");
            let rows: Vec<u8> = (0..(WIDTH * HEIGHT * 3) as usize)
                .flat_map(|i| ((i * 37 + index * 1009) as u16).to_le_bytes())
                .collect();
            writer.write_rows(&rows).expect("rows");
            writer.finish().expect("finish");
            path
        })
        .collect()
}

fn run(cwd: &Path, args: &[&str]) -> std::process::Output {
    common::cli_bin("tiff_orientation_input")
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
fn info_reports_the_upright_geometry_and_the_orientation() {
    let cwd = common::tmp_dir("tiff_orientation_info");
    let input = cwd.join("input");
    let frames = write_frames(&input, 3);

    let json = stdout(&run(
        &cwd,
        &["info", input.to_str().expect("UTF-8"), "--json"],
    ));
    let report: serde_json::Value = serde_json::from_str(&json).expect("JSON report");
    assert_eq!(report["frame_count"], 3);
    assert!(
        report["uniform_layout"].as_bool().expect("flag"),
        "{report}"
    );
    for file in report["files"].as_array().expect("files") {
        assert_eq!(file["width"], HEIGHT);
        assert_eq!(file["height"], WIDTH);
        assert_eq!(file["orientation"], 6);
        assert_eq!(file["reader"], "DecodedTiffStack");
        assert_eq!(file["mmap_fast_path"], false);
    }

    let text = stdout(&run(&cwd, &["info", frames[0].to_str().expect("UTF-8")]));
    assert!(text.contains("32x48 3ch"), "{text}");
    assert!(text.contains("orientation: 6"), "{text}");
    assert!(
        text.contains("mmap fast path: no — Orientation=6"),
        "{text}"
    );
}

#[test]
fn stack_writes_an_upright_sixteen_bit_composite() {
    let cwd = common::tmp_dir("tiff_orientation_stack");
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
    assert_eq!(stack.dimensions(), (HEIGHT, WIDTH));
    assert_eq!(stack.header(0).bits_per_sample.as_ref(), &[16, 16, 16]);
}
