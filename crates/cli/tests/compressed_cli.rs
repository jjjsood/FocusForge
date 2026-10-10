// SPDX-License-Identifier: GPL-3.0-or-later

mod common;

use std::path::{Path, PathBuf};

use focusforge_core::io::{Rect, TiffStack};

fn info(path: &std::path::Path) -> serde_json::Value {
    let output = info_output(path, true);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn info_output(path: &Path, json: bool) -> std::process::Output {
    let mut command = common::cli_bin("compressed_cli");
    command.args(["info", path.to_str().unwrap()]);
    if json {
        command.arg("--json");
    }
    command.output().unwrap()
}

#[test]
fn info_reports_the_serving_reader() {
    let fixtures = common::fixtures_dir();
    let mmap = info(&fixtures.join("mantis-64/frames/00.tif"));
    assert_eq!(mmap["files"][0]["reader"], "TiffStack");
    assert_eq!(mmap["files"][0]["refusal"], serde_json::Value::Null);

    let compressed = info(&fixtures.join("mantis-64-deflate-p1-strip-ii/frames/00.tif"));
    assert_eq!(compressed["files"][0]["reader"], "DecodedTiffStack");
    assert!(
        compressed["files"][0]["refusal"]
            .as_str()
            .is_some_and(|value| value.contains("Compression=8"))
    );
    let human = info_output(
        &fixtures.join("mantis-64-deflate-p1-strip-ii/frames/00.tif"),
        false,
    );
    assert!(human.status.success());
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(stdout.contains("reader: DecodedTiffStack"), "{stdout}");
    assert!(
        stdout.contains("mmap fast path: no — Compression=8"),
        "{stdout}"
    );
}

#[test]
fn info_covers_supported_unsupported_and_header_only_inputs() {
    let fixtures = common::fixtures_dir();
    let mut sets = std::fs::read_dir(&fixtures)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("mantis-64-"))
        })
        .collect::<Vec<_>>();
    sets.sort();
    assert_eq!(sets.len(), 24);
    for set in sets {
        let report = info(&set.join("frames/00.tif"));
        assert_eq!(report["files"][0]["reader"], "DecodedTiffStack");
        assert!(report["files"][0]["refusal"].is_string());
    }

    let root = common::tmp_dir("compressed_cli_info_headers");
    let unsupported = root.join("unsupported.tif");
    let mut bytes =
        std::fs::read(fixtures.join("mantis-64-lzw-p1-strip-ii/frames/00.tif")).unwrap();
    patch_short_tag(&mut bytes, 259, 7);
    std::fs::write(&unsupported, bytes).unwrap();
    let report = info(&unsupported);
    assert!(report["files"][0]["reader"].is_null());
    assert!(
        report["files"][0]["refusal"]
            .as_str()
            .is_some_and(|value| value.contains("Compression=7"))
    );

    let header_only = root.join("header-only.tif");
    let bytes =
        std::fs::read(fixtures.join("mantis-64-deflate-p1-strip-ii/frames/00.tif")).unwrap();
    std::fs::write(&header_only, &bytes[..1024]).unwrap();
    let report = info(&header_only);
    assert_eq!(report["files"][0]["reader"], "DecodedTiffStack");
    assert!(info_output(&header_only, false).status.success());

    let malformed = root.join("malformed.tif");
    std::fs::write(&malformed, b"not a TIFF").unwrap();
    let output = info_output(&malformed, true);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not a TIFF"));
}

fn run_stack(cwd: &Path, input: &Path, output: &Path) -> PathBuf {
    std::fs::create_dir_all(output).unwrap();
    let mut command = common::cli_bin("compressed_cli_stack");
    let args = common::push_no_gpu(vec![
        "stack",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
        "--algorithm",
        "laplacian",
        "--no-align",
        "--no-crop",
    ]);
    command.current_dir(cwd).args(args);
    let output_status = command.output().unwrap();
    assert!(
        output_status.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output_status.stderr)
    );
    output.join(format!(
        "{}.ffp",
        input.file_name().unwrap().to_string_lossy()
    ))
}

