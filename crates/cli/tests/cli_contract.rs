// SPDX-License-Identifier: GPL-3.0-or-later

mod common;

fn tiff_long_entry(bytes: &[u8], tag: u16) -> usize {
    assert_eq!(
        &bytes[..4],
        b"II*\0",
        "test TIFF must be classic little-endian"
    );
    let ifd = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let entries = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
    for index in 0..entries {
        let entry = ifd + 2 + index * 12;
        let entry_tag = u16::from_le_bytes(bytes[entry..entry + 2].try_into().unwrap());
        if entry_tag == tag {
            assert_eq!(
                u16::from_le_bytes(bytes[entry + 2..entry + 4].try_into().unwrap()),
                4,
                "tag {tag} must be a TIFF LONG"
            );
            assert_eq!(
                u32::from_le_bytes(bytes[entry + 4..entry + 8].try_into().unwrap()),
                1,
                "tag {tag} must have one value"
            );
            return entry + 8;
        }
    }
    panic!("test TIFF is missing tag {tag}");
}

fn write_sparse_memory_witness(path: &std::path::Path) {
    use focusforge_core::io::{TiffSpec, TiffWriter};

    let total_memory = focusforge_core::host::snapshot().total_memory();
    assert!(
        total_memory > 0,
        "the host-memory witness needs a live host probe"
    );
    // `bytes_per_registration` includes this 517-row RGB stream term. This
    // width makes that term exceed even total host RAM before any pixel read.
    let width = (total_memory / (517 * 13) + 1).max(16_777_216);
    let width = u32::try_from(width).expect("host is too large for a classic TIFF witness");
    let strip_bytes = width
        .checked_mul(6)
        .expect("witness strip must fit classic TIFF byte counts");

    let mut writer = TiffWriter::create(
        path,
        TiffSpec {
            width: 1,
            height: 1,
            samples_per_pixel: 3,
            description: None,
        },
    )
    .expect("tiny test TIFF must be creatable");
    writer
        .write_tile(common::full_rect(1, 1), &[0, 0, 0])
        .expect("tiny test TIFF must accept its pixel");
    writer.finish().expect("tiny test TIFF must finish");

    let mut bytes = std::fs::read(path).expect("tiny test TIFF must be readable");
    let width_entry = tiff_long_entry(&bytes, 256);
    let strip_offset_entry = tiff_long_entry(&bytes, 273);
    let strip_bytes_entry = tiff_long_entry(&bytes, 279);
    let strip_offset = u32::from_le_bytes(
        bytes[strip_offset_entry..strip_offset_entry + 4]
            .try_into()
            .unwrap(),
    );
    bytes[width_entry..width_entry + 4].copy_from_slice(&width.to_le_bytes());
    bytes[strip_bytes_entry..strip_bytes_entry + 4].copy_from_slice(&strip_bytes.to_le_bytes());
    std::fs::write(path, bytes).expect("patched test TIFF must be writable");
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("patched test TIFF must reopen")
        .set_len(u64::from(strip_offset) + u64::from(strip_bytes))
        .expect("sparse test TIFF must extend");
    let header = focusforge_core::io::TiffHeader::open(path).expect("sparse TIFF must parse");
    header
        .require_fast_path()
        .expect("sparse TIFF must remain on the fast path");
    assert_eq!(
        header.file_size,
        u64::from(strip_offset) + u64::from(strip_bytes)
    );
}

#[test]
fn flags_processing_help_lists_current_names_on_every_carrier() {
    for (command, flags) in [
        (
            "stack",
            &[
                "--algorithm",
                "--align-algorithm",
                "--strict",
                "--no-progress",
            ][..],
        ),
        ("align", &["--algorithm", "--strict", "--no-progress"][..]),
    ] {
        let output = common::cli_bin("cli_contract")
            .args([command, "--help"])
            .output()
            .expect("processing help must run");
        assert!(output.status.success());
        let help = String::from_utf8_lossy(&output.stdout);
        for flag in flags {
            assert!(help.contains(flag), "{command} missing {flag}: {help}");
        }
        assert_eq!(help.contains("--cpu"), cfg!(feature = "gpu"));
    }
}

