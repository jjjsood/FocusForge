// SPDX-License-Identifier: GPL-3.0-or-later

//! Every spawn isolates `.scratch/` with its own per-process temp directory
//! and `.current_dir(dir)`, since parallel tests must never share a working
//! directory. Only [`stack_defaults_to_cpu`]'s subcommand writes
//! cwd-relative state, but every spawn routes through [`spawn_in`] anyway.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use focusforge_core::io::{Rect, TiffSpec, TiffWriter};

use common::mantis_512_dir;

fn bin() -> Command {
    common::cli_bin("cli")
}

/// Spawns the built CLI with `args`, in `dir` as its working directory.
///
/// Returns the raw `std::process::Output` rather than a project-specific
/// wrapper: every test in this file reads `output.status` (including its
/// `Debug` rendering) and both byte streams directly.
fn spawn_in(dir: &Path, args: &[&str]) -> std::process::Output {
    bin()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run binary")
}

fn mantis_64_dir() -> PathBuf {
    common::fixtures_dir().join("mantis-64")
}

/// Copy `source` (a little-endian, classic-TIFF fixture frame) to `dest`,
/// patching its `Compression` tag (259) from 1 to 5 in place. Never PIL: it
/// silently down-converts 16-bit RGB to 8-bit and would test the wrong file.
fn write_compressed_copy(source: &Path, dest: &Path) {
    let mut bytes = std::fs::read(source).expect("read fixture frame");
    assert_eq!(&bytes[0..2], b"II", "fixture frame must be little-endian");
    let ifd_offset = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let n_entries =
        u16::from_le_bytes(bytes[ifd_offset..ifd_offset + 2].try_into().unwrap()) as usize;
    let mut patched = false;
    for i in 0..n_entries {
        let entry_off = ifd_offset + 2 + i * 12;
        let tag = u16::from_le_bytes(bytes[entry_off..entry_off + 2].try_into().unwrap());
        if tag == 259 {
            bytes[entry_off + 8..entry_off + 10].copy_from_slice(&5u16.to_le_bytes());
            patched = true;
            break;
        }
    }
    assert!(
        patched,
        "Compression tag (259) not found in {}",
        source.display()
    );
    std::fs::write(dest, bytes).expect("write compressed copy");
}

#[test]
fn version_flag_prints_package_version() {
    let cwd = common::tmp_dir("version_flag_prints_package_version");
    let output = spawn_in(&cwd, &["--version"]);
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "stdout did not contain package version: {stdout:?}"
    );
}

