// SPDX-License-Identifier: GPL-3.0-or-later

//! `sharp-areas`: the printed rows, their JSON form, `--set` and the refusals.

mod common;

use std::path::Path;
use std::process::Output;

use focusforge_core::io::open_frame_set;
use focusforge_core::params::ParamValues;
use focusforge_core::sharp_areas::{SharpAreas, count_sharp_areas};
use focusforge_core::tile::{HostBudget, NoControl};

fn run(test: &str, dir: &Path, args: &[&str]) -> Output {
    common::cli_bin(test)
        .arg("sharp-areas")
        .arg(dir)
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

fn frames_dir() -> std::path::PathBuf {
    common::mantis_512_dir().join("frames")
}

fn expected(dir: &Path) -> SharpAreas {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .expect("the frames directory exists")
        .map(|entry| entry.expect("a readable entry").path())
        .collect();
    paths.sort();
    let set = open_frame_set(
        &paths,
        HostBudget::host_derived(&focusforge_core::host::snapshot()),
    )
    .expect("the fixture frames open");
    count_sharp_areas(
        set.reader.as_ref(),
        set.reader.len(),
        set.reader.dimensions(),
        set.reader.frame(0).samples_per_pixel,
        &ParamValues::new(),
        256,
        &NoControl,
    )
    .expect("the count runs")
}

#[test]
fn sharp_areas_prints_one_row_per_frame() {
    let counts = expected(&frames_dir()).counts;
    assert!(counts.iter().any(|&n| n > 0), "{counts:?}");
    let output = run("sharp_areas_text", &frames_dir(), &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let rows: Vec<String> = counts
        .iter()
        .enumerate()
        .map(|(index, count)| format!("{index}  0{index}.tif  {count}"))
        .collect();
    assert_eq!(stdout(&output), format!("{}\n", rows.join("\n")));
}

#[test]
fn sharp_areas_json_and_cell_size() {
    let expected = expected(&frames_dir());
    let output = run("sharp_areas_json", &frames_dir(), &["--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one JSON object");
    assert_eq!(value["params"]["cell-size"], 64);
    assert_eq!(value["params"]["contrast"], 2.0);
    assert_eq!(value["cells"], 64);
    let frames = value["frames"].as_array().expect("a frames array");
    assert_eq!(frames.len(), 8);
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(frame["index"], index);
        assert_eq!(frame["file"], format!("0{index}.tif"));
        assert_eq!(frame["count"], expected.counts[index]);
    }

    let output = run(
        "sharp_areas_cell_size",
        &frames_dir(),
        &["--json", "--set", "cell-size=128"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one JSON object");
    assert_eq!(value["cells"], 16);
    assert_eq!(value["params"]["cell-size"], 128);
}

fn assert_refused(output: &Output, fragment: &str) {
    assert_eq!(output.status.code(), Some(1), "{}", stderr(output));
    let err = stderr(output);
    assert!(err.starts_with("error: "), "{err}");
    assert!(err.contains(fragment), "missing `{fragment}` in: {err}");
    assert!(!err.contains("panicked"), "{err}");
    assert!(output.stdout.is_empty());
}

#[test]
fn sharp_areas_refuses_bad_input_naming_it() {
    let empty = common::tmp_dir("sharp_areas_empty");
    assert_refused(
        &run("sharp_areas_empty", &empty, &[]),
        "no TIFF frames found",
    );

    let garbage = common::tmp_dir("sharp_areas_garbage");
    let bad = garbage.join("00.tif");
    std::fs::write(&bad, b"this is not a TIFF file at all").unwrap();
    assert_refused(
        &run("sharp_areas_garbage", &garbage, &[]),
        &bad.display().to_string(),
    );

    let mixed = common::tmp_dir("sharp_areas_mixed");
    for entry in std::fs::read_dir(frames_dir()).unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(&path, mixed.join(path.file_name().unwrap())).unwrap();
    }
    std::fs::copy(
        common::fixtures_dir().join("mantis-64/frames/00.tif"),
        mixed.join("08.tif"),
    )
    .unwrap();
    assert_refused(
        &run("sharp_areas_mixed", &mixed, &[]),
        "does not match first frame",
    );

    assert_refused(
        &run(
            "sharp_areas_unknown_key",
            &frames_dir(),
            &["--set", "nonsense=1"],
        ),
        "nonsense",
    );
    assert_refused(
        &run(
            "sharp_areas_contrast",
            &frames_dir(),
            &["--set", "contrast=11"],
        ),
        "contrast: value 11 is outside the allowed range [1, 10]",
    );
}

#[test]
fn sharp_areas_priority_flag() {
    let plain = run("sharp_areas_priority_plain", &frames_dir(), &[]);
    let plain_json = run(
        "sharp_areas_priority_plain_json",
        &frames_dir(),
        &["--json"],
    );
    assert!(plain.status.success(), "{}", stderr(&plain));
    for value in ["low", "normal"] {
        let text = run(
            "sharp_areas_priority_text",
            &frames_dir(),
            &["--priority", value],
        );
        assert!(text.status.success(), "{value}: {}", stderr(&text));
        assert_eq!(stdout(&text), stdout(&plain), "{value}: rows");
        let json = run(
            "sharp_areas_priority_json",
            &frames_dir(),
            &["--json", "--priority", value],
        );
        assert_eq!(stdout(&json), stdout(&plain_json), "{value}: JSON");
    }

    let refused = run(
        "sharp_areas_priority_urgent",
        &frames_dir(),
        &["--priority", "urgent"],
    );
    assert!(!refused.status.success());
    assert_eq!(stdout(&refused), "");
    let message = stderr(&refused);
    assert!(
        message.contains("low") && message.contains("normal"),
        "{message}"
    );

    let help = common::cli_bin("sharp_areas_priority_help")
        .args(["sharp-areas", "--help"])
        .output()
        .expect("the CLI binary starts");
    let help = stdout(&help);
    assert!(
        help.contains("--priority")
            && help.contains("[default: normal]")
            && help.contains("- low:")
            && help.contains("- normal:"),
        "{help}"
    );
}
