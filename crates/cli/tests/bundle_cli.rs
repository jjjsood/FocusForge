// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli diagnostics`, seen from outside the process: bundle
//! contents match the printed list, the minidump opt-in, redaction, the
//! allowlist, effective configuration, the version entry and boundary cases.
//!
//! Every case seeds a temporary data directory through `common::cli_bin`,
//! whose `FOCUSFORGE_DATA_DIR` override keeps the run off the tester's own.

mod common;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The data directory `cli_bin` pointed the child at, created empty.
fn data_dir(cmd: &Command) -> PathBuf {
    let dir = common::data_dir_of(cmd).join("focusforge");
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    std::fs::create_dir_all(dir.join("crashes")).unwrap();
    dir
}

/// A live log, a rotated log, a panic report, and a native crash's metadata
/// beside its dump.
fn seed_evidence(dir: &Path) {
    std::fs::write(
        dir.join("logs/focusforge.jsonl"),
        "{\"timestamp\":\"2026-09-18T01:02:03Z\",\"level\":\"INFO\",\"target\":\"focusforge_cli\",\"message\":\"live\"}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("logs/focusforge.1.jsonl"),
        "{\"timestamp\":\"2026-09-18T01:02:02Z\",\"level\":\"INFO\",\"target\":\"focusforge_cli\",\"message\":\"rotated\"}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("crashes/20260918T010203Z-focusforge-cli-7.json"),
        "{\"format\":1,\"payload\":\"boom\",\"backtrace\":\"at main\"}",
    )
    .unwrap();
    std::fs::write(
        dir.join("crashes/20260918T010204Z-focusforge-cli-8.json"),
        "{\"format\":1,\"signal\":11,\"minidump\":\"20260918T010204Z-focusforge-cli-8.dmp\"}",
    )
    .unwrap();
    std::fs::write(
        dir.join("crashes/20260918T010204Z-focusforge-cli-8.dmp"),
        b"MDMP\0\0\0\0",
    )
    .unwrap();
}

fn zip_names(path: &Path) -> Vec<String> {
    let file =
        std::fs::File::open(path).unwrap_or_else(|e| panic!("{} must exist: {e}", path.display()));
    let mut archive = zip::ZipArchive::new(file).unwrap();
    (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().unwrap().to_string())
        .collect()
}

fn zip_text(path: &Path, name: &str) -> String {
    let file = std::fs::File::open(path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut entry = archive
        .by_name(name)
        .unwrap_or_else(|e| panic!("{name} must be in the archive: {e}"));
    let mut text = String::new();
    entry.read_to_string(&mut text).unwrap();
    text
}

fn zip_json(path: &Path, name: &str) -> serde_json::Value {
    serde_json::from_str(&zip_text(path, name))
        .unwrap_or_else(|e| panic!("{name} must be JSON: {e}"))
}

/// The entry names the command printed, in the order it printed them:
/// every indented line that is not an exclusion line.
fn printed_entries(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|line| line.strip_prefix("  "))
        .filter(|line| !line.starts_with("excluded:"))
        .filter_map(|line| line.split_whitespace().next().map(str::to_string))
        .collect()
}

#[test]
fn bundle_contents_match_printed_list() {
    let mut cmd = common::cli_bin("bundle_cli_contents");
    let dir = data_dir(&cmd);
    seed_evidence(&dir);
    let out = dir.join("bundle.zip");

    let output = cmd
        .args(["diagnostics", "-o", out.to_str().unwrap()])
        .output()
        .expect("failed to run the binary");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();

    let names = zip_names(&out);
    assert_eq!(printed_entries(&stdout), names, "stdout was:\n{stdout}");
    for expected in [
        "system.json",
        "adapter.json",
        "configuration.json",
        "version.json",
        "logs/focusforge.jsonl",
        "logs/focusforge.1.jsonl",
        "crashes/20260918T010203Z-focusforge-cli-7.json",
        "crashes/20260918T010204Z-focusforge-cli-8.json",
    ] {
        assert!(names.contains(&expected.to_string()), "{names:?}");
    }
    assert!(
        !names.iter().any(|n| n.ends_with(".dmp")),
        "no minidump by default: {names:?}"
    );
}

