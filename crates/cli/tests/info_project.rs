// SPDX-License-Identifier: GPL-3.0-or-later

mod common;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use focusforge_core::align::Registration;
use focusforge_core::params::ParamValues;
use focusforge_core::project::container::{ContainerWriter, Index};
use focusforge_core::project::{self, Metrics, Project, SealRequest};
use focusforge_core::tile::Rect;
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};
use serde_json::Value;

fn run(dir: &Path, args: &[&str]) -> common::CliOutput {
    common::run_in(&mut common::cli_bin("info_project"), dir, args)
}

fn arg(path: &Path) -> &str {
    path.to_str().expect("test paths are UTF-8")
}

fn only_project(dir: &Path) -> PathBuf {
    let mut projects: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "ffp"))
        .collect();
    assert_eq!(projects.len(), 1, "one project expected in {dir:?}");
    projects.pop().unwrap()
}

struct SharedStack {
    _dir: common::TmpDir,
    project: PathBuf,
}

fn shared_stack() -> &'static SharedStack {
    static STACK: OnceLock<SharedStack> = OnceLock::new();
    STACK.get_or_init(|| {
        let dir = common::tmp_dir("info_project_stack");
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let output = run(
            &dir,
            &[
                "stack",
                arg(&common::mantis_512_dir()),
                "-o",
                arg(&out),
                "--algorithm",
                "dummy-stack",
                "--no-align",
                "--no-crop",
            ],
        );
        assert!(output.success, "stack: {}", output.stderr);
        SharedStack {
            project: only_project(&out),
            _dir: dir,
        }
    })
}

fn fresh_project(name: &str) -> (common::TmpDir, PathBuf) {
    let dir = common::tmp_dir(name);
    let project = dir.join("project.ffp");
    std::fs::copy(&shared_stack().project, &project).unwrap();
    let isolated = dir.join("isolated.ffp");
    isolate_sources(&project, &isolated, &dir);
    (dir, isolated)
}

fn retouch(dir: &Path, project: &Path, name: &str) -> String {
    let ops = dir.join(format!("{name}.json"));
    let strokes: Vec<Value> = serde_json::from_slice(
        &std::fs::read(common::fixtures_dir().join("retouch/stroke-list.json")).unwrap(),
    )
    .unwrap();
    std::fs::write(&ops, Value::Array(vec![strokes[0].clone()]).to_string()).unwrap();
    let output = run(
        dir,
        &[
            "retouch",
            arg(project),
            "--ops",
            arg(&ops),
            "--name",
            name,
            "--json",
        ],
    );
    assert!(output.success, "retouch: {}", output.stderr);
    serde_json::from_str::<Value>(&output.stdout).unwrap()["checkpoints"][0]
        .as_str()
        .unwrap()
        .to_string()
}

fn info(dir: &Path, project: &Path, json: bool) -> common::CliOutput {
    if json {
        run(dir, &["info", "--json", arg(project)])
    } else {
        run(dir, &["info", arg(project)])
    }
}

fn assert_project_report(dir: &Path, project_path: &Path) {
    let project = Project::open(project_path).unwrap();
    let checkpoint = project.checkpoint();
    let fused = project.fuse_state(&checkpoint.id).unwrap();
    let json = info(dir, project_path, true);
    assert!(json.success, "json: {}", json.stderr);
    let report: Value = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!(report["path"], project_path.display().to_string());
    assert_eq!(
        report["bytes"],
        std::fs::metadata(project_path).unwrap().len()
    );
    assert_eq!(report["producer"], project.manifest().producer);
    assert_eq!(report["created_at"], project.manifest().created_at);
    assert_eq!(report["checkpoint_id"], checkpoint.id);
    assert_eq!(
        report["checkpoint_label"],
        serde_json::to_value(&checkpoint.label).unwrap()
    );
    assert_eq!(report["algorithm"], checkpoint.algorithm);
    assert_eq!(
        report["params"],
        serde_json::to_value(&checkpoint.params).unwrap()
    );
    assert_eq!(report["width"], fused.composite.width);
    assert_eq!(report["height"], fused.composite.height);
    assert_eq!(report["channels"], fused.composite.samples_per_pixel);
    assert_eq!(report["source_count"], project.manifest().sources.len());
    assert_eq!(
        report["visible_checkpoint_count"],
        project.history().visible().len()
    );

    let text = info(dir, project_path, false);
    assert!(text.success, "text: {}", text.stderr);
    let params = serde_json::to_string(&checkpoint.params).unwrap();
    let values = [
        project_path.display().to_string(),
        format!(
            "  bytes: {}",
            std::fs::metadata(project_path).unwrap().len()
        ),
        format!("  producer: {}", project.manifest().producer),
        format!("  created_at: {}", project.manifest().created_at),
        format!("  checkpoint: {}", checkpoint.id),
        format!(
            "  checkpoint label: {}",
            checkpoint.label.as_deref().unwrap_or("none")
        ),
        format!("  algorithm: {}", checkpoint.algorithm),
        format!("  params: {params}"),
        format!("  width: {}", fused.composite.width),
        format!("  height: {}", fused.composite.height),
        format!("  channels: {}", fused.composite.samples_per_pixel),
        format!("  sources: {}", project.manifest().sources.len()),
        format!(
            "  visible checkpoints: {}",
            project.history().visible().len()
        ),
    ];
    for value in values {
        assert!(
            text.stdout.contains(&value),
            "missing {value}: {}",
            text.stdout
        );
    }
}

