// SPDX-License-Identifier: GPL-3.0-or-later

mod common;

#[path = "../../core/tests/common/raw_dng.rs"]
mod raw_dng;

use std::process::Command;

#[cfg(unix)]
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::process::Stdio;

use focusforge_core::algorithm::TileReader;
use focusforge_core::io::{RawStack, Rect, TiffStack};
use raw_dng::{DngSpec, write_dng};

fn run(dir: &std::path::Path, args: &[String]) -> std::process::Output {
    common::cli_bin("stack_raw")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run CLI")
}

#[test]
fn identical_raw_frames_stack_to_the_frame() {
    let cwd = common::tmp_dir("stack_raw_identical");
    let source = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 64]),
        false,
    );
    for count in [3, 8] {
        let input = cwd.join(format!("input-{count}"));
        let output = cwd.join(format!("output-{count}"));
        std::fs::create_dir(&input).expect("input directory");
        std::fs::create_dir(&output).expect("output directory");
        for index in 0..count {
            std::fs::copy(source.path(), input.join(format!("{index:02}.dng")))
                .expect("copy RAW frame");
        }

        let mut args = vec![
            "stack".to_string(),
            input.display().to_string(),
            "-o".to_string(),
            output.display().to_string(),
            "--algorithm".to_string(),
            "laplacian".to_string(),
            "--no-align".to_string(),
            "--no-crop".to_string(),
        ];
        if cfg!(feature = "gpu") {
            args.push("--cpu".to_string());
        }
        let stacked = run(&cwd, &args);
        assert!(
            stacked.status.success(),
            "stack stderr: {}",
            String::from_utf8_lossy(&stacked.stderr)
        );
        let project = output.join(format!("input-{count}.ffp"));
        assert!(project.is_file());
        let composite = output.join("composite.tif");
        let exported = Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
            .current_dir(&*cwd)
            .args([
                "export",
                project.to_str().expect("UTF-8 project"),
                "-o",
                composite.to_str().expect("UTF-8 composite"),
            ])
            .output()
            .expect("run export");
        assert!(
            exported.status.success(),
            "export stderr: {}",
            String::from_utf8_lossy(&exported.stderr)
        );
        let output_stack = TiffStack::open_files(&[composite]).expect("exported composite");
        let (width, height) = output_stack.dimensions();
        assert_eq!(
            output_stack.header(0).bits_per_sample.as_ref(),
            &[16, 16, 16]
        );
        let composite = output_stack
            .read_tile(
                0,
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
            )
            .expect("read composite")
            .samples;
        let source_reader = RawStack::open_files(&[input.join("00.dng")]).expect("source RAW");
        let mut developed = vec![0; composite.len()];
        source_reader
            .read_tile_into(
                0,
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                &mut developed,
            )
            .expect("develop source RAW");
        let max_diff = composite
            .iter()
            .zip(&developed)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0);
        eprintln!("raw stack frames={count} max_diff={max_diff}");
        assert!(max_diff <= 1, "{count} frames: max difference {max_diff}");

        let review = Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
            .current_dir(&*cwd)
            .args(["review", output.to_str().expect("UTF-8 output"), "--json"])
            .output()
            .expect("run review");
        assert!(review.status.success());
        assert!(String::from_utf8_lossy(&review.stdout).contains("\"outcome\": \"completed\""));
    }
}

#[test]
fn non_uniform_raw_group_is_refused_by_name() {
    let cwd = common::tmp_dir("stack_raw_non_uniform");
    let input = cwd.join("input");
    let output = cwd.join("output");
    std::fs::create_dir(&input).expect("input directory");
    std::fs::create_dir(&output).expect("output directory");
    let first = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 64]),
        false,
    );
    std::fs::copy(first.path(), input.join("00.dng")).expect("copy first RAW");
    let cases = [
        (
            "CFA",
            DngSpec::new(12, 64, 4095, [2, 1, 1, 0], vec![512; 64]),
        ),
        ("camera", {
            let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 64]);
            spec.model = "Different Bayer".to_string();
            spec
        }),
        ("dimensions", {
            let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 64]);
            spec.crop = Some([0, 0, 4, 4]);
            spec
        }),
    ];
    for (property, spec) in cases {
        let second = write_dng(&spec, false);
        std::fs::copy(second.path(), input.join("01.dng")).expect("copy second RAW");
        let result = run(
            &cwd,
            &[
                "stack".to_string(),
                input.display().to_string(),
                "-o".to_string(),
                output.display().to_string(),
                "--algorithm".to_string(),
                "dummy-stack".to_string(),
                "--no-align".to_string(),
                "--no-crop".to_string(),
            ],
        );
        assert!(!result.status.success());
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            stderr.contains("01.dng") && stderr.contains(&format!("RAW {property}")),
            "{stderr}"
        );
    }
}

