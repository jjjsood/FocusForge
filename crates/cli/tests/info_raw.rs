// SPDX-License-Identifier: GPL-3.0-or-later

mod common;

#[path = "../../core/tests/common/raw_dng.rs"]
mod raw_dng;

use focusforge_core::io::RawStack;
use raw_dng::{DngSpec, write_dng};

fn info(args: &[&str]) -> std::process::Output {
    common::cli_bin("info_raw")
        .args(args)
        .output()
        .expect("run info")
}

#[test]
fn info_reports_synthetic_dng_facts() {
    for (bits, black, white, cfa, crop, expected_cfa) in [
        (12, 64, 4095, [0, 1, 1, 2], None, [0, 1, 1, 2]),
        (
            14,
            128,
            16383,
            [1, 0, 2, 1],
            Some([1, 2, 4, 3]),
            [1, 0, 2, 1],
        ),
    ] {
        let mut spec = DngSpec::new(bits, black, white, cfa, vec![1; 64]);
        spec.crop = crop;
        let dng = write_dng(&spec, true);
        let json = info(&["info", "--json", dng.path().to_str().unwrap()]);
        assert!(
            json.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&json.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&json.stdout).expect("JSON report");
        let raw = &report["files"][0]["raw"];
        assert_eq!(raw["raw_width"], 8);
        assert_eq!(raw["raw_height"], 8);
        assert_eq!(raw["output_width"], crop.map_or(8, |crop| crop[2]));
        assert_eq!(raw["output_height"], crop.map_or(8, |crop| crop[3]));
        assert_eq!(raw["crop_x"], crop.map_or(0, |crop| crop[0]));
        assert_eq!(raw["crop_y"], crop.map_or(0, |crop| crop[1]));
        assert_eq!(raw["storage_bits"], bits);
        assert_eq!(raw["decoder_bits"], serde_json::Value::Null);
        assert_eq!(
            raw["black_levels"],
            serde_json::json!(vec![black as f32; 4])
        );
        assert_eq!(raw["white_levels"], serde_json::json!([white]));
        assert_eq!(raw["cfa"], serde_json::json!(expected_cfa));
        assert_eq!(raw["cfa_width"], 2);
        assert_eq!(raw["cfa_height"], 2);
        assert_eq!(
            raw["color_matrix"],
            serde_json::json!([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0])
        );
        assert_eq!(raw["capture_time_epoch_seconds"], serde_json::Value::Null);
        assert_eq!(raw["decoder"], "rawler");

        let human = info(&["info", dng.path().to_str().unwrap()]);
        assert!(human.status.success());
        let human = String::from_utf8_lossy(&human.stdout);
        assert!(human.contains(&format!("storage_bits=Some({bits})")));
        assert!(human.contains("matrix=Some([1.0, 0.0, 0.0"));
        assert!(!human.contains("capture_time_epoch_seconds:"));
    }
}