fn mutate_container(src: &Path, dst: &Path, replace: &[(String, Vec<u8>)]) {
    let index = Index::read(src).unwrap();
    let mut writer = ContainerWriter::create(dst).unwrap();
    for name in index.names() {
        let bytes = replace
            .iter()
            .find(|(replacement, _)| replacement == name)
            .map(|(_, bytes)| bytes.clone())
            .unwrap_or_else(|| index.read_entry(src, name).unwrap());
        writer.put_bytes(name, &bytes).unwrap();
    }
    writer.finish().unwrap();
}

fn isolate_sources(project: &Path, isolated: &Path, dir: &Path) {
    let index = Index::read(project).unwrap();
    let mut manifest: Value =
        serde_json::from_slice(&index.read_entry(project, "manifest.json").unwrap()).unwrap();
    let root = manifest["checkpoint"].as_str().unwrap().to_string();
    let root_entry = format!("checkpoints/{root}.json");
    let mut checkpoint: Value =
        serde_json::from_slice(&index.read_entry(project, &root_entry).unwrap()).unwrap();
    let local_sources = dir.join("sources");
    std::fs::create_dir_all(&local_sources).unwrap();
    for (index, source) in manifest["sources"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        let original = PathBuf::from(source["path"].as_str().unwrap());
        let local = local_sources.join(format!("source-{index}.tif"));
        std::fs::copy(&original, &local).unwrap();
        source["path"] = Value::String(local.display().to_string());
        checkpoint["fuse"]["sources"][index]["path"] = Value::String(local.display().to_string());
    }
    mutate_container(
        project,
        isolated,
        &[
            (
                "manifest.json".to_string(),
                serde_json::to_vec(&manifest).unwrap(),
            ),
            (root_entry, serde_json::to_vec(&checkpoint).unwrap()),
        ],
    );
}