#[test]
fn forced_mixed_group_is_refused_by_name() {
    let cwd = common::tmp_dir("stack_raw_mixed");
    let input = cwd.join("input");
    let output = cwd.join("output");
    std::fs::create_dir(&input).expect("input directory");
    std::fs::create_dir(&output).expect("output directory");
    let raw = write_dng(
        &DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 64]),
        false,
    );
    std::fs::copy(raw.path(), input.join("00.dng")).expect("copy RAW");
    std::fs::copy(
        common::fixtures_dir().join("mantis-64/frames/00.tif"),
        input.join("01.tif"),
    )
    .expect("copy TIFF");
    let groups = cwd.join("mixed.toml");
    std::fs::write(
        &groups,
        "[[group]]\nname = \"mixed\"\nframes = [\"00.dng\", \"01.tif\"]\n",
    )
    .expect("write group file");
    let result = run(
        &cwd,
        &[
            "stack".to_string(),
            input.display().to_string(),
            "-o".to_string(),
            output.display().to_string(),
            "--groups".to_string(),
            groups.display().to_string(),
            "--algorithm".to_string(),
            "dummy-stack".to_string(),
            "--no-align".to_string(),
            "--no-crop".to_string(),
        ],
    );
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("01.tif") && stderr.contains("TIFF"),
        "{stderr}"
    );
}

#[test]
fn info_and_stack_refuse_a_directory_with_only_prior_tiff_outputs() {
    let cwd = common::tmp_dir("stack_raw_prior_outputs");
    let input = cwd.join("input");
    let output = cwd.join("output");
    std::fs::create_dir(&input).expect("input directory");
    std::fs::create_dir(&output).expect("output directory");
    for name in ["composite.tif", "zmap.tif"] {
        std::fs::write(input.join(name), b"prior output").expect("write output marker");
    }

    let info = Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
        .current_dir(&*cwd)
        .args(["info", input.to_str().expect("UTF-8 input"), "--json"])
        .output()
        .expect("run info");
    assert!(!info.status.success());
    assert!(
        String::from_utf8_lossy(&info.stderr).contains("frames found"),
        "{}",
        String::from_utf8_lossy(&info.stderr)
    );

    let stacked = run(
        &cwd,
        &[
            "stack".to_string(),
            input.display().to_string(),
            "-o".to_string(),
            output.display().to_string(),
            "--algorithm".to_string(),
            "dummy-stack".to_string(),
        ],
    );
    assert!(!stacked.status.success());
    assert!(String::from_utf8_lossy(&stacked.stderr).contains("frames found"));
}

