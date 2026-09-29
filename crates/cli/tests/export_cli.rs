// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli export`: a project in, an image out.
//!
//! One `laplacian` run over `fixtures/mantis-512` is shared through a
//! `OnceLock`; its export is compared against an in-process export and a
//! gather of the head's Z-Map over the stack's frames.
//!
//! Every spawn isolates `.scratch/` with its own per-process temp directory
//! and `.current_dir(dir)`: `stack`'s scratch tree is cwd-relative, so
//! concurrent test runs sharing one `target/` -- or two tests in the same
//! run under cargo's default parallel execution -- must never share a
//! working directory either. `export` writes no scratch state, so its spawns
//! run in the shared `stack` run's cwd.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter};
use focusforge_core::params::ParamValues;
use focusforge_core::project::{
    self, AlignRecord, AppendRequest, Metrics, Op, Point, Project, SealRequest, ZMapStorage,
};
use focusforge_core::retouch::{Sources, recomposite};
use focusforge_core::zmap::{ZMapInfo, ZMapReader, ZMapWriter};

fn bin() -> Command {
    common::cli_bin("export_cli")
}

struct CliOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

fn run_cli_in(dir: &Path, args: &[&str]) -> CliOutput {
    let output = bin()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run binary");
    CliOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn only_project(dir: &Path) -> PathBuf {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "ffp"))
        .collect();
    assert_eq!(found.len(), 1, "exactly one project expected in {dir:?}");
    found.pop().unwrap()
}

/// One `stack` run over `fixtures/mantis-512`, collected into a `-o`
/// directory. ~50 s, paid once for this whole binary.
struct SharedProject {
    /// The run's own working directory, held alive for the process's
    /// lifetime: every test that reads [`SharedProject::project`] does so
    /// long after this struct's builder closure returns, so the directory
    /// must outlive the `OnceLock` cell, not just that closure. The
    /// `OnceLock` static is never dropped, so `common::TmpDir::drop`'s
    /// `remove_dir_all` never runs for this one.
    cwd: common::TmpDir,
    project: PathBuf,
}

fn shared_project() -> &'static SharedProject {
    static ONCE: OnceLock<SharedProject> = OnceLock::new();
    ONCE.get_or_init(|| {
        let cwd = common::tmp_dir("export_cli_shared_stack");
        let out = cwd.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let input = common::fixtures_dir().join("mantis-512");
        let mut args = vec![
            "stack".to_string(),
            input.to_str().unwrap().to_string(),
            "-o".to_string(),
            out.to_str().unwrap().to_string(),
            "--algo".to_string(),
            "laplacian".to_string(),
            "--no-align".to_string(),
            "--no-crop".to_string(),
        ];
        // The GPU path's ±1 LSB budget would break the exact-byte equality
        // this file asserts against a CPU library oracle -- the same reason
        // `stack_cli.rs`'s shared run pins `--no-gpu`.
        if cfg!(feature = "gpu") {
            args.push("--no-gpu".to_string());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = run_cli_in(&cwd, &args);
        assert!(
            result.success,
            "stack must succeed -- stderr: {}",
            result.stderr
        );
        let project = only_project(&out);
        SharedProject { cwd, project }
    })
}