#[test]
fn info_reports_fixture_directory() {
    let cwd = common::tmp_dir("info_reports_fixture_directory");
    let output = spawn_in(&cwd, &["info", mantis_512_dir().to_str().unwrap()]);
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("512x512 3ch 16-bit II compression=1 planar=1 photometric=2"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("rows_per_strip=1 strips=512 file_size=1577214"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("mmap fast path: yes"), "stdout: {stdout}");
    assert!(
        stdout.contains("8 TIFFs, 12617712 bytes total"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("uniform layout: yes"), "stdout: {stdout}");
    // frames/ descent: the top-level reference.tif must not be listed.
    assert!(
        !stdout.contains("reference.tif"),
        "top-level reference.tif must not be listed: {stdout}"
    );
}

#[test]
fn info_reports_single_file_fields() {
    let cwd = common::tmp_dir("info_reports_single_file_fields");
    let path = mantis_512_dir().join("frames").join("00.tif");
    let output = spawn_in(&cwd, &["info", path.to_str().unwrap()]);
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("512x512 3ch 16-bit II compression=1 planar=1 photometric=2"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("rows_per_strip=1 strips=512 file_size=1577214"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("mmap fast path: yes"), "stdout: {stdout}");
    // Single-file output has no directory summary.
    assert!(!stdout.contains("TIFFs,"), "stdout: {stdout}");
}

#[test]
fn info_reports_the_description() {
    let cwd = common::tmp_dir("info_reports_the_description");
    let path = mantis_64_dir().join("frames").join("00.tif");
    let expected = "description: FocusForge fixture mantis-64/00 from 20260829-IMG_9800.tif rect 4064,2700,64,64";

    let output = spawn_in(&cwd, &["info", path.to_str().unwrap()]);
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(expected), "stdout: {stdout}");

    let json_output = spawn_in(&cwd, &["info", "--json", path.to_str().unwrap()]);
    assert!(
        json_output.status.success(),
        "exit status: {:?}",
        json_output.status
    );
    let json_stdout = String::from_utf8_lossy(&json_output.stdout);
    let report: serde_json::Value = serde_json::from_str(&json_stdout).expect("valid JSON");
    assert_eq!(
        report["files"][0]["description"],
        "FocusForge fixture mantis-64/00 from 20260829-IMG_9800.tif rect 4064,2700,64,64"
    );
}

#[test]
fn info_json() {
    let cwd = common::tmp_dir("info_json");
    let output = spawn_in(
        &cwd,
        &["info", "--json", mantis_512_dir().to_str().unwrap()],
    );
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    for needle in [
        "\"frame_count\": 8",
        "\"total_bytes\": 12617712",
        "\"unreadable_count\": 0",
        "\"uniform_layout\": true",
        "\"uniform_detail\": null",
        "\"is_directory\": true",
        "\"rows_per_strip\": 1",
        "\"mmap_fast_path\": true",
        "\"error\": null",
    ] {
        assert!(stdout.contains(needle), "missing {needle:?} in: {stdout}");
    }
    let fast_path_count = stdout.matches("\"mmap_fast_path\": true").count();
    assert_eq!(fast_path_count, 8, "expected 8 fast-path entries: {stdout}");
}

/// `info --json` on a directory holding one valid frame and one broken
/// member: the broken row must carry all seventeen `FileReport` keys, with
/// every metadata field explicit `null` (no `skip_serializing_if`) except
/// `name`, `path` and `error`.
#[test]
fn info_json_nulls_every_metadata_field_on_a_broken_member() {
    let cwd = common::tmp_dir("info_json_nulls_every_metadata_field_on_a_broken_member_cwd");
    let tmp = common::tmp_dir("info_json_nulls_every_metadata_field_on_a_broken_member");
    std::fs::copy(
        mantis_64_dir().join("frames").join("00.tif"),
        tmp.join("00.tif"),
    )
    .unwrap();
    std::fs::write(tmp.join("broken.tif"), b"junkbyte").unwrap();

    let output = spawn_in(&cwd, &["info", "--json", tmp.to_str().unwrap()]);
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");

    assert_eq!(report["unreadable_count"], 1, "report: {report}");

    let files = report["files"].as_array().expect("files must be an array");
    let broken = files
        .iter()
        .find(|f| f["name"] == "broken.tif")
        .expect("broken.tif must be present in files");

    assert!(broken["path"].is_string(), "broken: {broken}");
    assert!(broken["error"].is_string(), "broken: {broken}");
    for key in [
        "width",
        "height",
        "bits_per_sample",
        "samples_per_pixel",
        "byte_order",
        "compression",
        "photometric",
        "planar_config",
        "rows_per_strip",
        "strip_count",
        "file_size",
        "description",
        "mmap_fast_path",
        "refusal",
    ] {
        assert!(
            broken.get(key).is_some_and(|v| v.is_null()),
            "expected key {key:?} to be present and null for a broken member, got {:?}",
            broken.get(key)
        );
    }
}

#[test]
fn info_reports_non_uniform_directory() {
    let cwd = common::tmp_dir("info_reports_non_uniform_directory_cwd");
    let tmp = common::tmp_dir("info_reports_non_uniform_directory");
    std::fs::copy(
        mantis_64_dir().join("reference.tif"),
        tmp.join("reference.tif"),
    )
    .unwrap();
    std::fs::copy(
        mantis_64_dir().join("reference-MM-rps8.tif"),
        tmp.join("reference-MM-rps8.tif"),
    )
    .unwrap();

    let output = spawn_in(&cwd, &["info", tmp.to_str().unwrap()]);
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("uniform layout: no — reference-MM-rps8.tif is MM/rps=8, others II/rps=1"),
        "stdout: {stdout}"
    );
}

/// Two members share byte order and `RowsPerStrip` (the only two fields the
/// report names) and disagree only on `Compression`: the
/// detail line must still name the field that actually differs, not repeat
/// an identical-looking `II/rps=1` on both sides.
#[test]
fn info_reports_non_uniform_directory_names_compression() {
    let cwd = common::tmp_dir("info_reports_non_uniform_directory_names_compression_cwd");
    let tmp = common::tmp_dir("info_reports_non_uniform_directory_names_compression");
    std::fs::copy(
        mantis_64_dir().join("frames").join("00.tif"),
        tmp.join("00.tif"),
    )
    .unwrap();
    std::fs::copy(
        mantis_64_dir().join("frames").join("01.tif"),
        tmp.join("01.tif"),
    )
    .unwrap();
    write_compressed_copy(
        &mantis_64_dir().join("frames").join("00.tif"),
        &tmp.join("02-compressed.tif"),
    );

    let output = spawn_in(&cwd, &["info", tmp.to_str().unwrap()]);
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(
            "uniform layout: no — 02-compressed.tif is II/rps=1/compression=5, others II/rps=1"
        ),
        "stdout: {stdout}"
    );
}