#[cfg(unix)]
#[test]
#[ignore = "requires a long RAW decode run"]
fn raw_stack_resumes_and_hits_stage_cache() {
    let cwd = common::tmp_dir("stack_raw_resume");
    let input = cwd.join("input");
    let reference_out = cwd.join("reference");
    let resumed_out = cwd.join("resumed");
    std::fs::create_dir(&input).expect("input directory");
    std::fs::create_dir(&reference_out).expect("reference output directory");
    std::fs::create_dir(&resumed_out).expect("resumed output directory");
    let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], vec![512; 1024 * 1024]);
    spec.width = 1024;
    spec.height = 1024;
    let source = write_dng(&spec, false);
    for index in 0..3 {
        std::fs::copy(source.path(), input.join(format!("{index:02}.dng")))
            .expect("copy RAW frame");
    }
    let mut args = vec![
        "stack".to_string(),
        input.display().to_string(),
        "-o".to_string(),
        resumed_out.display().to_string(),
        "--algorithm".to_string(),
        "laplacian".to_string(),
        "--no-align".to_string(),
        "--no-crop".to_string(),
        "--tile-size".to_string(),
        "160".to_string(),
        "--set".to_string(),
        "levels=3".to_string(),
        "--max-host-memory".to_string(),
        "536870912".to_string(),
    ];
    if cfg!(feature = "gpu") {
        args.push("--cpu".to_string());
    }
    let reference_args = {
        let mut reference = args.clone();
        let output_index = reference.iter().position(|arg| arg == "-o").expect("-o");
        reference[output_index + 1] = reference_out.display().to_string();
        reference
    };
    let reference = run(&cwd, &reference_args);
    assert!(
        reference.status.success(),
        "{}",
        String::from_utf8_lossy(&reference.stderr)
    );

    let mut child = common::cli_bin("stack_raw_resume")
        .current_dir(&*cwd)
        .args(&args)
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn stack");
    let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
    let mut line = String::new();
    loop {
        line.clear();
        stderr.read_line(&mut line).expect("read stderr");
        assert!(!line.is_empty(), "stack ended before fusion: {line}");
        if line.starts_with("fuse ") {
            break;
        }
    }
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(killed.success());
    assert_eq!(child.wait().expect("wait stack").code(), Some(130));

    args.push("--resume".to_string());
    let resumed = run(&cwd, &args);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let reference_project = reference_out.join("input.ffp");
    let resumed_project = resumed_out.join("input.ffp");
    let reference_tiff = reference_out.join("reference.tif");
    let resumed_tiff = resumed_out.join("resumed.tif");
    for (project, output) in [
        (&reference_project, &reference_tiff),
        (&resumed_project, &resumed_tiff),
    ] {
        let export = Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
            .current_dir(&*cwd)
            .args([
                "export",
                project.to_str().expect("project"),
                "-o",
                output.to_str().expect("output"),
            ])
            .output()
            .expect("export composite");
        assert!(
            export.status.success(),
            "{}",
            String::from_utf8_lossy(&export.stderr)
        );
    }
    assert_eq!(
        std::fs::read(reference_tiff).expect("reference TIFF"),
        std::fs::read(resumed_tiff).expect("resumed TIFF")
    );

    let cache_a = cwd.join("cache-a");
    let cache_b = cwd.join("cache-b");
    let cache_c = cwd.join("cache-c");
    for output in [&cache_a, &cache_b, &cache_c] {
        std::fs::create_dir(output).expect("cache output directory");
    }
    let cache_args = |output: &std::path::Path| {
        let mut args = vec![
            "stack".to_string(),
            input.display().to_string(),
            "-o".to_string(),
            output.display().to_string(),
            "--algorithm".to_string(),
            "laplacian".to_string(),
            "--align-algorithm".to_string(),
            "dummy-align".to_string(),
            "--no-crop".to_string(),
            "--tile-size".to_string(),
            "160".to_string(),
            "--set".to_string(),
            "levels=3".to_string(),
            "--max-host-memory".to_string(),
            "536870912".to_string(),
            "--json".to_string(),
        ];
        if cfg!(feature = "gpu") {
            args.push("--cpu".to_string());
        }
        args
    };
    let first_cache = run(&cwd, &cache_args(&cache_a));
    assert!(
        first_cache.status.success(),
        "{}",
        String::from_utf8_lossy(&first_cache.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&first_cache.stdout).expect("first JSON")["align_cached"],
        false
    );
    let second_cache = run(&cwd, &cache_args(&cache_b));
    assert!(
        second_cache.status.success(),
        "{}",
        String::from_utf8_lossy(&second_cache.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&second_cache.stdout).expect("second JSON")["align_cached"],
        true
    );
    assert!(String::from_utf8_lossy(&second_cache.stderr).contains("reused from the stage cache"));

    spec.serial_number = Some("changed-body".to_string());
    let changed = write_dng(&spec, false);
    std::fs::copy(changed.path(), input.join("00.dng")).expect("replace metadata");
    let invalidated = run(&cwd, &cache_args(&cache_c));
    assert!(
        invalidated.status.success(),
        "{}",
        String::from_utf8_lossy(&invalidated.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&invalidated.stdout).expect("invalidated JSON")
            ["align_cached"],
        false
    );
}