#[test]
fn include_minidumps_warns_and_marks() {
    let mut cmd = common::cli_bin("bundle_cli_minidumps");
    let dir = data_dir(&cmd);
    seed_evidence(&dir);
    let out = dir.join("bundle.zip");

    let output = cmd
        .args([
            "diagnostics",
            "--include-minidumps",
            "-o",
            out.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run the binary");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();

    let dump = "crashes/20260918T010204Z-focusforge-cli-8.dmp";
    assert!(zip_names(&out).contains(&dump.to_string()));
    assert_eq!(printed_entries(&stdout), zip_names(&out));
    let marked = stdout
        .lines()
        .find(|line| line.contains(dump))
        .unwrap_or_else(|| panic!("the dump must be listed:\n{stdout}"));
    assert!(marked.contains("not redacted"), "{marked}");
    assert!(
        stdout.to_lowercase().contains("warning"),
        "the opt-in warns:\n{stdout}"
    );
}

#[test]
fn redacts_home_and_user() {
    let mut cmd = common::cli_bin("bundle_cli_redaction");
    let dir = data_dir(&cmd);
    seed_evidence(&dir);
    let home = dir.join("home").join("frobnicator");
    std::fs::create_dir_all(&home).unwrap();
    let home_text = home.to_str().unwrap().to_string();
    std::fs::write(
        dir.join("logs/focusforge.jsonl"),
        format!("{{\"message\":\"read {home_text}/frames/IMG_1.tif for frobnicator\"}}\n"),
    )
    .unwrap();
    std::fs::write(
        dir.join("crashes/20260918T010203Z-focusforge-cli-7.json"),
        format!("{{\"format\":1,\"backtrace\":\"at {home_text}/src/main.rs\"}}"),
    )
    .unwrap();
    std::fs::write(
        dir.join("crashes/20260918T010204Z-focusforge-cli-8.json"),
        format!(
            "{{\"format\":1,\"minidump\":\"d.dmp\",\"binary\":\"{home_text}/bin/focusforge-cli\"}}"
        ),
    )
    .unwrap();
    let out = dir.join("bundle.zip");

    let output = cmd
        .env("HOME", &home)
        .env("USER", "frobnicator")
        .args(["diagnostics", "-o", out.to_str().unwrap()])
        .output()
        .expect("failed to run the binary");
    assert!(output.status.success(), "{output:?}");

    for name in zip_names(&out) {
        let text = zip_text(&out, &name);
        assert!(!text.contains(&home_text), "{name} holds the home path");
        assert!(!text.contains("frobnicator"), "{name} holds the user name");
    }
    assert!(
        zip_json(&out, "system.json").get("host_name").is_none(),
        "the host name is never collected"
    );
}

#[test]
fn allowlist_excludes_everything_else() {
    let mut cmd = common::cli_bin("bundle_cli_allowlist");
    let dir = data_dir(&cmd);
    seed_evidence(&dir);
    std::fs::write(dir.join("logs/preview.tif"), b"II*\0").unwrap();
    std::fs::write(dir.join("logs/notes.txt"), "stray").unwrap();
    std::fs::write(dir.join("crashes/frame.tif"), b"II*\0").unwrap();
    std::fs::write(dir.join("session.ffproj"), b"PK\x03\x04").unwrap();
    std::fs::write(dir.join("stray.txt"), "stray").unwrap();
    let out = dir.join("bundle.zip");

    let output = cmd
        .args(["diagnostics", "-o", out.to_str().unwrap()])
        .output()
        .expect("failed to run the binary");
    assert!(output.status.success(), "{output:?}");

    let names = zip_names(&out);
    for absent in [
        "logs/preview.tif",
        "logs/notes.txt",
        "crashes/frame.tif",
        "session.ffproj",
        "stray.txt",
    ] {
        assert!(!names.contains(&absent.to_string()), "{names:?}");
    }
    assert!(!names.iter().any(|n| n.ends_with(".tif")), "{names:?}");
    assert!(!names.iter().any(|n| n.ends_with(".ffproj")), "{names:?}");
}

#[test]
fn config_has_provenance() {
    let mut cmd = common::cli_bin("bundle_cli_config");
    let dir = data_dir(&cmd);
    let config_root = dir.join("config");
    std::fs::create_dir_all(config_root.join("focusforge")).unwrap();
    std::fs::write(
        config_root.join("focusforge").join("settings.toml"),
        "# input map\n",
    )
    .unwrap();
    let out = dir.join("bundle.zip");

    let output = cmd
        .env("FOCUSFORGE_CONFIG_DIR", &config_root)
        .args(["diagnostics", "-o", out.to_str().unwrap()])
        .output()
        .expect("failed to run the binary");
    assert!(output.status.success(), "{output:?}");

    let config = zip_json(&out, "configuration.json");
    let algorithms = config["algorithms"].as_array().unwrap();
    let dummy = algorithms
        .iter()
        .find(|a| a["id"] == "dummy-stack")
        .unwrap_or_else(|| panic!("dummy-stack must be registered: {config}"));
    let param = |id: &str| {
        dummy["params"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .unwrap_or_else(|| panic!("{id} must be listed: {dummy}"))
            .clone()
    };
    assert_eq!(param("workers")["provenance"], "Host");
    assert!(param("workers")["value"].as_i64().unwrap() >= 1);
    assert_eq!(param("mode")["provenance"], "Default");

    assert_eq!(config["input_map"]["exists"], true);
    assert!(
        config["input_map"]["path"]
            .as_str()
            .unwrap()
            .ends_with("settings.toml"),
        "{config}"
    );
}

#[test]
fn version_entry() {
    let mut cmd = common::cli_bin("bundle_cli_version");
    let dir = data_dir(&cmd);
    let out = dir.join("bundle.zip");

    let output = cmd
        .args(["diagnostics", "-o", out.to_str().unwrap()])
        .output()
        .expect("failed to run the binary");
    assert!(output.status.success(), "{output:?}");

    let version = zip_json(&out, "version.json");
    assert_eq!(version["version"], env!("CARGO_PKG_VERSION"));
    let commit = version["commit"].as_str().unwrap();
    assert!(!commit.is_empty(), "{version}");
}

#[test]
fn empty_data_dir_and_existing_output() {
    let mut cmd = common::cli_bin("bundle_cli_boundary");
    let work = common::tmp_dir("bundle_cli_boundary_work");
    let dir = data_dir(&cmd);
    let out = dir.join("bundle.zip");

    let output = cmd
        .args(["diagnostics", "-o", out.to_str().unwrap()])
        .output()
        .expect("failed to run the binary");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    // The run's own subscriber writes a startup line, so the live log is
    // the one collected file an otherwise empty data directory yields.
    assert_eq!(
        printed_entries(&stdout),
        vec![
            "adapter.json",
            "configuration.json",
            "system.json",
            "version.json",
            "logs/focusforge.jsonl",
        ]
    );
    assert_eq!(printed_entries(&stdout), zip_names(&out));
    assert!(
        stdout.contains("no crash reports were found"),
        "stdout was:\n{stdout}"
    );

    // An existing output is refused, and its bytes survive.
    let taken = work.join("taken.zip");
    std::fs::write(&taken, "not a bundle").unwrap();
    let mut refuse = common::cli_bin("bundle_cli_boundary_refuse");
    data_dir(&refuse);
    let refused = refuse
        .args(["diagnostics", "-o", taken.to_str().unwrap()])
        .output()
        .expect("failed to run the binary");
    assert!(!refused.status.success(), "{refused:?}");
    assert_eq!(std::fs::read_to_string(&taken).unwrap(), "not a bundle");

    let mut forced = common::cli_bin("bundle_cli_boundary_force");
    data_dir(&forced);
    let overwritten = forced
        .args(["diagnostics", "--force", "-o", taken.to_str().unwrap()])
        .output()
        .expect("failed to run the binary");
    assert!(overwritten.status.success(), "{overwritten:?}");
    assert!(zip_names(&taken).contains(&"version.json".to_string()));

    // With no -o, a dated name lands in the working directory.
    let mut defaulted = common::cli_bin("bundle_cli_boundary_default");
    data_dir(&defaulted);
    let wrote = defaulted
        .arg("diagnostics")
        .current_dir(&*work)
        .output()
        .expect("failed to run the binary");
    assert!(wrote.status.success(), "{wrote:?}");
    let written: Vec<String> = std::fs::read_dir(&*work)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("focusforge-diagnostics-") && name.ends_with(".zip"))
        .collect();
    assert_eq!(written.len(), 1, "{written:?}");
    assert!(!written[0].contains(':'), "{written:?}");
}

#[test]
fn system_and_adapter_entries() {
    let mut cmd = common::cli_bin("bundle_cli_system");
    let dir = data_dir(&cmd);
    seed_evidence(&dir);
    let out = dir.join("bundle.zip");

    let output = cmd
        .args(["diagnostics", "-o", out.to_str().unwrap()])
        .output()
        .expect("failed to run the binary");
    assert!(output.status.success(), "{output:?}");

    let system = zip_json(&out, "system.json");
    assert!(system["total_memory"].as_u64().unwrap() > 0, "{system}");
    assert!(system["available_memory"].as_u64().is_some(), "{system}");
    assert!(system["usable_memory"].as_u64().unwrap() > 0, "{system}");
    assert!(system["logical_cores"].as_u64().unwrap() >= 1, "{system}");
    assert_eq!(system["facts_origin"], "Probed");
    assert!(
        system["os_name"].is_string() || system["kernel_version"].is_string(),
        "{system}"
    );

    let adapter = zip_json(&out, "adapter.json");
    assert!(adapter["selected"].is_null(), "{adapter}");
    assert!(adapter["note"].is_string(), "{adapter}");

    // The same logs, holding the record a graphics run writes.
    let mut with_record = common::cli_bin("bundle_cli_system_adapter");
    let dir = data_dir(&with_record);
    std::fs::write(
        dir.join("logs/focusforge.jsonl"),
        "{\"level\":\"DEBUG\",\"target\":\"focusforge_gpu\",\"message\":\"graphics adapter selected\",\"adapter\":\"Test Adapter\",\"backend\":\"Vulkan\"}\n\
         {\"level\":\"DEBUG\",\"target\":\"focusforge_gpu\",\"message\":\"graphics adapter and device limits\",\"device_limits\":\"...\"}\n",
    )
    .unwrap();
    let out = dir.join("bundle.zip");
    let output = with_record
        .args(["diagnostics", "-o", out.to_str().unwrap()])
        .output()
        .expect("failed to run the binary");
    assert!(output.status.success(), "{output:?}");

    let adapter = zip_json(&out, "adapter.json");
    assert_eq!(adapter["selected"]["adapter"], "Test Adapter");
    assert_eq!(adapter["selected"]["backend"], "Vulkan");
    assert!(adapter["limits"].is_object(), "{adapter}");
}
