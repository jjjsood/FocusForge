// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli export`: a project in, an image out.
//!
//! One `laplacian` run over `fixtures/mantis-512` is shared through a
//! `OnceLock`; its export is compared against an in-process export and the
//! head's stored composite.
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

use focusforge_core::algorithm::TileReader;
use focusforge_core::align::{Registration, WarpingReader};
use focusforge_core::io::{Rect, TiffHeader, TiffSpec, TiffStack, TiffWriter, open_frame_set};
use focusforge_core::params::ParamValues;
use focusforge_core::project::container::{ContainerWriter, Index};
use focusforge_core::project::{
    self, AlignRecord, AppendRequest, Metrics, Op, Plane, Point, Project, SealRequest, partial_path,
};
use focusforge_core::tile::HostBudget;
use focusforge_core::zmap::{ZMapInfo, ZMapReader, ZMapWriter};

use common::CliOutput;

fn bin() -> Command {
    common::cli_bin("export_cli")
}

fn run_cli_in(dir: &Path, args: &[&str]) -> CliOutput {
    common::run_in(&mut bin(), dir, args)
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
            "--algorithm".to_string(),
            "laplacian".to_string(),
            "--no-align".to_string(),
            "--no-crop".to_string(),
            "--set".to_string(),
            "equalize=false".to_string(),
        ];
        if cfg!(feature = "gpu") {
            args.push("--cpu".to_string());
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

/// `project`'s head composite as stored, materialized under `dir`: its
/// width, height and samples.
fn stored_composite(project: &mut Project, dir: &Path) -> (u32, u32, Vec<u16>) {
    let path = dir.join("head-composite.tif");
    project::materialize_plane(project, Plane::Composite, &path).unwrap();
    let stack = TiffStack::open_files(&[path]).unwrap();
    let (width, height) = stack.dimensions();
    let samples = stack
        .read_tile(0, branched_full(width, height))
        .unwrap()
        .samples;
    (width, height, samples)
}

#[test]
fn exported_image_is_the_stored_composite() {
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
        &project::export::MetadataOptions::default(),
    )
    .expect("in-process export must succeed");
    assert!(
        std::fs::read(&out).unwrap() == std::fs::read(&in_process).unwrap(),
        "the CLI export must equal the in-process export byte for byte"
    );

    let (width, height, stored) = stored_composite(&mut project, &dir);
    let exported = TiffStack::open_files(std::slice::from_ref(&out)).unwrap();
    assert_eq!(exported.dimensions(), (width, height));
    let tile = exported.read_tile(0, branched_full(width, height)).unwrap();
    assert!(
        tile.samples == stored,
        "the exported pixels must be the head's stored composite"
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
        result
            .stderr
            .contains("unedited image export reads no source frame")
            && !result.stderr.contains("re-reads every source frame"),
        "the warning must say an unedited export needs no source: {}",
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
        registration: Registration::default(),
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
        registration: Registration {
            transforms: Vec::new(),
            crop: Some(Rect {
                x: 1,
                y: 1,
                width: RW,
                height: RH,
            }),
            equalization: None,
        },
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
            &project::export::MetadataOptions::default(),
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
fn fused_export_needs_no_sources() {
    let dir = common::tmp_dir("export_cli_fused_unsourced_out");
    let export = |branched: &Branched, checkpoint: &str, out: &Path| {
        let result = run_cli_in(
            branched.project_path.parent().unwrap(),
            &[
                "export",
                branched.project_path.to_str().unwrap(),
                "--checkpoint",
                checkpoint,
                "-o",
                out.to_str().unwrap(),
            ],
        );
        assert!(result.success, "stderr: {}", result.stderr);
        assert!(!partial_of(out).exists());
        let stack = TiffStack::open_files(&[out.to_path_buf()]).unwrap();
        let (width, height) = stack.dimensions();
        stack
            .read_tile(0, branched_full(width, height))
            .unwrap()
            .samples
    };

    // Every source missing: the stored composite is written regardless.
    let missing = build_branched("export_cli_fused_missing");
    let intact = export(&missing, &missing.root, &dir.join("root-intact.tif"));
    for path in &missing.source_paths {
        std::fs::remove_file(path).unwrap();
    }
    let without = export(&missing, &missing.root, &dir.join("root-missing.tif"));
    assert!(without == intact, "sources must not change the export");

    // `--zmap` alone reads no source frame either.
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

    // Changed sources are not read either.
    let changed = build_branched("export_cli_fused_changed");
    let intact = export(&changed, &changed.refuse, &dir.join("refuse-intact.tif"));
    for path in &changed.source_paths[1..] {
        let mut bytes = std::fs::read(path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        std::fs::write(path, &bytes).unwrap();
    }
    let altered = export(&changed, &changed.refuse, &dir.join("refuse-changed.tif"));
    assert!(altered == intact, "sources must not change the export");
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
        &project::export::MetadataOptions::default(),
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

// ---------------------------------------------------------------------
// `--frame <INDEX> --frame-output <FILE>`: one source frame, as fused.
// ---------------------------------------------------------------------

/// One `stack` run that actually aligns and crops (`dummy-align`, the
/// default `--crop`), so the exported frame's extent genuinely differs from
/// a raw source frame's.
struct AlignedProject {
    cwd: common::TmpDir,
    project: PathBuf,
}

fn aligned_project() -> &'static AlignedProject {
    static ONCE: OnceLock<AlignedProject> = OnceLock::new();
    ONCE.get_or_init(|| {
        let cwd = common::tmp_dir("export_cli_aligned_stack");
        let out = cwd.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let input = common::fixtures_dir().join("mantis-512");
        let mut args = vec![
            "stack".to_string(),
            input.to_str().unwrap().to_string(),
            "-o".to_string(),
            out.to_str().unwrap().to_string(),
            "--algorithm".to_string(),
            "laplacian".to_string(),
            "--align-algorithm".to_string(),
            "dummy-align".to_string(),
            "--set".to_string(),
            "equalize=false".to_string(),
        ];
        if cfg!(feature = "gpu") {
            args.push("--cpu".to_string());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = run_cli_in(&cwd, &args);
        assert!(
            result.success,
            "an aligned, cropped stack must succeed -- stderr: {}",
            result.stderr
        );
        let project = only_project(&out);
        AlignedProject { cwd, project }
    })
}

/// The frame-as-fused view of `index`, built directly through the library
/// (never through `export`) -- the independent oracle `--frame` is checked
/// against.
fn oracle_fused_frame(project_path: &Path, index: usize) -> (u32, u32, u32, Vec<u16>) {
    let project = Project::open(project_path).expect("open project");
    let composite = project.plane(Plane::Composite).clone();
    let registration = project.manifest().registration.clone();
    let sources: Vec<PathBuf> = project
        .manifest()
        .sources
        .iter()
        .map(|source| project.source_path(source))
        .collect();

    let budget = HostBudget::host_derived(&focusforge_core::host::snapshot());
    let frame_set = open_frame_set(&sources, budget).expect("open frame set");
    let frame_dims = frame_set.reader.dimensions();
    let transforms = registration.transforms(frame_set.reader.len());
    let warping = WarpingReader::new(
        frame_set.reader.as_ref(),
        frame_dims,
        composite.samples_per_pixel,
        transforms,
    );
    let origin = registration.origin();
    let rect = Rect {
        x: origin.0,
        y: origin.1,
        width: composite.width,
        height: composite.height,
    };
    let mut samples =
        vec![0u16; (composite.width * composite.height * composite.samples_per_pixel) as usize];
    warping
        .read_tile_into(index, rect, &mut samples)
        .expect("read the fused frame directly through the library");
    (
        composite.width,
        composite.height,
        composite.samples_per_pixel,
        samples,
    )
}

#[test]
fn export_writes_one_frame_as_fused_at_the_composite_size() {
    let shared = aligned_project();
    let dir = common::tmp_dir("export_cli_frame");
    let out = dir.join("frame-05.tif");
    let result = run_cli_in(
        &shared.cwd,
        &[
            "export",
            shared.project.to_str().unwrap(),
            "--frame",
            "5",
            "--frame-output",
            out.to_str().unwrap(),
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);
    assert!(out.exists(), "the frame TIFF must have been written");
    assert!(
        !partial_path(&out).exists(),
        "no .partial must survive a successful export"
    );

    let header = TiffHeader::open(&out).expect("exported frame header");
    let (expected_w, expected_h, expected_spp, expected_samples) =
        oracle_fused_frame(&shared.project, 5);
    assert_eq!(
        (header.width, header.height),
        (expected_w, expected_h),
        "the exported frame must be at the composite's own extent"
    );
    assert_eq!(header.samples_per_pixel, expected_spp);

    let exported = TiffStack::open_files(std::slice::from_ref(&out))
        .expect("open the exported frame as a one-frame stack");
    let whole = Rect {
        x: 0,
        y: 0,
        width: header.width,
        height: header.height,
    };
    let got = exported
        .read_tile(0, whole)
        .expect("read the exported samples");
    assert_eq!(
        got.samples, expected_samples,
        "the exported frame must equal the fused view of that frame"
    );
}

#[test]
fn export_frame_out_of_range_is_refused_and_writes_nothing() {
    let shared = aligned_project();
    let dir = common::tmp_dir("export_cli_frame_out_of_range");
    let out = dir.join("none.tif");
    let result = run_cli_in(
        &shared.cwd,
        &[
            "export",
            shared.project.to_str().unwrap(),
            "--frame",
            "999",
            "--frame-output",
            out.to_str().unwrap(),
        ],
    );
    assert!(!result.success, "an out-of-range frame must be refused");
    assert!(
        result.stderr.contains("999") && result.stderr.contains('0'),
        "the refusal must name the valid range: {}",
        result.stderr
    );
    assert!(!out.exists(), "no output file must be written");
    assert!(
        !partial_path(&out).exists(),
        "no .partial must be left behind either"
    );
}

#[test]
fn export_zmap_of_a_checkpoint_matches_the_core_map() {
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
        "export --zmap must equal core's own map byte for byte"
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
fn exported_zmap_uses_the_checkpoints_own_geometry() {
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
        let image = dir.join(format!("{label}.tif"));
        for (args, out) in [
            (
                vec!["export", project_arg, "--checkpoint", id, "--zmap"],
                &exported,
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
        let map = ZMapReader::open(&exported).unwrap();
        assert_eq!(map.dimensions(), extent, "{label}: map");
        assert_eq!(map.frame_count(), BFRAMES as u32, "{label}: map");
        let stack = TiffStack::open_files(std::slice::from_ref(&image)).unwrap();
        assert_eq!(stack.dimensions(), extent, "{label}: image");
    }
}

#[test]
fn output_naming_the_project_is_refused() {
    let branched = build_branched("export_cli_output_names_project");
    let cwd = branched.project_path.parent().unwrap();
    let project_arg = branched.project_path.to_str().unwrap();
    let before = std::fs::read(&branched.project_path).unwrap();
    for flags in [
        vec!["-o", project_arg, "--format", "tiff"],
        vec!["--zmap", project_arg],
        vec!["--frame", "0", "--frame-output", project_arg],
    ] {
        let mut args = vec!["export", project_arg];
        args.extend(flags.iter().copied());
        let result = run_cli_in(cwd, &args);
        assert!(!result.success, "{args:?} must be refused");
        assert!(
            result.stderr.contains("also the project file"),
            "{args:?}: stderr: {}",
            result.stderr
        );
        assert_eq!(
            std::fs::read(&branched.project_path).unwrap(),
            before,
            "{args:?} must leave the project unchanged"
        );
    }
}

fn read_entry_bytes(path: &Path, name: &str) -> Vec<u8> {
    Index::read(path).unwrap().read_entry(path, name).unwrap()
}

fn read_checkpoint_json(path: &Path, id: &str) -> serde_json::Value {
    serde_json::from_slice(&read_entry_bytes(path, &format!("checkpoints/{id}.json"))).unwrap()
}

/// Copies `src` to `dst`, dropping the entries named in `drop` and replacing
/// the entries in `replace` -- a raw rewrite of a real container, so a test
/// can inject a record the production writer would never produce (a fused
/// checkpoint whose `fuse` record was stripped).
fn mutate_container(src: &Path, dst: &Path, drop: &[&str], replace: &[(String, Vec<u8>)]) {
    let index = Index::read(src).unwrap();
    let mut writer = ContainerWriter::create(dst).unwrap();
    for name in index.names() {
        if drop.contains(&name) {
            continue;
        }
        let bytes = match replace.iter().find(|entry| entry.0 == name) {
            Some(entry) => entry.1.clone(),
            None => index.read_entry(src, name).unwrap(),
        };
        writer.put_bytes(name, &bytes).unwrap();
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

/// A fuse record naming an algorithm whose `Registration` is the identity
/// (no per-frame transforms), plus one op-bearing child. Returns the fused
/// root's id and the child's.
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
        registration: Registration::default(),
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

    // A fused checkpoint without its `fuse` record is refused at open:
    // `root`'s own record is stripped after the re-fuse moved the head's
    // fused ancestor to `refuse`.
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
    // source moved away: its stored composite is exported without them.
    for path in &fuse_unknown.source_paths {
        std::fs::remove_file(path).unwrap();
    }
    let out = dir.join("fused-without-sources.tif");
    let result = run_cli_in(
        fuse_unknown.project_path.parent().unwrap(),
        &[
            "export",
            fuse_unknown.project_path.to_str().unwrap(),
            "--checkpoint",
            &fuse_unknown.root,
            "-o",
            out.to_str().unwrap(),
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);
    assert!(out.exists());

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
        let args = [
            "export",
            align_project.to_str().unwrap(),
            "--checkpoint",
            checkpoint,
            "-o",
            out.to_str().unwrap(),
        ];
        if label == "fused" {
            let result = run_cli_in(align_project.parent().unwrap(), &args);
            assert!(result.success, "stderr: {}", result.stderr);
            assert!(out.exists());
        } else {
            assert_refused(align_project.parent().unwrap(), &args, &out);
        }
    }

    // An unwritable destination. A read-only directory is a Unix permission; Windows has no
    // mode to refuse with.
    #[cfg(unix)]
    {
        let readonly_dir = dir.join("readonly");
        std::fs::create_dir_all(&readonly_dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&readonly_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
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
        std::fs::set_permissions(&readonly_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!out.exists());
        assert!(!partial_of(&out).exists());
    }

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

#[test]
fn export_frame_without_frame_output_is_a_clap_usage_error() {
    let shared = aligned_project();
    let result = run_cli_in(
        &shared.cwd,
        &["export", shared.project.to_str().unwrap(), "--frame", "0"],
    );
    assert!(
        !result.success,
        "--frame without --frame-output must be a usage error"
    );
    assert!(
        result.stderr.contains("frame-output") || result.stderr.contains("frame_output"),
        "clap's usage error must name the missing flag: {}",
        result.stderr
    );
}

#[test]
fn export_frame_output_alone_is_refused_by_name() {
    let shared = aligned_project();
    let dir = common::tmp_dir("export_cli_frame_output_alone");
    let out = dir.join("frame.tif");
    let result = run_cli_in(
        &shared.cwd,
        &[
            "export",
            shared.project.to_str().unwrap(),
            "--frame-output",
            out.to_str().unwrap(),
        ],
    );
    assert!(!result.success, "--frame-output alone must be refused");
    assert!(
        result.stderr.contains("--frame"),
        "the refusal must name --frame: {}",
        result.stderr
    );
    assert!(!out.exists());
}

#[test]
fn export_frame_applies_the_recorded_equalization() {
    let cwd = common::tmp_dir("export_cli_equalized_frame");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let input = common::fixtures_dir().join("mantis-512");
    let stacked = run_cli_in(
        &cwd,
        &common::push_no_gpu(vec![
            "stack",
            input.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "weighted-average",
            "--no-align",
            "--no-crop",
            "--set",
            "equalize=true",
        ]),
    );
    assert!(stacked.success, "stderr: {}", stacked.stderr);
    let project_path = only_project(&out_dir);
    let project = Project::open(&project_path).expect("open project");
    let gains = project
        .manifest()
        .registration
        .equalization
        .clone()
        .expect("the project records the equalization")
        .gains;
    let unit = 1u32 << 16;
    let (frame, gain) = gains
        .iter()
        .copied()
        .enumerate()
        .max_by_key(|&(_, g)| g.abs_diff(unit))
        .unwrap();
    assert_ne!(gain, unit, "the fixture's frames differ in brightness");

    let out = cwd.join("frame.tif");
    let result = run_cli_in(
        &cwd,
        &[
            "export",
            project_path.to_str().unwrap(),
            "--frame",
            &frame.to_string(),
            "--frame-output",
            out.to_str().unwrap(),
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);

    let source = project.source_path(&project.manifest().sources[frame]);
    let raw = TiffStack::open_files(std::slice::from_ref(&source)).expect("open the source frame");
    let exported = TiffStack::open_files(std::slice::from_ref(&out)).expect("open the export");
    let (width, height) = raw.dimensions();
    let whole = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let want: Vec<u16> = raw
        .read_tile(0, whole)
        .expect("read the source")
        .samples
        .iter()
        .map(|&s| ((u64::from(s) * u64::from(gain) + 32_768) >> 16).min(65_535) as u16)
        .collect();
    let got = exported
        .read_tile(0, whole)
        .expect("read the export")
        .samples;
    assert!(
        got == want,
        "frame {frame} must be its source times gain {gain}/65536"
    );
}

#[test]
fn equalized_export_is_the_stored_composite() {
    let cwd = common::tmp_dir("export_cli_equalized_image");
    let out_dir = cwd.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let input = common::fixtures_dir().join("mantis-512");
    let stacked = run_cli_in(
        &cwd,
        &common::push_no_gpu(vec![
            "stack",
            input.to_str().unwrap(),
            "-o",
            out_dir.to_str().unwrap(),
            "--algorithm",
            "weighted-average",
            "--no-align",
            "--no-crop",
            "--set",
            "equalize=true",
        ]),
    );
    assert!(stacked.success, "stderr: {}", stacked.stderr);
    let project_path = only_project(&out_dir);
    let mut project = Project::open(&project_path).expect("open project");
    assert!(
        project
            .manifest()
            .registration
            .equalization
            .as_ref()
            .expect("the project records the equalization")
            .gains
            .iter()
            .any(|&g| g != 1 << 16),
        "the fixture's frames differ in brightness"
    );

    let out = cwd.join("image.tif");
    let result = run_cli_in(
        &cwd,
        &[
            "export",
            project_path.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ],
    );
    assert!(result.success, "stderr: {}", result.stderr);

    let (width, height, stored) = stored_composite(&mut project, &cwd);
    let exported = TiffStack::open_files(std::slice::from_ref(&out)).unwrap();
    let got = exported
        .read_tile(0, branched_full(width, height))
        .unwrap()
        .samples;
    assert!(
        got == stored,
        "the exported pixels must be the stored composite, which already carries the gains"
    );
}