#[test]
#[ignore = "reads the configured RAW corpus and may use the GPU"]
fn corpus_frame_stacks_on_default_backend() {
    let samples = std::env::var_os("FOCUSFORGE_RAW_SAMPLES")
        .expect("FOCUSFORGE_RAW_SAMPLES is required for the corpus stack test");
    let source = std::path::PathBuf::from(samples).join("canon-eos-20d.cr2");
    assert!(
        source.is_file(),
        "missing corpus RAW frame: {}",
        source.display()
    );
    let cwd = common::tmp_dir("stack_raw_corpus");
    let input = cwd.join("input");
    let default_out = cwd.join("default");
    let cpu_out = cwd.join("cpu");
    std::fs::create_dir(&input).expect("input directory");
    std::fs::create_dir(&default_out).expect("default output directory");
    std::fs::create_dir(&cpu_out).expect("CPU output directory");
    for name in ["00.cr2", "01.cr2"] {
        std::fs::copy(&source, input.join(name)).expect("copy corpus RAW");
    }
    let base = vec![
        "stack".to_string(),
        input.display().to_string(),
        "-o".to_string(),
        default_out.display().to_string(),
        "--algorithm".to_string(),
        "laplacian".to_string(),
        "--no-align".to_string(),
        "--no-crop".to_string(),
        "--json".to_string(),
    ];
    let default = run(&cwd, &base);
    assert!(
        default.status.success(),
        "{}",
        String::from_utf8_lossy(&default.stderr)
    );
    let default_report: serde_json::Value =
        serde_json::from_slice(&default.stdout).expect("default stack JSON");
    let backend = default_report["backend"]
        .as_str()
        .expect("default report backend");
    let adapter_or_fallback = if backend == "cpu" {
        "cpu-fallback"
    } else {
        backend
    };
    eprintln!(
        "corpus selected_backend={backend} selected_adapter_or_fallback={adapter_or_fallback}"
    );
    let mut cpu_args = base.clone();
    let output_index = cpu_args.iter().position(|arg| arg == "-o").expect("-o");
    cpu_args[output_index + 1] = cpu_out.display().to_string();
    if cfg!(feature = "gpu") {
        cpu_args.push("--cpu".to_string());
    }
    let cpu = run(&cwd, &cpu_args);
    assert!(
        cpu.status.success(),
        "{}",
        String::from_utf8_lossy(&cpu.stderr)
    );
    let mut exported = Vec::new();
    for (out, name) in [(&default_out, "default.tif"), (&cpu_out, "cpu.tif")] {
        let project = out.join("input.ffp");
        let tiff = out.join(name);
        let export = Command::new(env!("CARGO_BIN_EXE_focusforge-cli"))
            .current_dir(&*cwd)
            .args([
                "export",
                project.to_str().expect("project"),
                "-o",
                tiff.to_str().expect("TIFF"),
            ])
            .output()
            .expect("export composite");
        assert!(
            export.status.success(),
            "{}",
            String::from_utf8_lossy(&export.stderr)
        );
        let stack = TiffStack::open_files(&[tiff]).expect("exported TIFF");
        let (width, height) = stack.dimensions();
        exported.push(
            stack
                .read_tile(
                    0,
                    Rect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    },
                )
                .expect("read composite")
                .samples,
        );
    }
    let max_diff = exported[0]
        .iter()
        .zip(&exported[1])
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    eprintln!("corpus CPU/default max_diff={max_diff}");
    assert!(max_diff <= 1, "CPU/default max difference {max_diff}");

    let low_memory = run(
        &cwd,
        &[
            "stack".to_string(),
            input.display().to_string(),
            "-o".to_string(),
            cpu_out.display().to_string(),
            "--algorithm".to_string(),
            "laplacian".to_string(),
            "--no-align".to_string(),
            "--no-crop".to_string(),
            "--max-host-memory".to_string(),
            "1".to_string(),
        ],
    );
    assert!(!low_memory.status.success());
    let low_memory_stderr = String::from_utf8_lossy(&low_memory.stderr);
    eprintln!("corpus low_memory_refusal={low_memory_stderr}");
    assert!(low_memory_stderr.contains("RAW cache bound"));
}