#[test]
fn flags_old_processing_names_are_rejected() {
    for command in ["stack", "align"] {
        for old_flag in ["--algo", "--align-algo", "--lax", "--no-gpu"] {
            let output = common::cli_bin("cli_contract")
                .args([command, "missing", old_flag])
                .output()
                .expect("CLI must run");
            assert_eq!(
                output.status.code(),
                Some(2),
                "{command} {old_flag} must be a clap error"
            );
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("unexpected argument"),
                "{command} {old_flag}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[test]
fn flags_cpu_is_feature_gated() {
    let output = common::cli_bin("cli_contract")
        .args(["stack", "--help"])
        .output()
        .expect("stack help must run");
    let help = String::from_utf8_lossy(&output.stdout);
    assert_eq!(help.contains("--cpu"), cfg!(feature = "gpu"));
    if !cfg!(feature = "gpu") {
        let output = common::cli_bin("cli_contract")
            .args(["align", "missing", "--cpu"])
            .output()
            .expect("no-default CLI must run");
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument"));
    }
}

#[test]
fn flags_new_processing_names_parse_on_every_carrier() {
    for mut args in [
        vec![
            "stack",
            "missing",
            "--algorithm",
            "dummy-stack",
            "--align-algorithm",
            "dummy-align",
            "--strict",
            "--no-progress",
        ],
        vec![
            "align",
            "missing",
            "--algorithm",
            "dummy-align",
            "--strict",
            "--no-progress",
        ],
    ] {
        if cfg!(feature = "gpu") {
            args.push("--cpu");
        }
        let output = common::cli_bin("cli_contract")
            .args(&args)
            .output()
            .expect("CLI must run");
        assert_ne!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("unexpected argument"),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn quiet_is_global_before_or_after_subcommand() {
    for command in [
        &["info"][..],
        &["algorithms"][..],
        &["stack"][..],
        &["export"][..],
        &["retouch"][..],
        &["history", "list"][..],
        &["history", "delete"][..],
        &["history", "compact"][..],
        &["align"][..],
        &["review"][..],
        &["cache", "clear"][..],
        &["diagnostics"][..],
    ] {
        for quiet in ["-q", "--quiet"] {
            let mut before = vec![quiet];
            before.extend_from_slice(command);
            before.push("--help");
            let mut after = command.to_vec();
            after.extend([quiet, "--help"]);
            for args in [before, after] {
                let output = common::cli_bin("cli_contract")
                    .args(&args)
                    .output()
                    .expect("CLI must run");
                assert!(output.status.success(), "quiet must parse: {args:?}");
            }
        }
    }
}

#[test]
fn root_help_groups_product_commands_from_metadata() {
    for args in [
        ["--help"].as_slice(),
        ["-h"].as_slice(),
        ["help"].as_slice(),
    ] {
        let output = common::cli_bin("cli_contract")
            .args(args)
            .output()
            .expect("CLI must run");
        assert!(output.status.success(), "{args:?}");
        let help = String::from_utf8_lossy(&output.stdout);
        for heading in ["Processing:", "Inspection:", "Maintenance:"] {
            assert!(help.contains(heading), "{heading} missing from {help}");
        }
        for example in [
            "Examples:",
            "  focusforge-cli stack frames/ --algorithm laplacian",
            "  focusforge-cli history list project.ffp",
        ] {
            assert!(help.contains(example), "{example} missing from {help}");
        }
        for command in [
            "stack",
            "align",
            "retouch",
            "export",
            "info",
            "review",
            "algorithms",
            "history",
            "cache",
            "diagnostics",
        ] {
            let listed = help
                .lines()
                .filter(|line| {
                    line.trim_start()
                        .strip_prefix(command)
                        .is_some_and(|rest| rest.starts_with(' '))
                })
                .count();
            assert_eq!(listed, 1, "{command}: {help}");
        }
    }

    for args in [
        ["help", "history", "list"].as_slice(),
        ["cache", "clear", "--help"].as_slice(),
        ["--version"].as_slice(),
        ["-V"].as_slice(),
    ] {
        let output = common::cli_bin("cli_contract")
            .args(args)
            .output()
            .expect("CLI must run");
        assert!(output.status.success(), "{args:?}");
    }
}

#[test]
fn quiet_and_no_progress_are_independent_for_json_stacks() {
    let cwd = common::tmp_dir("cli_contract_stack_quiet");
    let frames = common::fixtures_dir().join("mantis-64");
    for quiet in [false, true] {
        for no_progress in [false, true] {
            let output_dir = cwd.join(format!("{quiet}-{no_progress}"));
            std::fs::create_dir(&output_dir).unwrap();
            let mut args = vec![
                "stack".to_string(),
                frames.display().to_string(),
                "-o".to_string(),
                output_dir.display().to_string(),
                "--algorithm".to_string(),
                "dummy-stack".to_string(),
                "--no-align".to_string(),
                "--no-crop".to_string(),
                "--json".to_string(),
            ];
            if cfg!(feature = "gpu") {
                args.push("--cpu".to_string());
            }
            if quiet {
                args.push("--quiet".to_string());
            }
            if no_progress {
                args.push("--no-progress".to_string());
            }
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let output = common::run_in(&mut common::cli_bin("cli_contract"), &cwd, &refs);
            assert!(output.success, "{args:?}: {}", output.stderr);
            serde_json::from_str::<serde_json::Value>(&output.stdout)
                .unwrap_or_else(|error| panic!("{args:?} must produce one JSON document: {error}"));
            assert_eq!(output.stderr.contains("grouping "), !quiet, "{args:?}");
            assert_eq!(output.stderr.contains("fuse "), !no_progress, "{args:?}");
        }
    }
}

#[test]
fn quiet_and_no_progress_are_independent_for_json_alignments() {
    for quiet in [false, true] {
        for no_progress in [false, true] {
            let frames = common::fixtures_dir().join("mantis-64");
            let mut args = vec![
                "align".to_string(),
                frames.display().to_string(),
                "--algorithm".to_string(),
                "pyramidal-ecc".to_string(),
                "--json".to_string(),
            ];
            if cfg!(feature = "gpu") {
                args.push("--cpu".to_string());
            }
            if quiet {
                args.push("-q".to_string());
            }
            if no_progress {
                args.push("--no-progress".to_string());
            }
            let output = common::cli_bin("cli_contract")
                .args(&args)
                .output()
                .expect("CLI must run");
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice::<serde_json::Value>(&output.stdout)
                .unwrap_or_else(|error| panic!("{args:?} must produce one JSON document: {error}"));
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("fell back to identity"),
                "{args:?} must retain the registration warning: {stderr}"
            );
            assert_eq!(
                stderr.contains("align 3 frames"),
                !no_progress,
                "{args:?}: {stderr}"
            );
        }
    }
}

#[test]
fn quiet_stack_retains_a_fallback_warning_without_routine_stderr() {
    let cwd = common::tmp_dir("cli_contract_stack_fallback_quiet");
    let output_dir = cwd.join("out");
    std::fs::create_dir(&output_dir).unwrap();
    let mut args = vec![
        "stack".to_string(),
        common::mantis_512_dir().display().to_string(),
        "-o".to_string(),
        output_dir.display().to_string(),
        "--algorithm".to_string(),
        "dummy-stack".to_string(),
        "--align-algorithm".to_string(),
        "pyramidal-ecc".to_string(),
        "--json".to_string(),
        "--quiet".to_string(),
        "--no-progress".to_string(),
    ];
    if cfg!(feature = "gpu") {
        args.push("--cpu".to_string());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = common::run_in(&mut common::cli_bin("cli_contract"), &cwd, &refs);
    assert!(output.success, "stderr: {}", output.stderr);
    let report: serde_json::Value = serde_json::from_str(&output.stdout).unwrap();
    assert!(report["align"]["failure_count"].as_u64().unwrap() > 0);
    assert!(output.stderr.contains("fell back to identity"));
    assert!(
        !output.stderr.contains("grouping "),
        "stderr: {}",
        output.stderr
    );
    assert!(
        !output.stderr.contains("fuse "),
        "stderr: {}",
        output.stderr
    );
}

/// How the operating system words a path that does not exist.
const MISSING_PATH: &str = if cfg!(windows) {
    "The system cannot find the path specified"
} else {
    "No such file or directory"
};

fn assert_fatal_parity(args: &[&str], needle: &str) {
    let default = common::cli_bin("cli_contract")
        .args(args)
        .output()
        .expect("default CLI must run");
    let mut strict_args = args.to_vec();
    strict_args.push("--strict");
    let strict = common::cli_bin("cli_contract")
        .args(&strict_args)
        .output()
        .expect("strict CLI must run");
    assert_eq!(default.status.code(), strict.status.code(), "{args:?}");
    assert!(!default.status.success(), "{args:?} must fail");
    for output in [&default, &strict] {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(needle), "{args:?}: {stderr}");
        assert!(
            !stderr.contains("fell back to identity"),
            "{args:?} must not turn a fatal error into fallback"
        );
    }
}

#[test]
fn fatal_errors_match_with_or_without_strict() {
    let frames = common::fixtures_dir().join("mantis-64");
    let frames = frames.to_str().expect("fixture path is UTF-8");
    for (args, needle) in [
        (
            vec![
                "align",
                "/definitely/does/not/exist/cli-contract",
                "--algorithm",
                "pyramidal-ecc",
            ],
            MISSING_PATH,
        ),
        (
            vec!["align", frames, "--algorithm", "no-such-algorithm"],
            "unknown algorithm",
        ),
        (
            vec![
                "align",
                frames,
                "--algorithm",
                "pyramidal-ecc",
                "--max-host-memory",
                "not-a-size",
            ],
            "does not start with a decimal integer",
        ),
    ] {
        assert_fatal_parity(&args, needle);
    }

    let invalid_dir = common::tmp_dir("cli_contract_invalid_frame");
    std::fs::write(invalid_dir.join("00.tif"), b"not a TIFF").unwrap();
    let invalid_path = invalid_dir.to_str().expect("temporary path is UTF-8");
    assert_fatal_parity(
        &["align", invalid_path, "--algorithm", "pyramidal-ecc"],
        "not a TIFF",
    );

    #[cfg(unix)]
    {
        use focusforge_core::io::{TiffSpec, TiffWriter};
        use std::os::unix::fs::PermissionsExt;

        let unreadable_dir = common::tmp_dir("cli_contract_unreadable_frame");
        let unreadable = unreadable_dir.join("00.tif");
        let mut writer = TiffWriter::create(
            &unreadable,
            TiffSpec {
                width: 1,
                height: 1,
                samples_per_pixel: 3,
                description: None,
            },
        )
        .unwrap();
        writer
            .write_tile(common::full_rect(1, 1), &[0, 0, 0])
            .unwrap();
        writer.finish().unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(
            std::fs::File::open(&unreadable).is_err(),
            "test file must be unreadable"
        );
        assert_fatal_parity(
            &[
                "align",
                unreadable_dir.to_str().expect("temporary path is UTF-8"),
                "--algorithm",
                "pyramidal-ecc",
            ],
            "Permission denied",
        );
    }

    let stack_out = common::tmp_dir("cli_contract_stack_fatal_out");
    let stack_out_path = stack_out.to_str().expect("temporary path is UTF-8");
    assert_fatal_parity(
        &[
            "stack",
            "/definitely/does/not/exist/cli-contract-stack",
            "-o",
            stack_out_path,
            "--algorithm",
            "dummy-stack",
            "--no-align",
        ],
        MISSING_PATH,
    );
    assert_fatal_parity(
        &[
            "stack",
            frames,
            "-o",
            stack_out_path,
            "--algorithm",
            "no-such-algorithm",
        ],
        "unknown algorithm",
    );
}

#[test]
fn fatal_errors_memory_ceiling_matches_with_or_without_strict() {
    let frames_dir = common::tmp_dir("cli_contract_memory_ceiling");
    write_sparse_memory_witness(&frames_dir.join("00.tif"));
    write_sparse_memory_witness(&frames_dir.join("01.tif"));
    let frames = frames_dir.to_str().expect("temporary path is UTF-8");
    let output = std::path::Path::new(frames).join("out");
    std::fs::create_dir(&output).expect("test output directory must be creatable");
    let output = output.to_str().expect("temporary path is UTF-8");
    for (mut args, writes_project) in [
        (vec!["align", frames, "--algorithm", "dummy-align"], false),
        (
            vec![
                "stack",
                frames,
                "-o",
                output,
                "--algorithm",
                "dummy-stack",
                "--align-algorithm",
                "dummy-align",
            ],
            true,
        ),
    ] {
        let default = common::cli_bin("cli_contract")
            .args(&args)
            .output()
            .expect("default CLI must run");
        args.push("--strict");
        let strict = common::cli_bin("cli_contract")
            .args(&args)
            .output()
            .expect("strict CLI must run");
        assert_eq!(default.status.code(), Some(1), "{args:?}");
        assert_eq!(strict.status.code(), Some(1), "{args:?}");
        for output in [&default, &strict] {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("host-derived memory budget"),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stdout.is_empty(), "{args:?} must not report JSON");
            assert!(
                !String::from_utf8_lossy(&output.stderr).contains("fell back to identity"),
                "{args:?} must not turn the memory refusal into a fallback"
            );
        }
        if writes_project {
            assert!(
                std::fs::read_dir(output)
                    .expect("test output directory must remain readable")
                    .all(|entry| entry
                        .expect("output entry must be readable")
                        .path()
                        .extension()
                        != Some(std::ffi::OsStr::new("ffp"))),
                "{args:?} must write no project"
            );
        }
    }
}

#[test]
fn quiet_still_parses_on_a_failing_command() {
    for args in [["-q", "info", "missing.tif"], ["info", "missing.tif", "-q"]] {
        let output = common::cli_bin("cli_contract")
            .args(args)
            .output()
            .expect("CLI must run");
        assert_eq!(output.status.code(), Some(1), "quiet must parse: {args:?}");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("error: unexpected argument"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn no_progress_is_local_to_processing_commands() {
    for command in ["stack", "align"] {
        let output = common::cli_bin("cli_contract")
            .args([command, "--help"])
            .output()
            .expect("help must run");
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("--no-progress"),
            "{command} must expose --no-progress"
        );
    }
}