#[test]
fn info_skips_non_tiff_and_names_a_broken_tiff() {
    let cwd = common::tmp_dir("info_skips_non_tiff_and_names_a_broken_tiff_cwd");
    let tmp = common::tmp_dir("info_skips_non_tiff_and_names_a_broken_tiff");
    std::fs::copy(
        mantis_64_dir().join("frames").join("00.tif"),
        tmp.join("00.tif"),
    )
    .unwrap();
    std::fs::write(tmp.join("README.md"), b"not a frame").unwrap();
    std::fs::write(tmp.join("broken.tif"), b"junkbyte").unwrap();

    let output = spawn_in(&cwd, &["info", tmp.to_str().unwrap()]);
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("README.md"), "stdout: {stdout}");
    assert!(stdout.contains("broken.tif"), "stdout: {stdout}");
    assert!(stdout.contains("error:"), "stdout: {stdout}");
    assert!(stdout.contains("unreadable: 1"), "stdout: {stdout}");
}

#[test]
fn info_reads_headers_only() {
    let cwd = common::tmp_dir("info_reads_headers_only_cwd");
    // Truncate a copy of mantis-64/frames/00.tif to its 762-byte header:
    // header, IFD and both strip tables complete, zero pixel bytes.
    let source = mantis_64_dir().join("frames").join("00.tif");
    let bytes = std::fs::read(&source).expect("read fixture frame");
    let truncated = &bytes[..762];

    let holder = common::tmp_dir("info_reads_headers_only");
    let file_path = holder.join("info_reads_headers_only.tif");
    std::fs::write(&file_path, truncated).unwrap();

    let output = spawn_in(&cwd, &["info", file_path.to_str().unwrap()]);
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("64x64 3ch 16-bit II compression=1 planar=1 photometric=2"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("mmap fast path: yes"), "stdout: {stdout}");

    // Same proof for a directory containing the truncated file.
    let tmp = common::tmp_dir("info_reads_headers_only_dir");
    std::fs::write(tmp.join("truncated.tif"), truncated).unwrap();
    let output = spawn_in(&cwd, &["info", tmp.to_str().unwrap()]);
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("64x64 3ch 16-bit II"), "stdout: {stdout}");
}

#[test]
fn info_reports_written_file_as_fast_path() {
    let cwd = common::tmp_dir("info_reports_written_file_as_fast_path_cwd");
    let holder = common::tmp_dir("info_reports_written_file_as_fast_path");
    let out_path = holder.join("info_reports_written_file_as_fast_path.tif");
    let mut writer = TiffWriter::create(
        &out_path,
        TiffSpec {
            width: 4,
            height: 4,
            samples_per_pixel: 3,
            description: None,
        },
    )
    .expect("create must succeed");
    writer
        .write_tile(
            Rect {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            &[0u16; 4 * 4 * 3],
        )
        .expect("write_tile must succeed");
    writer.finish().expect("finish must succeed");

    let output = spawn_in(&cwd, &["info", out_path.to_str().unwrap()]);
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("mmap fast path: yes"), "stdout: {stdout}");
}