#[test]
fn exported_image_is_the_zmap_gather() {
    let shared = shared_project();
    let dir = common::tmp_dir("export_cli_gather");
    let out = dir.join("out.tif");
    let result = run_cli_in(
        &shared.cwd,
        &[
            "export",
            shared.project.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);
    assert!(
        !dir.join("out.tif.partial").exists(),
        "a temp file survived a successful export"
    );

    let mut project = Project::open(&shared.project).unwrap();
    let head = project.checkpoint().clone();
    let in_process = dir.join("in-process.tif");
    project::export_checkpoint(
        &mut project,
        &head.id,
        &in_process,
        &project::export::ExportOptions::default(),
    )
    .expect("in-process export must succeed");
    assert!(
        std::fs::read(&out).unwrap() == std::fs::read(&in_process).unwrap(),
        "the CLI export must equal the in-process export byte for byte"
    );

    // The `--no-align --no-crop` run gathers straight from the frames.
    let zmap_path = dir.join("head-zmap.tif");
    project::materialize_zmap(&mut project, &head, &zmap_path).unwrap();
    let map = ZMapReader::open(&zmap_path).unwrap();
    let (width, height) = map.dimensions();
    let whole = branched_full(width, height);
    let mut indices = vec![0u16; (width * height) as usize];
    map.read_tile_into(whole, &mut indices).unwrap();
    let stack = TiffStack::open(common::fixtures_dir().join("mantis-512")).unwrap();
    let spp = stack.header(0).samples_per_pixel;
    let sources = Sources {
        reader: &stack,
        frame_count: stack.len() as u32,
        samples_per_pixel: spp,
        origin: (0, 0),
    };
    let mut expected = vec![0u16; indices.len() * spp as usize];
    recomposite(&indices, whole, &sources, &mut expected).unwrap();

    let exported = TiffStack::open_files(std::slice::from_ref(&out)).unwrap();
    assert_eq!(exported.dimensions(), (width, height));
    let tile = exported.read_tile(0, whole).unwrap();
    assert!(
        tile.samples == expected,
        "the exported pixels must be the head's Z-Map gathered over the stack's frames"
    );
}

#[test]
fn exported_zmap_matches_the_golden_map() {
    let shared = shared_project();
    let dir = common::tmp_dir("export_cli_zmap");
    let out = dir.join("out-zmap.tif");
    let result = run_cli_in(
        &shared.cwd,
        &[
            "export",
            shared.project.to_str().unwrap(),
            "--zmap",
            out.to_str().unwrap(),
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);

    let golden = common::fixtures_dir()
        .join("mantis-512")
        .join("golden")
        .join("laplacian-zmap.tif");
    assert_eq!(
        std::fs::read(&out).unwrap(),
        std::fs::read(&golden).unwrap(),
        "the exported Z-Map must equal the pinned golden map byte for byte -- the \
         Z-Map is invariant to tile geometry, the composite is not"
    );
}

#[test]
fn export_needs_at_least_one_output_flag() {
    let shared = shared_project();
    let result = run_cli_in(&shared.cwd, &["export", shared.project.to_str().unwrap()]);
    assert!(!result.success);
    assert!(
        result.stderr.contains("-o") && result.stderr.contains("--zmap"),
        "the refusal must name both flags: {}",
        result.stderr
    );
}

#[test]
fn export_warns_about_unresolved_sources_and_still_writes() {
    // A project written with `-o <dir>` records absolute, `as-given`
    // sources by construction: "relative" means descendant of the
    // project's own directory.
    let shared = shared_project();
    let dir = common::tmp_dir("export_cli_unresolved");
    let out = dir.join("moved.tif");
    let result = run_cli_in(
        &shared.cwd,
        &[
            "export",
            shared.project.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ],
    );
    assert!(
        result.success,
        "an unresolved source is a warning, not a refusal"
    );
    assert!(
        result.stderr.contains("unresolved"),
        "the warning must appear on stderr: {}",
        result.stderr
    );
    assert!(
        result.stderr.contains("re-reads every source frame")
            && !result.stderr.contains("unaffected"),
        "the warning must say an image export needs every source: {}",
        result.stderr
    );
    assert!(
        result.stderr.contains("00.tif") || result.stderr.contains(".tif"),
        "the warning must name the sources: {}",
        result.stderr
    );
    assert!(out.exists(), "the image is still written");
}

#[test]
fn export_json_reports_what_it_wrote() {
    let shared = shared_project();
    let dir = common::tmp_dir("export_cli_json");
    let composite = dir.join("out.tif");
    let zmap = dir.join("out-zmap.tif");
    let result = run_cli_in(
        &shared.cwd,
        &[
            "export",
            shared.project.to_str().unwrap(),
            "-o",
            composite.to_str().unwrap(),
            "--zmap",
            zmap.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);
    let report: serde_json::Value = serde_json::from_str(&result.stdout).expect("--json parses");
    let wrote: Vec<String> = report["wrote"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        wrote.len(),
        2,
        "one invocation emitted both payloads: {wrote:?}"
    );
    assert_eq!(report["width"], serde_json::json!(512));
    assert_eq!(report["height"], serde_json::json!(512));
    assert_eq!(report["samples_per_pixel"], serde_json::json!(3));
    assert_eq!(report["algorithm"], serde_json::json!("laplacian"));
    assert!(report["frame_count"].as_u64().unwrap() >= 2);
    assert!(!report["unresolved_sources"].as_array().unwrap().is_empty());
    assert!(composite.exists() && zmap.exists());
}

// `--checkpoint`: every history below is built directly through `core`'s
// append surface -- no CLI command produces a re-fuse, so a branched,
// re-fused history is a synthetic project rather than a `stack` run.

const BW: u32 = 16;
const BH: u32 = 12;
const BSPP: u32 = 3;
const BFRAMES: usize = 3;

fn branched_full(width: u32, height: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

fn write_branched_tiff(
    path: &Path,
    w: u32,
    h: u32,
    spp: u32,
    samples: &[u16],
    description: Option<String>,
) {
    let mut writer = TiffWriter::create(
        path,
        TiffSpec {
            width: w,
            height: h,
            samples_per_pixel: spp,
            description,
        },
    )
    .unwrap();
    writer.write_tile(branched_full(w, h), samples).unwrap();
    writer.finish().unwrap();
}

fn write_branched_zmap(path: &Path, w: u32, h: u32, frames: u32, samples: &[u16]) {
    let mut map = ZMapWriter::create(path, ZMapInfo::new(w, h, frames).unwrap()).unwrap();
    map.write_tile(branched_full(w, h), samples).unwrap();
    map.finish().unwrap();
}

/// A fused root, a smaller re-fuse (a second fused checkpoint, child of the
/// root) and one op-bearing child on each side of that branch: `branch_a`
/// off the root directly, appended while the re-fuse is the head, and
/// `branch_b` off the re-fuse -- the project's final head.
struct Branched {
    _dir: common::TmpDir,
    project_path: PathBuf,
    source_paths: Vec<PathBuf>,
    root: String,
    refuse: String,
    branch_a: String,
    branch_b: String,
}

const ROOT_COMPOSITE_VALUE: u16 = 9000;
const REFUSE_COMPOSITE_VALUE: u16 = 7000;
/// The re-fuse's extent: cropped to `(1, 1, BW - 2, BH - 2)`.
const RW: u32 = BW - 2;
const RH: u32 = BH - 2;

/// The extent every checkpoint of [`build_branched`] renders at.
fn branched_extent(branched: &Branched, id: &str) -> (u32, u32) {
    if id == branched.root || id == branched.branch_a {
        (BW, BH)
    } else {
        (RW, RH)
    }
}

fn build_branched(name: &str) -> Branched {
    let dir = common::tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    let source_paths: Vec<PathBuf> = (0..BFRAMES)
        .map(|i| {
            let path = dir.join(format!("frame-{i:02}.tif"));
            let samples: Vec<u16> = (0..(BW * BH * BSPP) as usize)
                .map(|j| ((j + i * 97) % 65_535) as u16)
                .collect();
            write_branched_tiff(&path, BW, BH, BSPP, &samples, None);
            path
        })
        .collect();

    let root_composite_path = staging.join("root-composite.tif");
    write_branched_tiff(
        &root_composite_path,
        BW,
        BH,
        BSPP,
        &vec![ROOT_COMPOSITE_VALUE; (BW * BH * BSPP) as usize],
        Some("staged composite".to_string()),
    );
    let root_zmap_path = staging.join("root-zmap.tif");
    let root_zmap: Vec<u16> = (0..(BW * BH) as usize)
        .map(|i| (i % BFRAMES) as u16)
        .collect();
    write_branched_zmap(&root_zmap_path, BW, BH, BFRAMES as u32, &root_zmap);

    let project_path = dir.join(format!("branched.{}", project::PROJECT_EXTENSION));
    let sealed = project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &root_composite_path,
        staged_zmap: &root_zmap_path,
        sources: &source_paths,
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        crop: None,
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");
    let root = sealed.checkpoint;

    let refuse_composite_path = staging.join("refuse-composite.tif");
    write_branched_tiff(
        &refuse_composite_path,
        RW,
        RH,
        BSPP,
        &vec![REFUSE_COMPOSITE_VALUE; (RW * RH * BSPP) as usize],
        Some("re-fused composite".to_string()),
    );
    let refuse_zmap_path = staging.join("refuse-zmap.tif");
    let refuse_zmap: Vec<u16> = (0..(RW * RH) as usize)
        .map(|i| ((i + 1) % BFRAMES) as u16)
        .collect();
    write_branched_zmap(&refuse_zmap_path, RW, RH, BFRAMES as u32, &refuse_zmap);
    let refused = project::append(AppendRequest {
        project_path: &project_path,
        staged_composite: &refuse_composite_path,
        staged_zmap: &refuse_zmap_path,
        sources: &source_paths,
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        crop: Some(Rect {
            x: 1,
            y: 1,
            width: RW,
            height: RH,
        }),
        composite_description: "re-fused composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("append (re-fuse) must succeed");
    let refuse = refused.checkpoint;

    let branch_a = project::append_ops_onto(
        &project_path,
        &root,
        vec![Op::BrushStroke {
            source_id: 1,
            path: vec![Point { x: 2, y: 2 }],
            radius: 1.5,
            softness: 0.0,
        }],
        Some("branch-a".to_string()),
        1,
        ZMapStorage::Bounded,
    )
    .expect("branch a must succeed")
    .checkpoint;

    let branch_b = project::append_ops_onto(
        &project_path,
        &refuse,
        vec![Op::BrushStroke {
            source_id: 2,
            path: vec![Point { x: 3, y: 3 }],
            radius: 1.5,
            softness: 0.0,
        }],
        Some("branch-b".to_string()),
        2,
        ZMapStorage::Bounded,
    )
    .expect("branch b must succeed")
    .checkpoint;

    Branched {
        _dir: dir,
        project_path,
        source_paths,
        root,
        refuse,
        branch_a,
        branch_b,
    }
}

fn partial_of(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap().to_os_string();
    name.push(".partial");
    path.with_file_name(name)
}

#[test]
fn exports_every_checkpoint_of_a_branched_history() {
    let branched = build_branched("export_cli_branched");
    let dir = common::tmp_dir("export_cli_branched_out");
    let cwd = branched.project_path.parent().unwrap();
    for (label, id) in [
        ("root", &branched.root),
        ("refuse", &branched.refuse),
        ("branch_a", &branched.branch_a),
        ("branch_b", &branched.branch_b),
    ] {
        let cli_out = dir.join(format!("{label}.tif"));
        let result = run_cli_in(
            cwd,
            &[
                "export",
                branched.project_path.to_str().unwrap(),
                "--checkpoint",
                id,
                "-o",
                cli_out.to_str().unwrap(),
            ],
        );
        assert!(result.success, "{label}: stderr: {}", result.stderr);

        let in_process_out = dir.join(format!("{label}-in-process.tif"));
        let mut project = Project::open(&branched.project_path).unwrap();
        project::export_checkpoint(
            &mut project,
            id,
            &in_process_out,
            &project::export::ExportOptions::default(),
        )
        .expect("in-process export must succeed");
        assert_eq!(
            std::fs::read(&cli_out).unwrap(),
            std::fs::read(&in_process_out).unwrap(),
            "{label}: CLI export must equal the in-process export byte for byte"
        );

        let stack = TiffStack::open_files(std::slice::from_ref(&cli_out)).unwrap();
        assert_eq!(
            stack.dimensions(),
            branched_extent(&branched, id),
            "{label}: wrong shape"
        );
        assert_eq!(
            stack.header(0).samples_per_pixel,
            BSPP,
            "{label}: wrong sample depth"
        );
    }
}

#[test]
fn fused_export_without_sources_is_refused() {
    let dir = common::tmp_dir("export_cli_fused_refused_out");

    // Every source missing: each one named on its own line.
    let missing = build_branched("export_cli_fused_missing");
    for path in &missing.source_paths {
        std::fs::remove_file(path).unwrap();
    }
    let out = dir.join("root-missing.tif");
    let result = run_cli_in(
        missing.project_path.parent().unwrap(),
        &[
            "export",
            missing.project_path.to_str().unwrap(),
            "--checkpoint",
            &missing.root,
            "-o",
            out.to_str().unwrap(),
        ],
    );
    assert!(!result.success, "stdout: {}", result.stdout);
    assert!(!out.exists() && !partial_of(&out).exists());
    assert!(
        result.stderr.contains(&missing.root),
        "the refusal must name the checkpoint: {}",
        result.stderr
    );
    for path in &missing.source_paths {
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            result
                .stderr
                .lines()
                .filter(|line| line.contains("missing:") && line.contains(name))
                .count(),
            1,
            "{name} must be listed on a line of its own: {}",
            result.stderr
        );
    }

    // `--zmap` alone reads no source frame.
    let zmap_out = dir.join("root-zmap.tif");
    let result = run_cli_in(
        missing.project_path.parent().unwrap(),
        &[
            "export",
            missing.project_path.to_str().unwrap(),
            "--checkpoint",
            &missing.root,
            "--zmap",
            zmap_out.to_str().unwrap(),
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);

    // Changed sources, each one named.
    let changed = build_branched("export_cli_fused_changed");
    for path in &changed.source_paths[1..] {
        let mut bytes = std::fs::read(path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(path, &bytes).unwrap();
    }
    let out = dir.join("refuse-changed.tif");
    let result = run_cli_in(
        changed.project_path.parent().unwrap(),
        &[
            "export",
            changed.project_path.to_str().unwrap(),
            "--checkpoint",
            &changed.refuse,
            "-o",
            out.to_str().unwrap(),
        ],
    );
    assert!(!result.success, "stdout: {}", result.stdout);
    assert!(!out.exists() && !partial_of(&out).exists());
    for path in &changed.source_paths[1..] {
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(
            result
                .stderr
                .lines()
                .any(|line| line.contains("changed:") && line.contains(name)),
            "{name} must be listed as changed: {}",
            result.stderr
        );
    }
}

#[test]
fn plain_export_of_an_edited_head_renders_the_edit() {
    let branched = build_branched("export_cli_plain_head");
    let dir = common::tmp_dir("export_cli_plain_head_out");
    let out = dir.join("head.tif");
    let result = run_cli_in(
        branched.project_path.parent().unwrap(),
        &[
            "export",
            branched.project_path.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);

    let mut project = Project::open(&branched.project_path).unwrap();
    let expected = dir.join("head-expected.tif");
    project::export_checkpoint(
        &mut project,
        &branched.branch_b,
        &expected,
        &project::export::ExportOptions::default(),
    )
    .expect("in-process export of the head must succeed");
    assert_eq!(
        std::fs::read(&out).unwrap(),
        std::fs::read(&expected).unwrap(),
        "plain export must render the head's own edit, not the fused ancestor's stale composite"
    );

    // Proves the assertion above is not vacuous: the export really differs
    // from the ancestor's stale composite, rather than the edit having
    // touched no pixel.
    let stack = TiffStack::open_files(std::slice::from_ref(&out)).unwrap();
    let tile = stack.read_tile(0, branched_full(RW, RH)).unwrap();
    assert_ne!(
        tile.samples,
        vec![REFUSE_COMPOSITE_VALUE; (RW * RH * BSPP) as usize],
        "the export must differ from the ancestor's stale composite"
    );
}

#[test]
fn export_zmap_of_a_checkpoint_matches_history_materialize() {
    let branched = build_branched("export_cli_zmap_checkpoint");
    let dir = common::tmp_dir("export_cli_zmap_checkpoint_out");
    let out = dir.join("branch-a-zmap.tif");
    let result = run_cli_in(
        branched.project_path.parent().unwrap(),
        &[
            "export",
            branched.project_path.to_str().unwrap(),
            "--checkpoint",
            &branched.branch_a,
            "--zmap",
            out.to_str().unwrap(),
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);

    let mut project = Project::open(&branched.project_path).unwrap();
    let checkpoint = project.history().get(&branched.branch_a).unwrap().clone();
    let expected = dir.join("branch-a-zmap-expected.tif");
    project::materialize_zmap(&mut project, &checkpoint, &expected)
        .expect("in-process materialize must succeed");
    assert_eq!(
        std::fs::read(&out).unwrap(),
        std::fs::read(&expected).unwrap(),
        "export --zmap must equal history materialize's own map byte for byte"
    );

    let json_out = dir.join("branch-a-zmap.json.tif");
    let json_result = run_cli_in(
        branched.project_path.parent().unwrap(),
        &[
            "export",
            branched.project_path.to_str().unwrap(),
            "--checkpoint",
            &branched.branch_a,
            "--zmap",
            json_out.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(json_result.success, "stderr: {}", json_result.stderr);
    let report: serde_json::Value = serde_json::from_str(&json_result.stdout).unwrap();
    assert_eq!(report["checkpoint"], serde_json::json!(branched.branch_a));
}

#[test]
fn materialize_uses_the_checkpoints_own_geometry() {
    let branched = build_branched("export_cli_own_geometry");
    let dir = common::tmp_dir("export_cli_own_geometry_out");
    let cwd = branched.project_path.parent().unwrap();
    let project_arg = branched.project_path.to_str().unwrap();
    for (label, id) in [
        ("root", &branched.root),
        ("refuse", &branched.refuse),
        ("branch_a", &branched.branch_a),
        ("branch_b", &branched.branch_b),
    ] {
        let exported = dir.join(format!("{label}-export-zmap.tif"));
        let materialized = dir.join(format!("{label}-materialize.tif"));
        let image = dir.join(format!("{label}.tif"));
        for (args, out) in [
            (
                vec!["export", project_arg, "--checkpoint", id, "--zmap"],
                &exported,
            ),
            (
                vec![
                    "history",
                    "materialize",
                    project_arg,
                    "--checkpoint",
                    id,
                    "-o",
                ],
                &materialized,
            ),
            (
                vec!["export", project_arg, "--checkpoint", id, "-o"],
                &image,
            ),
        ] {
            let mut args = args;
            args.push(out.to_str().unwrap());
            let result = run_cli_in(cwd, &args);
            assert!(
                result.success,
                "{label} {args:?}: stderr: {}",
                result.stderr
            );
        }
        let extent = branched_extent(&branched, id);
        for map_path in [&exported, &materialized] {
            let map = ZMapReader::open(map_path).unwrap();
            assert_eq!(map.dimensions(), extent, "{label}: {map_path:?}");
            assert_eq!(map.frame_count(), BFRAMES as u32, "{label}: {map_path:?}");
        }
        let stack = TiffStack::open_files(std::slice::from_ref(&image)).unwrap();
        assert_eq!(stack.dimensions(), extent, "{label}: image");
    }
}

fn read_entry_bytes(path: &Path, name: &str) -> Vec<u8> {
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut bytes = Vec::new();
    archive
        .by_name(name)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

fn read_checkpoint_json(path: &Path, id: &str) -> serde_json::Value {
    serde_json::from_slice(&read_entry_bytes(path, &format!("checkpoints/{id}.json"))).unwrap()
}

/// Copies `src` to `dst`, dropping the entries named in `drop` and replacing
/// the entries in `replace` -- a raw rewrite of a real container, so a test
/// can inject a record the production writer would never produce (an older
/// fuse whose `fuse` record was stripped).
fn mutate_container(src: &Path, dst: &Path, drop: &[&str], replace: &[(String, Vec<u8>)]) {
    use std::io::{Read, Write};
    let mut archive = zip::ZipArchive::new(std::fs::File::open(src).unwrap()).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    let mut writer = zip::write::ZipWriter::new(std::fs::File::create(dst).unwrap());
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for name in &names {
        if drop.contains(&name.as_str()) {
            continue;
        }
        let mut bytes = Vec::new();
        archive
            .by_name(name)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        if let Some(entry) = replace.iter().find(|entry| &entry.0 == name) {
            bytes = entry.1.clone();
        }
        writer.start_file(name.as_str(), options).unwrap();
        writer.write_all(&bytes).unwrap();
    }
    writer.finish().unwrap();
}

fn assert_refused(dir: &Path, args: &[&str], out: &Path) {
    let result = run_cli_in(dir, args);
    assert!(
        !result.success,
        "must fail for {args:?} -- stdout: {}",
        result.stdout
    );
    assert!(!out.exists(), "a refusal must leave no output: {out:?}");
    assert!(
        !partial_of(out).exists(),
        "a refusal must leave no .partial: {out:?}"
    );
}

/// A fuse record naming an algorithm but carrying no per-frame transforms
/// (`AlignRecord::transforms` is `#[serde(default)]`, so an older record can
/// legally omit it), plus one op-bearing child. Returns the fused root's id
/// and the child's.
fn build_unrecorded_alignment(name: &str) -> (common::TmpDir, PathBuf, String, String) {
    let dir = common::tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    let source_paths: Vec<PathBuf> = (0..BFRAMES)
        .map(|i| {
            let path = dir.join(format!("frame-{i:02}.tif"));
            write_branched_tiff(
                &path,
                BW,
                BH,
                BSPP,
                &vec![0u16; (BW * BH * BSPP) as usize],
                None,
            );
            path
        })
        .collect();

    let composite_path = staging.join("composite.tif");
    write_branched_tiff(
        &composite_path,
        BW,
        BH,
        BSPP,
        &vec![1234u16; (BW * BH * BSPP) as usize],
        Some("staged composite".to_string()),
    );
    let zmap_path = staging.join("zmap.tif");
    write_branched_zmap(
        &zmap_path,
        BW,
        BH,
        BFRAMES as u32,
        &vec![0u16; (BW * BH) as usize],
    );

    let align = AlignRecord {
        algorithm: "identity".to_string(),
        params: ParamValues::new(),
        reference: 0,
        transforms: vec![],
    };

    let project_path = dir.join(format!("align.{}", project::PROJECT_EXTENSION));
    let sealed = project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite_path,
        staged_zmap: &zmap_path,
        sources: &source_paths,
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: Some(align),
        crop: None,
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");

    let child = project::append_ops_onto(
        &project_path,
        &sealed.checkpoint,
        vec![Op::BrushStroke {
            source_id: 1,
            path: vec![Point { x: 1, y: 1 }],
            radius: 1.0,
            softness: 0.0,
        }],
        None,
        1,
        ZMapStorage::Bounded,
    )
    .expect("append_ops_onto must succeed");

    (dir, project_path, sealed.checkpoint, child.checkpoint)
}

#[test]
fn refused_checkpoint_export_leaves_nothing() {
    let dir = common::tmp_dir("export_cli_refused");

    // Unknown checkpoint id.
    let branched = build_branched("export_cli_refused_unknown");
    let out = dir.join("unknown.tif");
    assert_refused(
        branched.project_path.parent().unwrap(),
        &[
            "export",
            branched.project_path.to_str().unwrap(),
            "--checkpoint",
            "not-a-real-id",
            "-o",
            out.to_str().unwrap(),
        ],
        &out,
    );

    // FuseStateUnknown: after the re-fuse moved the head's fused ancestor
    // to `refuse`, `root`'s own `fuse` record is stripped -- root is still
    // fused (empty ops) but no longer the head's ancestor, so the manifest
    // fallback cannot recover it either.
    let fuse_unknown = build_branched("export_cli_refused_fuse_unknown");
    let mut stripped = read_checkpoint_json(&fuse_unknown.project_path, &fuse_unknown.root);
    stripped.as_object_mut().unwrap().remove("fuse");
    let mutated = dir.join("fuse-unknown.ffp");
    mutate_container(
        &fuse_unknown.project_path,
        &mutated,
        &[],
        &[(
            format!("checkpoints/{}.json", fuse_unknown.root),
            serde_json::to_vec_pretty(&stripped).unwrap(),
        )],
    );
    let out = dir.join("fuse-unknown.tif");
    assert_refused(
        &dir,
        &[
            "export",
            mutated.to_str().unwrap(),
            "--checkpoint",
            &fuse_unknown.root,
            "-o",
            out.to_str().unwrap(),
        ],
        &out,
    );

    // The same fused checkpoint from the *unmutated* project, with every
    // source moved away: its image is gathered from those sources, so it
    // is refused too.
    for path in &fuse_unknown.source_paths {
        std::fs::remove_file(path).unwrap();
    }
    let out = dir.join("fused-without-sources.tif");
    assert_refused(
        fuse_unknown.project_path.parent().unwrap(),
        &[
            "export",
            fuse_unknown.project_path.to_str().unwrap(),
            "--checkpoint",
            &fuse_unknown.root,
            "-o",
            out.to_str().unwrap(),
        ],
        &out,
    );

    // Missing sources, each one named.
    let missing = build_branched("export_cli_refused_missing_sources");
    std::fs::remove_file(&missing.source_paths[0]).unwrap();
    std::fs::remove_file(&missing.source_paths[1]).unwrap();
    let out = dir.join("missing-sources.tif");
    let result = run_cli_in(
        missing.project_path.parent().unwrap(),
        &[
            "export",
            missing.project_path.to_str().unwrap(),
            "--checkpoint",
            &missing.branch_a,
            "-o",
            out.to_str().unwrap(),
        ],
    );
    assert!(!result.success);
    assert!(!out.exists());
    assert!(!partial_of(&out).exists());
    for path in &missing.source_paths[..2] {
        assert!(
            result
                .stderr
                .contains(path.file_name().unwrap().to_str().unwrap()),
            "each missing source must be named: {}",
            result.stderr
        );
    }

    // A changed source, named.
    let changed = build_branched("export_cli_refused_changed_sources");
    let mut bytes = std::fs::read(&changed.source_paths[2]).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    std::fs::write(&changed.source_paths[2], &bytes).unwrap();
    let out = dir.join("changed-sources.tif");
    let result = run_cli_in(
        changed.project_path.parent().unwrap(),
        &[
            "export",
            changed.project_path.to_str().unwrap(),
            "--checkpoint",
            &changed.branch_a,
            "-o",
            out.to_str().unwrap(),
        ],
    );
    assert!(!result.success);
    assert!(!out.exists());
    assert!(!partial_of(&out).exists());
    assert!(
        result.stderr.contains(
            changed.source_paths[2]
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
        ),
        "the changed source must be named: {}",
        result.stderr
    );

    // Unrecorded alignment: a fuse record naming an algorithm but no
    // per-frame transforms.
    let (_align_dir, align_project, align_root, align_child) =
        build_unrecorded_alignment("export_cli_refused_alignment");
    for (label, checkpoint) in [("fused", &align_root), ("op-bearing", &align_child)] {
        let out = dir.join(format!("alignment-{label}.tif"));
        assert_refused(
            align_project.parent().unwrap(),
            &[
                "export",
                align_project.to_str().unwrap(),
                "--checkpoint",
                checkpoint,
                "-o",
                out.to_str().unwrap(),
            ],
            &out,
        );
    }

    // An unwritable destination.
    let readonly_dir = dir.join("readonly");
    std::fs::create_dir_all(&readonly_dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&readonly_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    }
    let unwritable = build_branched("export_cli_refused_unwritable");
    let out = readonly_dir.join("out.tif");
    let result = run_cli_in(
        unwritable.project_path.parent().unwrap(),
        &[
            "export",
            unwritable.project_path.to_str().unwrap(),
            "--checkpoint",
            &unwritable.root,
            "-o",
            out.to_str().unwrap(),
        ],
    );
    assert!(!result.success);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&readonly_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert!(!out.exists());
    assert!(!partial_of(&out).exists());

    // Positive: a unique id prefix is accepted.
    let prefix_project = build_branched("export_cli_refused_prefix");
    let prefix = &prefix_project.branch_a[..8];
    let out = dir.join("prefix.tif");
    let result = run_cli_in(
        prefix_project.project_path.parent().unwrap(),
        &[
            "export",
            prefix_project.project_path.to_str().unwrap(),
            "--checkpoint",
            prefix,
            "-o",
            out.to_str().unwrap(),
        ],
    );
    assert!(
        result.success,
        "a unique id prefix must resolve: stderr: {}",
        result.stderr
    );
}
