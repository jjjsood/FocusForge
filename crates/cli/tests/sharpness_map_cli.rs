// SPDX-License-Identifier: GPL-3.0-or-later

//! `sharpness-map`: the scale, the written maps, the printed rows, the help
//! and the refusals.

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;

use focusforge_core::io::{FrameReader, TiffStack, open_frame_set};
use focusforge_core::sharpness_map::{
    SHARPNESS_PERCENTILE, sharpness_map_tile, sharpness_plan, stack_sharpness_scale,
};
use focusforge_core::tile::{HostBudget, NoControl, Rect};

fn run(test: &str, dir: &Path, out: &Path, args: &[&str]) -> Output {
    common::cli_bin(test)
        .arg("sharpness-map")
        .arg(dir)
        .arg("--out")
        .arg(out)
        .args(args)
        .output()
        .expect("the CLI binary starts")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

fn frames_dir() -> PathBuf {
    common::mantis_512_dir().join("frames")
}

/// The map of `frame` as `sharpness_map_tile` gives it at tile size 256.
fn expected_map(
    reader: &dyn focusforge_core::algorithm::TileReader,
    extent: (u32, u32),
    frame: usize,
    scale: f32,
) -> Vec<f32> {
    let mut map = vec![0.0; extent.0 as usize * extent.1 as usize];
    for tile in sharpness_plan(extent, 256).unwrap().tiles() {
        let values = sharpness_map_tile(reader, frame, tile, 3, scale).unwrap();
        let rect = tile.interior();
        for (row, chunk) in values.chunks_exact(rect.width as usize).enumerate() {
            let start = (rect.y as usize + row) * extent.0 as usize + rect.x as usize;
            map[start..start + chunk.len()].copy_from_slice(chunk);
        }
    }
    map
}

/// A written map: its size, samples per pixel and values.
fn read_map(path: &Path) -> ((u32, u32), u32, Vec<f32>) {
    let stack = TiffStack::open_staged(path).expect("the map is a readable TIFF");
    let (width, height) = stack.dimensions();
    let mut values = vec![0.0f32; width as usize * height as usize];
    stack
        .read_tile_f32_into(
            0,
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            &mut values,
        )
        .expect("the map reads back as f32");
    ((width, height), stack.frame(0).samples_per_pixel, values)
}

fn bits(values: &[f32]) -> Vec<u32> {
    values.iter().map(|v| v.to_bits()).collect()
}

#[test]
fn sharpness_map_writes_frame_and_composite_maps() {
    let out = common::tmp_dir("sharpness_map_mantis");
    let reference = common::mantis_512_dir().join("reference.tif");
    let output = run(
        "sharpness_map_mantis",
        &frames_dir(),
        &out.join("maps"),
        &["--json", "--composite", reference.to_str().unwrap()],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one JSON object");

    let mut paths: Vec<_> = std::fs::read_dir(frames_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    paths.sort();
    let set = open_frame_set(
        &paths,
        HostBudget::host_derived(&focusforge_core::host::snapshot()),
    )
    .expect("the fixture frames open");
    let reader = set.reader.as_ref();
    let extent = reader.dimensions();
    let scale = stack_sharpness_scale(reader, reader.len(), extent, 3, 256, &NoControl).unwrap();
    assert!(scale > 0.0);

    assert_eq!(value["percentile"], SHARPNESS_PERCENTILE);
    assert_eq!(value["scale"], f64::from(scale));
    let frames = value["frames"].as_array().expect("a frames array");
    assert_eq!(frames.len(), 8);
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(frame["index"], index);
        assert_eq!(frame["file"], format!("0{index}.tif"));
        let map = PathBuf::from(frame["map"].as_str().expect("a map path"));
        assert_eq!(map, out.join(format!("maps/0{index}-sharpness.tif")));
        let (size, spp, values) = read_map(&map);
        assert_eq!((size, spp), ((512, 512), 1), "frame {index}");
        if index == 3 {
            let wanted = expected_map(reader, extent, 3, scale);
            assert_eq!(
                bits(&values),
                bits(&wanted),
                "frame 3 is its score over the scale"
            );
        }
    }

    assert_eq!(value["composite"]["file"], "reference.tif");
    let map = PathBuf::from(value["composite"]["map"].as_str().expect("a map path"));
    assert_eq!(map, out.join("maps/composite-sharpness.tif"));
    let (size, spp, values) = read_map(&map);
    assert_eq!((size, spp), ((512, 512), 1));
    let composite = open_frame_set(
        &[reference],
        HostBudget::host_derived(&focusforge_core::host::snapshot()),
    )
    .expect("the composite opens");
    let wanted = expected_map(composite.reader.as_ref(), extent, 0, scale);
    assert_eq!(
        bits(&values),
        bits(&wanted),
        "the composite on the frames' scale"
    );
}

#[test]
fn sharpness_map_prints_the_scale_and_one_row_per_map() {
    let frames = common::synthetic_frames("sharpness_map_text_frames");
    let out = common::tmp_dir("sharpness_map_text_out");
    let output = run("sharpness_map_text", &frames, &out, &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    let scale = lines[0].strip_prefix("scale  ").expect("the scale line");
    assert!(scale.parse::<f32>().is_ok_and(|s| s > 0.0), "{scale}");
    for index in 0..3 {
        let map = out.join(format!("frame-{index:02}-sharpness.tif"));
        assert_eq!(
            lines[1 + index],
            format!("{index}  frame-{index:02}.tif  {}", map.display())
        );
        assert!(map.is_file());
    }
    let low = run("sharpness_map_low", &frames, &out, &["--priority", "low"]);
    assert_eq!(
        stdout(&low),
        text,
        "the priority does not change the result"
    );
}

#[test]
fn sharpness_map_help_states_the_scale() {
    let output = common::cli_bin("sharpness_map_help")
        .args(["sharpness-map", "--help"])
        .output()
        .expect("the CLI binary starts");
    assert!(output.status.success());
    let help = stdout(&output)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(help.contains("99th percentile"), "{help}");
    assert!(help.contains("without alignment"), "{help}");
}

fn assert_refused(output: &Output, fragments: &[&str], out: &Path) {
    assert_eq!(output.status.code(), Some(1), "{}", stderr(output));
    let err = stderr(output);
    assert!(err.starts_with("error: "), "{err}");
    for fragment in fragments {
        assert!(err.contains(fragment), "missing `{fragment}` in: {err}");
    }
    assert!(!err.contains("panicked"), "{err}");
    assert!(output.stdout.is_empty());
    let left: Vec<_> = std::fs::read_dir(out)
        .into_iter()
        .flatten()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with("-sharpness.tif") || name.ends_with(".partial"))
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

#[test]
fn sharpness_map_refuses_bad_input_naming_it() {
    let out = common::tmp_dir("sharpness_map_refused_out");

    let empty = common::tmp_dir("sharpness_map_empty");
    assert_refused(
        &run("sharpness_map_empty", &empty, &out, &[]),
        &["no TIFF frames found"],
        &out,
    );

    let garbage = common::tmp_dir("sharpness_map_garbage");
    let bad = garbage.join("00.tif");
    std::fs::write(&bad, b"this is not a TIFF file at all").unwrap();
    let bad = bad.display().to_string();
    assert_refused(
        &run("sharpness_map_garbage", &garbage, &out, &[]),
        &[&bad],
        &out,
    );

    let small = common::fixtures_dir().join("mantis-64/frames/00.tif");
    assert_refused(
        &run(
            "sharpness_map_size",
            &frames_dir(),
            &out,
            &["--composite", small.to_str().unwrap()],
        ),
        &["64x64", "512x512"],
        &out,
    );

    #[cfg(unix)]
    if let Some(readonly) = common::readonly_dir("sharpness_map_unwritable", 0o555, |_| {}) {
        let blocked = readonly.join("maps");
        let output = run("sharpness_map_unwritable", &frames_dir(), &blocked, &[]);
        assert_refused(
            &output,
            &[
                "cannot create output directory",
                &blocked.display().to_string(),
            ],
            &blocked,
        );
    }
}