fn export_planes(cwd: &Path, project: &Path) -> (PathBuf, PathBuf) {
    let composite = project.with_extension("composite.tif");
    let zmap = project.with_extension("zmap.tif");
    let output = common::cli_bin("compressed_cli_export")
        .current_dir(cwd)
        .args([
            "export",
            project.to_str().unwrap(),
            "-o",
            composite.to_str().unwrap(),
            "--zmap",
            zmap.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (composite, zmap)
}

fn samples(path: &Path) -> Vec<u16> {
    let reader = TiffStack::open_files(&[path.to_path_buf()]).unwrap();
    let (width, height) = reader.dimensions();
    reader
        .read_tile(
            0,
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
        )
        .unwrap()
        .samples
}

#[test]
fn stack_output_matches_uncompressed_twin() {
    let cwd = common::tmp_dir("compressed_cli_stack");
    let fixtures = common::fixtures_dir();
    let reference = run_stack(&cwd, &fixtures.join("mantis-64"), &cwd.join("reference"));
    let (reference_composite, reference_zmap) = export_planes(&cwd, &reference);
    let expected = (samples(&reference_composite), samples(&reference_zmap));
    let mut sets = std::fs::read_dir(&fixtures)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("mantis-64-"))
        })
        .collect::<Vec<_>>();
    sets.sort();
    assert_eq!(sets.len(), 24);

    for set in sets {
        let project = run_stack(&cwd, &set, &cwd.join(set.file_name().unwrap()));
        let (composite, zmap) = export_planes(&cwd, &project);
        assert_eq!(
            samples(&composite),
            expected.0,
            "{} composite",
            set.display()
        );
        assert_eq!(samples(&zmap), expected.1, "{} Z-Map", set.display());
    }
}