fn different_same_shape_zmap(dir: &Path, width: u32, height: u32, channels: u32) -> Vec<u8> {
    let replacement_dir = dir.join("replacement");
    std::fs::create_dir_all(&replacement_dir).unwrap();
    let source = common::mantis_512_dir().join("frames/00.tif");
    let source_one = replacement_dir.join("source-one.tif");
    let source_two = replacement_dir.join("source-two.tif");
    std::fs::copy(&source, &source_one).unwrap();
    std::fs::copy(&source, &source_two).unwrap();

    let zmap = replacement_dir.join("zmap.tif");
    let mut writer = ZMapWriter::create(&zmap, ZMapInfo::new(width, height, 2).unwrap()).unwrap();
    writer
        .write_tile(
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            &vec![1; (width * height) as usize],
        )
        .unwrap();
    writer.finish().unwrap();

    let replacement = replacement_dir.join("replacement.ffp");
    project::seal(SealRequest {
        project_path: &replacement,
        staged_composite: &source_one,
        staged_zmap: &zmap,
        sources: &[source_one.clone(), source_two],
        algorithm: "dummy-stack",
        params: &ParamValues::new(),
        align: None,
        registration: Registration::default(),
        composite_description: "replacement composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .unwrap();

    let replacement = Project::open(&replacement).unwrap();
    let fused = replacement
        .fuse_state(&replacement.checkpoint().id)
        .unwrap();
    assert_eq!(fused.composite.width, width);
    assert_eq!(fused.composite.height, height);
    assert_eq!(fused.composite.samples_per_pixel, channels);
    let hash = replacement.checkpoint().zmap_blob.clone();
    Index::read(replacement.path())
        .unwrap()
        .read_entry(replacement.path(), &format!("blobs/{hash}.zst"))
        .unwrap()
}

#[test]
fn info_project_reports_fresh_retouch_and_hidden_history_facts() {
    let (dir, project) = fresh_project("info_project_reports_facts");
    assert_project_report(&dir, &project);

    let first = retouch(&dir, &project, "first label");
    let _second = retouch(&dir, &project, "second label");
    let deleted = run(
        &dir,
        &[
            "history",
            "delete",
            arg(&project),
            "--checkpoint",
            &first,
            "--json",
        ],
    );
    assert!(deleted.success, "delete: {}", deleted.stderr);
    let open = Project::open(&project).unwrap();
    assert!(open.history().all().len() > open.history().visible().len());
    drop(open);
    assert_project_report(&dir, &project);

    let uppercase = dir.join("PROJECT.FFP");
    std::fs::copy(&project, &uppercase).unwrap();
    assert_project_report(&dir, &uppercase);
}

#[test]
fn info_project_leaves_file_unchanged() {
    let (dir, project) = fresh_project("info_project_source_independence");
    let _first = retouch(&dir, &project, "first label");
    let _second = retouch(&dir, &project, "second label");
    assert!(
        Project::open(&project).unwrap().history().visible().len() >= 2,
        "source independence covers a multi-checkpoint project"
    );
    let before = std::fs::read(&project).unwrap();
    let source = PathBuf::from(&Project::open(&project).unwrap().manifest().sources[0].path);
    std::fs::write(&source, b"changed source").unwrap();
    assert_project_report(&dir, &project);
    let moved = source.with_extension("moved");
    std::fs::rename(&source, &moved).unwrap();
    assert_project_report(&dir, &project);
    std::fs::remove_file(&moved).unwrap();
    assert_project_report(&dir, &project);
    assert_eq!(std::fs::read(&project).unwrap(), before);
}

#[test]
fn info_project_keeps_ffp_directories_on_the_directory_route() {
    let dir = common::tmp_dir("info_project_directory_route");
    let project_named_dir = dir.join("frames.ffp");
    std::fs::create_dir_all(&project_named_dir).unwrap();
    std::fs::copy(
        common::mantis_512_dir().join("frames/00.tif"),
        project_named_dir.join("00.tif"),
    )
    .unwrap();
    let output = info(&dir, &project_named_dir, true);
    assert!(output.success, "directory info: {}", output.stderr);
    let report: Value = serde_json::from_str(&output.stdout).unwrap();
    assert_eq!(report["is_directory"], true);
    assert_eq!(report["frame_count"], 1);
}

#[test]
fn info_project_refuses_invalid_projects_without_stdout() {
    let (dir, project) = fresh_project("info_project_refuses_invalid");
    let foreign = dir.join("foreign.ffp");
    std::fs::write(&foreign, b"not a project").unwrap();
    let bytes = std::fs::read(&project).unwrap();
    let truncated = dir.join("truncated.ffp");
    std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();

    let open = Project::open(&project).unwrap();
    let head = open.checkpoint().zmap_blob.clone();
    let fused = open.fuse_state(&open.checkpoint().id).unwrap();
    drop(open);
    let bad_hash = dir.join("bad-hash.ffp");
    let index = Index::read(&project).unwrap();
    let replacement_bytes = different_same_shape_zmap(
        &dir,
        fused.composite.width,
        fused.composite.height,
        fused.composite.samples_per_pixel,
    );
    let head_bytes = index
        .read_entry(&project, &format!("blobs/{head}.zst"))
        .unwrap();
    assert_ne!(
        replacement_bytes, head_bytes,
        "the replacement Z-Map must have different compressed bytes"
    );
    mutate_container(
        &project,
        &bad_hash,
        &[(format!("blobs/{head}.zst"), replacement_bytes)],
    );

    for (path, code, reason) in [
        (&foreign, 3, "is not a FocusForge project"),
        (&truncated, 4, "truncated"),
        (&bad_hash, 7, "failed its blake3 hash check"),
    ] {
        for json in [false, true] {
            let output = info(&dir, path, json);
            assert_eq!(output.code, Some(code), "{}", output.stderr);
            assert!(output.stdout.is_empty());
            assert!(output.stderr.contains(&path.display().to_string()));
            assert!(output.stderr.contains(reason), "{}", output.stderr);
        }
    }
}