#[test]
fn info_reports_compressed_file_as_refused() {
    let cwd = common::tmp_dir("info_reports_compressed_file_as_refused_cwd");
    let holder = common::tmp_dir("info_reports_compressed_file_as_refused");
    let dest = holder.join("info_reports_compressed_file_as_refused.tif");
    write_compressed_copy(&mantis_64_dir().join("frames").join("00.tif"), &dest);

    let output = spawn_in(&cwd, &["info", dest.to_str().unwrap()]);
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("mmap fast path: no — Compression=5, only uncompressed (1) is supported"),
        "stdout: {stdout}"
    );
}

#[test]
fn info_exits_1_for_unparseable_single_file() {
    let cwd = common::tmp_dir("info_exits_1_for_unparseable_single_file_cwd");
    let holder = common::tmp_dir("info_exits_1_for_unparseable_single_file");
    let dest = holder.join("info_exits_1_for_unparseable_single_file");
    std::fs::write(&dest, b"not a tiff at all").unwrap();

    let output = spawn_in(&cwd, &["info", dest.to_str().unwrap()]);
    assert!(
        !output.status.success(),
        "expected exit 1, got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error:"), "stderr: {stderr}");
}

#[test]
fn info_exits_1_for_empty_directory() {
    let cwd = common::tmp_dir("info_exits_1_for_empty_directory_cwd");
    let tmp = common::tmp_dir("info_exits_1_for_empty_directory");

    let output = spawn_in(&cwd, &["info", tmp.to_str().unwrap()]);
    assert!(
        !output.status.success(),
        "expected exit 1, got {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error:"), "stderr: {stderr}");
}

/// With the `gpu` feature off, `stack` runs CPU-only:
/// no GPU probing, no `--cpu` flag, and the `backend` field is
/// always `"cpu"` -- run against `dummy-stack` (cheap: no pyramid, no
/// alignment, no crop) to keep the run inexpensive.
#[test]
fn stack_defaults_to_cpu() {
    let cwd = common::tmp_dir("stack_defaults_to_cpu");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();

    let output = spawn_in(
        &cwd,
        &[
            "stack",
            mantis_512_dir().to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "dummy-stack",
            "--no-align",
            "--no-crop",
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(report["backend"], "cpu", "report: {report}");
}

/// With the `gpu` feature off, `--cpu` does not exist on `stack` or
/// `align` -- it is refused, not silently accepted.
#[test]
#[cfg(not(feature = "gpu"))]
fn no_gpu_flag_absent_without_the_feature() {
    let cwd = common::tmp_dir("no_gpu_flag_absent_without_the_feature");
    for sub in ["stack", "align"] {
        let output = spawn_in(&cwd, &[sub, "--help"]);
        assert!(output.status.success(), "exit status: {:?}", output.status);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !stdout.contains("--cpu"),
            "{sub} --help must not mention --cpu without the gpu feature: {stdout}"
        );
    }
}

#[test]
#[ignore = "reads the 16 GB reference stack"]
fn info_reads_the_reference_stack() {
    let Some(stack_dir) = common::reference_stack_dir() else {
        return;
    };

    let cwd = common::tmp_dir("info_reads_the_reference_stack");
    let output = spawn_in(&cwd, &["info", "--json", stack_dir.to_str().unwrap()]);
    assert!(output.status.success(), "exit status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("\"frame_count\": 62"), "stdout: {stdout}");
    assert!(
        stdout.contains("\"uniform_layout\": false"),
        "stdout: {stdout}"
    );
    let strip_5464 = stdout.matches("\"strip_count\": 5464").count();
    assert_eq!(
        strip_5464, 61,
        "expected 61 frames at strip_count 5464: {stdout}"
    );
    assert!(stdout.contains("\"strip_count\": 683"), "stdout: {stdout}");
}