#[test]
fn info_on_raw_decodes_no_pixels() {
    let dng = write_dng(
        &DngSpec::new(14, 128, 16383, [0, 1, 1, 2], vec![1; 64]),
        true,
    );
    let output = info(&["info", dng.path().to_str().unwrap()]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("RAW FocusForge Test Synthetic Bayer"),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn mixed_directory_keeps_tiff_records_unchanged() {
    let tiff = common::fixtures_dir().join("mantis-64/frames/00.tif");
    let dir = common::tmp_dir("mixed_directory_keeps_tiff_records_unchanged");
    std::fs::copy(&tiff, dir.join("00.tif")).expect("copy TIFF");
    let before = info(&["info", "--json", dir.to_str().unwrap()]);
    assert!(before.status.success());
    let before: serde_json::Value = serde_json::from_slice(&before.stdout).expect("TIFF JSON");
    let dng = write_dng(&DngSpec::new(12, 0, 4095, [0, 1, 1, 2], vec![1; 64]), true);
    std::fs::copy(dng.path(), dir.join("01.dng")).expect("copy DNG");

    let output = info(&["info", "--json", dir.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("mixed JSON");
    assert_eq!(report["files"][0], before["files"][0]);
    assert!(report["files"][1]["raw"].is_object());
}

#[test]
fn unreadable_raw_is_reported_by_name() {
    let dir = common::tmp_dir("unreadable_raw_is_reported_by_name");
    let broken = dir.join("broken.dng");
    std::fs::write(&broken, b"not a DNG").expect("write malformed DNG");

    let file = info(&["info", broken.to_str().unwrap()]);
    assert!(!file.status.success());
    assert!(String::from_utf8_lossy(&file.stderr).contains("broken.dng"));

    let listing = info(&["info", "--json", dir.to_str().unwrap()]);
    assert!(listing.status.success());
    let report: serde_json::Value = serde_json::from_slice(&listing.stdout).expect("listing JSON");
    assert_eq!(report["frame_count"], 1);
    assert_eq!(report["unreadable_count"], 1);
    assert_eq!(report["files"][0]["name"], "broken.dng");
    assert!(report["files"][0]["error"].as_str().is_some());

    let human = info(&["info", dir.to_str().unwrap()]);
    assert!(human.status.success());
    assert!(String::from_utf8_lossy(&human.stdout).contains("1 frames (0 TIFF, 1 RAW)"));
}

#[test]
#[ignore = "uses locally supplied CR3 samples"]
fn info_reports_local_canon_cr3() {
    let Some(samples) = std::env::var_os("FOCUSFORGE_RAW_LOCAL_SAMPLES") else {
        eprintln!("skip: FOCUSFORGE_RAW_LOCAL_SAMPLES is not set");
        return;
    };
    let samples = std::path::PathBuf::from(samples);
    for (directory, file, model, geometry, crop, black, white, expected_capture) in [
        (
            "canon-r5-mark-ii",
            "canon-eos-r5-mark-ii_2026-09-26_125359.cr3",
            "EOS R5 Mark II",
            (8480, 5650, 8192, 5464),
            (272, 176),
            512.0,
            14888,
            1790427239,
        ),
        (
            "canon-r7",
            "canon-eos-r7_2025-01-10_220452.cr3",
            "EOS R7",
            (7128, 4732, 6960, 4640),
            (156, 84),
            2047.0,
            12735,
            1736546692,
        ),
    ] {
        let path = samples.join(directory).join(file);
        assert!(
            path.is_file(),
            "configured local RAW sample is missing: {}",
            path.display()
        );
        let human = info(&["info", path.to_str().unwrap()]);
        assert!(
            human.status.success(),
            "{}: {}",
            path.display(),
            String::from_utf8_lossy(&human.stderr)
        );
        let human_stdout = String::from_utf8_lossy(&human.stdout);
        assert!(human_stdout.contains(model));
        let json = info(&["info", "--json", path.to_str().unwrap()]);
        assert!(
            json.status.success(),
            "{}: {}",
            path.display(),
            String::from_utf8_lossy(&json.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&json.stdout).expect("CR3 JSON");
        let raw = &report["files"][0]["raw"];
        let reader = RawStack::open_files(std::slice::from_ref(&path)).expect("CR3 metadata");
        let metadata = reader.metadata(0).expect("CR3 metadata record");
        assert_eq!(metadata.make, "Canon");
        assert_eq!(metadata.model, model);
        assert_eq!(
            (
                metadata.raw_width,
                metadata.raw_height,
                metadata.output_width,
                metadata.output_height
            ),
            geometry
        );
        assert_eq!((metadata.crop_x, metadata.crop_y), crop);
        assert_eq!(metadata.storage_bits, None);
        assert_eq!(metadata.decoder_bits, Some(16));
        assert_eq!(metadata.black_levels.as_deref(), Some(&[black; 4][..]));
        assert_eq!(metadata.white_levels.as_deref(), Some(&[white][..]));
        assert_eq!(metadata.cfa.as_deref(), Some(&[0, 1, 1, 2][..]));
        assert_eq!(
            (metadata.cfa_width, metadata.cfa_height),
            (Some(2), Some(2))
        );
        assert_eq!(raw["make"], metadata.make);
        assert_eq!(raw["model"], metadata.model);
        assert_eq!(raw["raw_width"], metadata.raw_width);
        assert_eq!(raw["raw_height"], metadata.raw_height);
        assert_eq!(raw["output_width"], metadata.output_width);
        assert_eq!(raw["output_height"], metadata.output_height);
        assert_eq!(
            raw["storage_bits"].as_u64(),
            metadata.storage_bits.map(u64::from)
        );
        assert_eq!(
            raw["decoder_bits"].as_u64(),
            metadata.decoder_bits.map(|bits| bits as u64)
        );
        assert_eq!(
            raw["black_levels"],
            serde_json::json!(metadata.black_levels)
        );
        assert_eq!(
            raw["white_levels"],
            serde_json::json!(metadata.white_levels)
        );
        assert_eq!(raw["cfa"], serde_json::json!(metadata.cfa));
        assert_eq!(raw["cfa_width"], serde_json::json!(metadata.cfa_width));
        assert_eq!(raw["cfa_height"], serde_json::json!(metadata.cfa_height));
        let json_matrix = raw["color_matrix"].as_array().expect("CR3 color matrix");
        let metadata_matrix = metadata.color_matrix.as_deref().expect("CR3 color matrix");
        assert_eq!(json_matrix.len(), metadata_matrix.len());
        for (actual, expected) in json_matrix.iter().zip(metadata_matrix) {
            assert!((actual.as_f64().expect("matrix number") as f32 - expected).abs() < 0.000_001);
        }
        let capture = metadata
            .capture_time
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());
        assert_eq!(capture, Some(expected_capture));
        assert_eq!(
            raw["capture_time_epoch_seconds"],
            serde_json::json!(expected_capture)
        );
        assert_eq!(raw["decoder"], "rawler");
        assert!(human_stdout.contains(&format!("capture_time_epoch_seconds: {expected_capture}")));
    }
}

#[test]
#[ignore = "reads the configured RAW corpus"]
fn info_matches_corpus_manifest() {
    let Some(samples) = std::env::var_os("FOCUSFORGE_RAW_SAMPLES") else {
        eprintln!("skip: FOCUSFORGE_RAW_SAMPLES is not set");
        return;
    };
    let expectations = std::env::var_os("FOCUSFORGE_RAW_EXPECTATIONS")
        .expect("FOCUSFORGE_RAW_EXPECTATIONS is required with FOCUSFORGE_RAW_SAMPLES");
    let samples = std::path::PathBuf::from(samples);
    assert!(
        samples.is_dir(),
        "configured RAW samples are missing: {}",
        samples.display()
    );
    let expectations_path = std::path::PathBuf::from(expectations);
    assert!(
        expectations_path.is_file(),
        "configured RAW expectations are missing: {}",
        expectations_path.display()
    );
    let expectations: serde_json::Value =
        serde_json::from_slice(&std::fs::read(expectations_path).expect("read RAW expectations"))
            .expect("parse RAW expectations");
    for (name, file) in [
        (
            "blackmagic-micro-cinema-camera",
            "blackmagic-micro-cinema-camera.dng",
        ),
        ("canon-eos-20d", "canon-eos-20d.cr2"),
        ("fujifilm-x-s10", "fujifilm-x-s10.raf"),
        ("panasonic-dmc-lx7", "panasonic-dmc-lx7.rw2"),
        ("sony-ilce-7s", "sony-ilce-7s.arw"),
    ] {
        let path = samples.join(file);
        let output = info(&["info", "--json", path.to_str().unwrap()]);
        assert!(
            output.status.success(),
            "{}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("RAW JSON");
        let expected = &expectations["frames"][name];
        let raw = &report["files"][0]["raw"];
        let reader = RawStack::open_files(std::slice::from_ref(&path)).expect("corpus metadata");
        let metadata = reader.metadata(0).expect("corpus metadata record");
        assert_cli_metadata(name, raw, metadata);
        assert_oracle_metadata(name, raw, expected);
        assert_oracle_controls(name, raw, expected);
    }
    let blackmagic = samples.join("blackmagic-micro-cinema-camera.dng");
    let output = info(&["info", "--json", blackmagic.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("Blackmagic JSON");
    assert!(
        report["files"][0]["raw"].is_object(),
        "Blackmagic RAW metadata"
    );
    let nikon = samples.join("nikon-d1h.nef");
    let output = info(&["info", nikon.to_str().unwrap()]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("nikon-d1h.nef"));
    assert!(error.contains("NIKON") && error.contains("D1H"));

    let refused_dir = common::tmp_dir("unsupported_camera_directory");
    std::fs::copy(&nikon, refused_dir.join("nikon-d1h.nef")).expect("copy Nikon refusal fixture");
    let listing = info(&["info", "--json", refused_dir.to_str().unwrap()]);
    assert!(listing.status.success());
    let listing: serde_json::Value =
        serde_json::from_slice(&listing.stdout).expect("Nikon directory JSON");
    assert_eq!(listing["files"][0]["name"], "nikon-d1h.nef");
    assert!(
        listing["files"][0]["error"]
            .as_str()
            .is_some_and(|error| error.contains("NIKON") && error.contains("D1H"))
    );

    let sigma = samples.join("sigma-dp1.x3f");
    let output = info(&["info", sigma.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("sigma-dp1.x3f"));
}

fn assert_cli_metadata(
    name: &str,
    raw: &serde_json::Value,
    metadata: &focusforge_core::io::RawMetadata,
) {
    assert_eq!(raw["decoder"], "rawler", "{name}: decoder");
    assert_eq!(raw["raw_width"], metadata.raw_width, "{name}: raw width");
    assert_eq!(raw["raw_height"], metadata.raw_height, "{name}: raw height");
    assert_eq!(
        raw["output_width"], metadata.output_width,
        "{name}: output width"
    );
    assert_eq!(
        raw["output_height"], metadata.output_height,
        "{name}: output height"
    );
    assert_eq!(raw["crop_x"], metadata.crop_x, "{name}: crop x");
    assert_eq!(raw["crop_y"], metadata.crop_y, "{name}: crop y");
    assert_eq!(
        raw["storage_bits"],
        serde_json::json!(metadata.storage_bits)
    );
    assert_eq!(
        raw["decoder_bits"],
        serde_json::json!(metadata.decoder_bits)
    );
    assert_eq!(
        raw["black_levels"],
        serde_json::json!(metadata.black_levels)
    );
    assert_eq!(
        raw["white_levels"],
        serde_json::json!(metadata.white_levels)
    );
    assert_eq!(raw["cfa"], serde_json::json!(metadata.cfa));
    assert_eq!(raw["cfa_width"], serde_json::json!(metadata.cfa_width));
    assert_eq!(raw["cfa_height"], serde_json::json!(metadata.cfa_height));
    let matrix = raw["color_matrix"].as_array().expect("CLI matrix");
    let metadata_matrix = metadata.color_matrix.as_deref().expect("decoder matrix");
    assert_eq!(
        matrix.len(),
        metadata_matrix.len(),
        "{name}: matrix dimensions"
    );
    for (actual, expected) in matrix.iter().zip(metadata_matrix) {
        assert!(
            (actual.as_f64().expect("CLI matrix value") as f32 - expected).abs() <= f32::EPSILON,
            "{name}: CLI matrix value"
        );
    }
}

fn assert_oracle_metadata(name: &str, raw: &serde_json::Value, expected: &serde_json::Value) {
    assert_eq!(
        raw["raw_width"], expected["raw_width"]["value"],
        "{name}: raw width"
    );
    assert_eq!(
        raw["raw_height"], expected["raw_height"]["value"],
        "{name}: raw height"
    );
    assert_eq!(
        raw["cfa"],
        serde_json::json!(oracle_cfa(expected)),
        "{name}: CFA channels"
    );
    assert_eq!(
        raw["cfa_width"], expected["cfa_pattern"]["value"]["cols"],
        "{name}: CFA width"
    );
    assert_eq!(
        raw["cfa_height"], expected["cfa_pattern"]["value"]["rows"],
        "{name}: CFA height"
    );

    let oracle_storage = expected["bits_per_sample"]["value"]
        .as_u64()
        .expect("oracle storage bits");
    let decoder_bits = raw["decoder_bits"].as_u64();
    let storage_bits = raw["storage_bits"].as_u64();
    if storage_bits != Some(oracle_storage) || decoder_bits != Some(oracle_storage) {
        eprintln!(
            "{name}: storage_bits={storage_bits:?} (DNG header storage domain), decoder_bits={decoder_bits:?} (rawler unpack domain), oracle_storage_bits={oracle_storage} (LibRaw unpack storage domain)"
        );
    }
    if name == "blackmagic-micro-cinema-camera" {
        assert_eq!(storage_bits, Some(oracle_storage), "{name}: storage depth");
        assert!(decoder_bits.is_none(), "{name}: DNG has no decoder depth");
    }

    let oracle_crop = &expected["crop_margins"]["value"];
    let decoder_crop = [
        raw["crop_x"].as_u64().expect("decoder crop x"),
        raw["crop_y"].as_u64().expect("decoder crop y"),
        raw["output_width"].as_u64().expect("decoder output width"),
        raw["output_height"]
            .as_u64()
            .expect("decoder output height"),
    ];
    let oracle_crop = [
        oracle_crop["left"].as_u64().expect("oracle crop left"),
        oracle_crop["top"].as_u64().expect("oracle crop top"),
        oracle_crop["output_width"]
            .as_u64()
            .expect("oracle output width"),
        oracle_crop["output_height"]
            .as_u64()
            .expect("oracle output height"),
    ];
    if decoder_crop != oracle_crop {
        eprintln!(
            "{name}: decoder_crop={decoder_crop:?} (rawler default crop in raw coordinates), oracle_crop={oracle_crop:?} (LibRaw visible bounds)"
        );
    }

    let oracle_black = oracle_black(expected);
    let decoder_black = raw["black_levels"]
        .as_array()
        .expect("decoder black levels");
    if matches!(
        name,
        "blackmagic-micro-cinema-camera" | "fujifilm-x-s10" | "panasonic-dmc-lx7" | "sony-ilce-7s"
    ) {
        assert_eq!(
            normalized_levels(decoder_black, oracle_black.len()),
            oracle_black,
            "{name}: equivalent black values"
        );
    } else {
        eprintln!(
            "{name}: decoder_black={decoder_black:?} (rawler raw-code values), oracle_black={oracle_black:?} (LibRaw base plus offsets in raw-code values)"
        );
    }

    let oracle_white = oracle_white(expected);
    let decoder_white = raw["white_levels"]
        .as_array()
        .expect("decoder white levels");
    if matches!(
        name,
        "blackmagic-micro-cinema-camera" | "canon-eos-20d" | "sony-ilce-7s"
    ) {
        assert_eq!(
            decoder_white.first().and_then(serde_json::Value::as_u64),
            Some(oracle_white),
            "{name}: equivalent white value"
        );
    } else {
        eprintln!(
            "{name}: decoder_white={decoder_white:?} (rawler raw-code values), oracle_white={oracle_white} (LibRaw {})",
            expected["white_level"]["value"]["selected"]
                .as_str()
                .expect("oracle white selection")
        );
    }

    let matrix = raw["color_matrix"].as_array().expect("decoder matrix");
    let oracle_matrix = oracle_matrix(expected);
    let delta = matrix_delta(matrix, &oracle_matrix);
    if name == "blackmagic-micro-cinema-camera" {
        eprintln!(
            "{name}: decoder_matrix={matrix:?} (rawler DNG ColorMatrix1, Standard Light A, XYZ-to-camera), oracle_matrix={oracle_matrix:?} (DNG ColorMatrix2, D65, XYZ-to-camera), max_abs_delta={delta:.8}; different file-tag selection"
        );
    } else {
        assert!(
            delta <= 0.0002,
            "{name}: matrix delta {delta} exceeds 0.0002"
        );
        eprintln!(
            "{name}: matrix_max_abs_delta={delta:.8} (rawler XYZ-to-camera vs LibRaw camera-table XYZ-to-camera)"
        );
    }
}

fn oracle_cfa(expected: &serde_json::Value) -> Vec<u8> {
    expected["cfa_pattern"]["value"]["channels"]
        .as_array()
        .expect("oracle CFA")
        .iter()
        .map(|channel| match channel.as_str().expect("CFA channel") {
            "R" => 0,
            "G" => 1,
            "B" => 2,
            other => panic!("unsupported CFA channel: {other}"),
        })
        .collect()
}

fn oracle_black(expected: &serde_json::Value) -> Vec<f64> {
    let black = &expected["black_level"]["value"];
    let base = black["base"].as_f64().expect("oracle black base");
    let offsets = black["pattern_offsets"]
        .as_array()
        .filter(|offsets| !offsets.is_empty())
        .unwrap_or_else(|| {
            black["channel_offsets"]
                .as_array()
                .expect("oracle black offsets")
        });
    offsets
        .iter()
        .map(|offset| base + offset.as_f64().expect("oracle black offset"))
        .collect()
}

fn oracle_white(expected: &serde_json::Value) -> u64 {
    let white = &expected["white_level"]["value"];
    white["linear_max"]
        .as_array()
        .expect("oracle linear maximum")
        .iter()
        .filter_map(serde_json::Value::as_u64)
        .find(|value| *value > 0)
        .unwrap_or_else(|| white["maximum"].as_u64().expect("oracle white maximum"))
}

fn normalized_levels(levels: &[serde_json::Value], len: usize) -> Vec<f64> {
    let levels: Vec<_> = levels
        .iter()
        .map(|value| value.as_f64().expect("decoder level"))
        .collect();
    if levels.len() == 1 {
        vec![levels[0]; len]
    } else {
        levels
    }
}

fn oracle_matrix(expected: &serde_json::Value) -> Vec<f64> {
    let rows = expected["color_matrix"]["value"]
        .as_array()
        .expect("oracle matrix rows");
    if rows.len() == 4 {
        assert!(
            rows[3]
                .as_array()
                .expect("oracle fourth matrix row")
                .iter()
                .all(|value| value.as_f64() == Some(0.0)),
            "only a zero-padded fourth LibRaw matrix row may be omitted"
        );
    }
    rows.iter()
        .take(3)
        .flat_map(|row| row.as_array().expect("oracle matrix row"))
        .map(|value| value.as_f64().expect("oracle matrix value"))
        .collect()
}

fn matrix_delta(actual: &[serde_json::Value], expected: &[f64]) -> f64 {
    assert_eq!(actual.len(), expected.len(), "matrix dimensions");
    actual
        .iter()
        .zip(expected)
        .map(|(actual, expected)| (actual.as_f64().expect("decoder matrix value") - expected).abs())
        .fold(0.0, f64::max)
}

fn assert_oracle_controls(name: &str, raw: &serde_json::Value, expected: &serde_json::Value) {
    let mut wrong_dimensions = expected.clone();
    wrong_dimensions["raw_width"]["value"] =
        serde_json::json!(raw["raw_width"].as_u64().expect("raw width") + 1);
    assert_rejected(|| assert_oracle_metadata(name, raw, &wrong_dimensions));

    let mut wrong_cfa = expected.clone();
    wrong_cfa["cfa_pattern"]["value"]["channels"][0] = serde_json::json!("B");
    assert_rejected(|| assert_oracle_metadata(name, raw, &wrong_cfa));

    if name != "blackmagic-micro-cinema-camera" {
        let mut wrong_matrix = expected.clone();
        wrong_matrix["color_matrix"]["value"][0][0] = serde_json::json!(1000.0);
        assert_rejected(|| assert_oracle_metadata(name, raw, &wrong_matrix));
    }

    if matches!(
        name,
        "blackmagic-micro-cinema-camera" | "fujifilm-x-s10" | "panasonic-dmc-lx7" | "sony-ilce-7s"
    ) {
        let mut wrong_black = expected.clone();
        wrong_black["black_level"]["value"]["base"] = serde_json::json!(
            wrong_black["black_level"]["value"]["base"]
                .as_f64()
                .expect("oracle black base")
                + 1.0
        );
        assert_rejected(|| assert_oracle_metadata(name, raw, &wrong_black));
    }

    if matches!(
        name,
        "blackmagic-micro-cinema-camera" | "canon-eos-20d" | "sony-ilce-7s"
    ) {
        let mut wrong_white = expected.clone();
        increment_oracle_white(&mut wrong_white);
        assert_rejected(|| assert_oracle_metadata(name, raw, &wrong_white));
    }
}

fn increment_oracle_white(expected: &mut serde_json::Value) {
    let white = &mut expected["white_level"]["value"];
    if let Some(level) = white["linear_max"].as_array_mut().and_then(|levels| {
        levels
            .iter_mut()
            .find(|level| level.as_u64().is_some_and(|value| value > 0))
    }) {
        *level = serde_json::json!(level.as_u64().expect("oracle linear maximum") + 1);
    } else {
        white["maximum"] =
            serde_json::json!(white["maximum"].as_u64().expect("oracle white maximum") + 1);
    }
}

fn assert_rejected(test: impl FnOnce() + std::panic::UnwindSafe) {
    assert!(
        std::panic::catch_unwind(test).is_err(),
        "mutated oracle was accepted"
    );
}