#[test]
fn compressed_group_resume_and_export() {
    let cwd = common::tmp_dir("compressed_cli_group_export");
    let fixtures = common::fixtures_dir();
    let output = cwd.join("output");
    let first = run_stack(
        &cwd,
        &fixtures.join("mantis-64-deflate-p1-strip-ii"),
        &output,
    );
    let first_planes = export_planes(&cwd, &first);
    let first_samples = (samples(&first_planes.0), samples(&first_planes.1));
    let first_manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(output.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(first_manifest["groups"].as_array().unwrap().len(), 1);
    assert_eq!(first_manifest["groups"][0]["outcome"], "completed");
    assert_eq!(
        first_manifest["groups"][0]["outputs"][0],
        first.display().to_string()
    );
    assert!(first.is_file());
    let second = run_stack(
        &cwd,
        &fixtures.join("mantis-64-deflate-p1-strip-ii"),
        &output,
    );
    // The repeated group keeps its stored-project identity in the run manifest.
    assert_eq!(second, first);
    assert!(second.is_file());
    let second_planes = export_planes(&cwd, &second);
    assert_eq!(first_samples.0, samples(&second_planes.0));
    assert_eq!(first_samples.1, samples(&second_planes.1));
    let second_manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(output.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(second_manifest["groups"].as_array().unwrap().len(), 1);
    assert_eq!(
        second_manifest["groups"][0]["identity"],
        first_manifest["groups"][0]["identity"]
    );
    assert_eq!(
        second_manifest["groups"][0]["outputs"],
        first_manifest["groups"][0]["outputs"]
    );
}

#[test]
fn changed_tile_size_keeps_compressed_output_equal() {
    let cwd = common::tmp_dir("compressed_cli_tile_size");
    let fixtures = common::fixtures_dir();
    let run = |input: &Path, output: &Path| {
        std::fs::create_dir_all(output).unwrap();
        let args = common::push_no_gpu(vec![
            "stack",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--algorithm",
            "laplacian",
            "--no-align",
            "--no-crop",
            "--tile-size",
            "80",
            "--set",
            "levels=1",
        ]);
        let result = common::cli_bin("compressed_cli_tile_size")
            .current_dir(&*cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        output.join(format!(
            "{}.ffp",
            input.file_name().unwrap().to_string_lossy()
        ))
    };
    let mmap = run(&fixtures.join("mantis-64"), &cwd.join("mmap"));
    let decoded = run(
        &fixtures.join("mantis-64-deflate-p1-strip-ii"),
        &cwd.join("decoded"),
    );
    let mmap_planes = export_planes(&cwd, &mmap);
    let decoded_planes = export_planes(&cwd, &decoded);
    assert_eq!(samples(&mmap_planes.0), samples(&decoded_planes.0));
    assert_eq!(samples(&mmap_planes.1), samples(&decoded_planes.1));
}

#[test]
fn mixed_compression_set_is_handled() {
    let cwd = common::tmp_dir("compressed_cli_mixed");
    let fixtures = common::fixtures_dir();
    let mixed = cwd.join("mixed");
    let reference = cwd.join("reference");
    std::fs::create_dir_all(&mixed).unwrap();
    std::fs::create_dir_all(&reference).unwrap();
    for (name, source, oracle) in [
        (
            "00.tif",
            fixtures.join("mantis-64/frames/00.tif"),
            fixtures.join("mantis-64/frames/00.tif"),
        ),
        (
            "01.tif",
            fixtures.join("mantis-64-lzw-p1-strip-ii/frames/01.tif"),
            fixtures.join("mantis-64/frames/01.tif"),
        ),
        (
            "02.tif",
            fixtures.join("mantis-64-deflate-p2-tile-mm/frames/02.tif"),
            fixtures.join("mantis-64/frames/02.tif"),
        ),
    ] {
        std::fs::copy(source, mixed.join(name)).unwrap();
        std::fs::copy(oracle, reference.join(name)).unwrap();
    }

    let mixed_project = run_stack(&cwd, &mixed, &cwd.join("mixed-output"));
    let reference_project = run_stack(&cwd, &reference, &cwd.join("reference-output"));
    let (mixed_composite, mixed_zmap) = export_planes(&cwd, &mixed_project);
    let (reference_composite, reference_zmap) = export_planes(&cwd, &reference_project);
    assert_eq!(samples(&mixed_composite), samples(&reference_composite));
    assert_eq!(samples(&mixed_zmap), samples(&reference_zmap));

    let decoded_first = cwd.join("decoded-first");
    std::fs::create_dir_all(&decoded_first).unwrap();
    for (name, source) in [
        (
            "00.tif",
            fixtures.join("mantis-64-deflate-p2-tile-mm/frames/00.tif"),
        ),
        ("01.tif", fixtures.join("mantis-64/frames/01.tif")),
        (
            "02.tif",
            fixtures.join("mantis-64-lzw-p1-strip-ii/frames/02.tif"),
        ),
    ] {
        std::fs::copy(source, decoded_first.join(name)).unwrap();
    }
    let decoded_first_project = run_stack(&cwd, &decoded_first, &cwd.join("decoded-first-output"));
    let (composite, zmap) = export_planes(&cwd, &decoded_first_project);
    assert_eq!(samples(&composite), samples(&reference_composite));
    assert_eq!(samples(&zmap), samples(&reference_zmap));
}

fn dry_run_plan(cwd: &Path, input: &Path) -> serde_json::Value {
    let output = cwd.join("dry-run-output");
    std::fs::create_dir_all(&output).unwrap();
    let mut command = common::cli_bin("compressed_cli_dry_run");
    command.current_dir(cwd).args([
        "stack",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
        "--algorithm",
        "laplacian",
        "--no-align",
        "--no-crop",
        "--dry-run",
        "--tile-size",
        "80",
        "--set",
        "levels=1",
        "--max-host-memory",
        "65536",
        "--json",
    ]);
    if cfg!(feature = "gpu") {
        command.arg("--cpu");
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn dry_run_uses_decoded_scheduler_share() {
    let cwd = common::tmp_dir("compressed_cli_dry_run");
    let fixtures = common::fixtures_dir();
    let mmap = dry_run_plan(&cwd, &fixtures.join("mantis-64"));
    let decoded = dry_run_plan(&cwd, &fixtures.join("mantis-64-deflate-p1-strip-ii"));
    assert_eq!(mmap["max_bytes_in_flight"], 65_536);
    assert_eq!(decoded["max_bytes_in_flight"], 49_152);
    for plan in [&mmap, &decoded] {
        for output in plan["would_write"].as_array().unwrap() {
            assert!(!cwd.join(output.as_str().unwrap()).exists());
        }
    }
}

fn align_report(cwd: &Path, input: &Path) -> serde_json::Value {
    let output = common::cli_bin("compressed_cli_align")
        .current_dir(cwd)
        .args([
            "align",
            input.to_str().unwrap(),
            "--algorithm",
            "pyramidal-ecc",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn align_matches_uncompressed_twin() {
    let cwd = common::tmp_dir("compressed_cli_align");
    let fixtures = common::fixtures_dir();
    let mmap = align_report(&cwd, &fixtures.join("mantis-64"));
    let decoded = align_report(&cwd, &fixtures.join("mantis-64-deflate-p1-strip-ii"));
    assert_eq!(decoded["alignments"], mmap["alignments"]);
}

#[test]
fn review_reads_compressed_sources() {
    let cwd = common::tmp_dir("compressed_cli_review");
    let fixtures = common::fixtures_dir();
    let output = cwd.join("output");
    let project = run_stack(
        &cwd,
        &fixtures.join("mantis-64-deflate-p1-strip-ii"),
        &output,
    );
    assert!(project.is_file());
    let review = common::cli_bin("compressed_cli_review")
        .current_dir(&*cwd)
        .args(["review", output.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(
        review.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&review.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&review.stdout).unwrap();
    assert!(!report["results"][0]["metrics"].is_null(), "{report}");
    let reference_output = cwd.join("reference-output");
    let _reference = run_stack(&cwd, &fixtures.join("mantis-64"), &reference_output);
    let review = common::cli_bin("compressed_cli_review")
        .current_dir(&*cwd)
        .args(["review", reference_output.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(review.status.success());
    let reference: serde_json::Value = serde_json::from_slice(&review.stdout).unwrap();
    for key in ["composite_lapvar", "max_input_lapvar", "sharpness_ratio"] {
        assert_eq!(
            report["results"][0]["metrics"][key],
            reference["results"][0]["metrics"][key]
        );
        assert!(report["results"][0]["metrics"][key].is_number());
    }
}

#[test]
fn refused_codec_fails_only_its_group() {
    let cwd = common::tmp_dir("compressed_cli_refused_group");
    let fixtures = common::fixtures_dir();
    let input = cwd.join("input");
    let output = cwd.join("output");
    std::fs::create_dir_all(&input).unwrap();
    for name in ["00.tif", "01.tif", "02.tif"] {
        std::fs::copy(
            fixtures.join("mantis-64/frames").join(name),
            input.join(name),
        )
        .unwrap();
    }
    let mut refused =
        std::fs::read(fixtures.join("mantis-64-lzw-p1-strip-ii/frames/00.tif")).unwrap();
    patch_short_tag(&mut refused, 259, 7);
    std::fs::write(input.join("refused.tif"), refused).unwrap();
    let groups = cwd.join("groups.toml");
    std::fs::write(
        &groups,
        "[[group]]\nname = \"valid\"\nframes = [\"00.tif\", \"01.tif\", \"02.tif\"]\n\n[[group]]\nname = \"refused\"\nframes = [\"refused.tif\"]\n",
    )
    .unwrap();
    std::fs::create_dir_all(&output).unwrap();
    let mut command = common::cli_bin("compressed_cli_refused_group");
    command.current_dir(&*cwd).args([
        "stack",
        input.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
        "--groups",
        groups.to_str().unwrap(),
        "--algorithm",
        "laplacian",
        "--no-align",
        "--no-crop",
    ]);
    if cfg!(feature = "gpu") {
        command.arg("--cpu");
    }
    let result = command.output().unwrap();
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("refused.tif") && stderr.contains("Compression=7"),
        "{stderr}"
    );
    assert!(output.join("valid.ffp").is_file(), "{stderr}");
}

fn patch_short_tag(bytes: &mut [u8], wanted_tag: u16, value: u16) {
    assert_eq!(&bytes[..2], b"II");
    let ifd = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let entries = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
    for index in 0..entries {
        let offset = ifd + 2 + index * 12;
        if u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap()) != wanted_tag {
            continue;
        }
        assert_eq!(
            u16::from_le_bytes(bytes[offset + 2..offset + 4].try_into().unwrap()),
            3
        );
        let count = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
        let data = if count * 2 <= 4 {
            offset + 8
        } else {
            u32::from_le_bytes(bytes[offset + 8..offset + 12].try_into().unwrap()) as usize
        };
        bytes[data..data + 2].copy_from_slice(&value.to_le_bytes());
        return;
    }
    panic!("missing TIFF tag {wanted_tag}");
}
