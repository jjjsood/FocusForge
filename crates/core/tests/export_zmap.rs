// SPDX-License-Identifier: GPL-3.0-or-later

//! `project::export_zmap`: the live export entry over a Z-Map file plus its
//! placement, checked against `export_checkpoint` of the same state and
//! against a direct `recomposite` of an edited Z-Map.

mod common;

use std::path::{Path, PathBuf};

use focusforge_core::align::{Registration, Transform, WarpingReader};
use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter};
use focusforge_core::params::ParamValues;
use focusforge_core::project::export::{ExportOptions, MetadataOptions, TiffBits, TiffCompression};
use focusforge_core::project::{
    self, AlignRecord, Metrics, Project, SealRequest, ZmapSource, export_checkpoint, export_zmap,
};
use focusforge_core::retouch::{Sources, recomposite};
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};

const FRAME_W: u32 = 10;
const FRAME_H: u32 = 8;
const SPP: u32 = 1;
const CROP: Rect = Rect {
    x: 2,
    y: 1,
    width: 6,
    height: 5,
};

fn tmp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn full(width: u32, height: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

fn write_frame(path: &Path, width: u32, height: u32, variant: u16) -> Vec<u16> {
    let samples: Vec<u16> = (0..(width * height) as usize)
        .map(|i| variant * 1000 + i as u16)
        .collect();
    let mut writer = TiffWriter::create(
        path,
        TiffSpec {
            width,
            height,
            samples_per_pixel: SPP,
            description: None,
        },
    )
    .unwrap();
    writer.write_tile(full(width, height), &samples).unwrap();
    writer.finish().unwrap();
    samples
}

/// A one-checkpoint project: `frame_count` plain source frames, `crop` (or
/// no crop when `None`) and identity alignment, a uniform Z-Map cycling
/// through the sources.
struct Fixture {
    project_path: PathBuf,
    source_paths: Vec<PathBuf>,
    checkpoint: String,
    registration: Registration,
}

fn seal_fixture(name: &str, frame_count: usize, crop: Option<Rect>) -> Fixture {
    seal_fixture_ex(
        name,
        frame_count,
        crop,
        Some(AlignRecord {
            algorithm: "identity".to_string(),
            params: ParamValues::new(),
            reference: 0,
        }),
        vec![Transform::identity(); frame_count],
    )
}

/// [`seal_fixture`], with the align record and per-frame transforms as the
/// caller chooses -- for a project sealed `--no-align` (`align: None`) or
/// with a real (non-identity) registration.
fn seal_fixture_ex(
    name: &str,
    frame_count: usize,
    crop: Option<Rect>,
    align: Option<AlignRecord>,
    transforms: Vec<Transform>,
) -> Fixture {
    let dir = tmp_dir(name);
    let source_paths: Vec<PathBuf> = (0..frame_count)
        .map(|i| {
            let path = dir.join(format!("frame-{i:02}.tif"));
            write_frame(&path, FRAME_W, FRAME_H, i as u16);
            path
        })
        .collect();

    let (out_width, out_height) = crop.map_or((FRAME_W, FRAME_H), |r| (r.width, r.height));
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let composite_path = staging.join("composite.tif");
    let mut writer = TiffWriter::create(
        &composite_path,
        TiffSpec {
            width: out_width,
            height: out_height,
            samples_per_pixel: SPP,
            description: Some("staged composite".to_string()),
        },
    )
    .unwrap();
    writer
        .write_tile(
            full(out_width, out_height),
            &vec![9000u16; (out_width * out_height) as usize],
        )
        .unwrap();
    writer.finish().unwrap();

    let zmap_path = staging.join("zmap.tif");
    let zmap: Vec<u16> = (0..(out_width * out_height) as usize)
        .map(|i| (i % frame_count) as u16)
        .collect();
    let mut map = ZMapWriter::create(
        &zmap_path,
        ZMapInfo::new(out_width, out_height, frame_count as u32).unwrap(),
    )
    .unwrap();
    map.write_tile(full(out_width, out_height), &zmap).unwrap();
    map.finish().unwrap();

    let registration = Registration {
        transforms,
        crop,
        equalization: None,
    };
    let project_path = dir.join(format!("fixture.{}", project::PROJECT_EXTENSION));
    let sealed = project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite_path,
        staged_zmap: &zmap_path,
        sources: &source_paths,
        algorithm: "weighted_average",
        params: &ParamValues::new(),
        align,
        registration: registration.clone(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");

    Fixture {
        project_path,
        source_paths,
        checkpoint: sealed.checkpoint,
        registration,
    }
}

fn zmap_source(fixture: &Fixture) -> ZmapSource {
    ZmapSource {
        sources: fixture.source_paths.clone(),
        registration: fixture.registration.clone(),
        reference: 0,
        description: "staged composite".to_string(),
    }
}

/// Materializes `fixture`'s own checkpoint Z-Map to `dest`.
fn materialize(fixture: &Fixture, dest: &Path) {
    let mut project = Project::open(&fixture.project_path).unwrap();
    let checkpoint = project.history().get(&fixture.checkpoint).unwrap().clone();
    project::materialize_zmap(&mut project, &checkpoint, dest).unwrap();
}

fn assert_export_zmap_matches_export_checkpoint(
    fixture: &Fixture,
    options: &ExportOptions,
    metadata_options: &MetadataOptions,
) {
    let dir = fixture.project_path.parent().unwrap();
    let checkpoint_zmap = dir.join("checkpoint-zmap.tif");
    materialize(fixture, &checkpoint_zmap);

    let via_zmap = dir.join("via-zmap.out");
    export_zmap(
        &checkpoint_zmap,
        &zmap_source(fixture),
        &via_zmap,
        options,
        metadata_options,
    )
    .expect("export_zmap must succeed");

    let via_checkpoint = dir.join("via-checkpoint.out");
    let mut project = Project::open(&fixture.project_path).unwrap();
    export_checkpoint(
        &mut project,
        &fixture.checkpoint,
        &via_checkpoint,
        options,
        metadata_options,
    )
    .expect("export_checkpoint must succeed");

    let a = std::fs::read(&via_zmap).unwrap();
    let b = std::fs::read(&via_checkpoint).unwrap();
    assert_eq!(
        a, b,
        "export_zmap over the checkpoint's own materialized Z-Map must be byte-identical to export_checkpoint"
    );
}

#[test]
fn matches_export_checkpoint_for_the_default_tiff_uncropped() {
    let fixture = seal_fixture("export_zmap_default_uncropped", 3, None);
    assert_export_zmap_matches_export_checkpoint(
        &fixture,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    );
}

#[test]
fn matches_export_checkpoint_for_the_default_tiff_aligned_and_cropped() {
    let fixture = seal_fixture("export_zmap_default_cropped", 3, Some(CROP));
    assert_export_zmap_matches_export_checkpoint(
        &fixture,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    );
}

#[test]
fn matches_export_checkpoint_for_a_real_non_identity_transform() {
    // Real per-frame translations, not every fixture's own identity default
    // -- both paths must still agree byte for byte once `warp` actually has
    // work to do.
    let fixture = seal_fixture_ex(
        "export_zmap_non_identity",
        3,
        Some(CROP),
        Some(AlignRecord {
            algorithm: "identity".to_string(),
            params: ParamValues::new(),
            reference: 0,
        }),
        vec![
            Transform::identity(),
            Transform::translation(1.0, 0.0),
            Transform::translation(-1.0, 1.0),
        ],
    );
    assert_export_zmap_matches_export_checkpoint(
        &fixture,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    );
}

#[test]
fn matches_export_checkpoint_with_no_alignment_and_no_crop() {
    // `--no-align --no-crop`: no `AlignRecord` at all, identity transforms,
    // no crop rect.
    let fixture = seal_fixture_ex(
        "export_zmap_no_align_no_crop",
        3,
        None,
        None,
        vec![Transform::identity(); 3],
    );
    assert_export_zmap_matches_export_checkpoint(
        &fixture,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    );
}

#[test]
fn matches_export_checkpoint_for_a_non_default_option_set() {
    let fixture = seal_fixture("export_zmap_non_default", 3, Some(CROP));
    let options = ExportOptions::Tiff {
        bits: TiffBits::Eight,
        compression: TiffCompression::Lzw,
    };
    assert_export_zmap_matches_export_checkpoint(&fixture, &options, &MetadataOptions::default());
}

#[test]
fn over_an_edited_zmap_its_samples_equal_recomposite_of_those_indices() {
    let fixture = seal_fixture("export_zmap_edited", 3, Some(CROP));
    let dir = fixture.project_path.parent().unwrap();

    // An edited Z-Map: not the checkpoint's own cycling pattern, so the
    // exported samples can only match if `export_zmap` really gathered
    // these indices, not the checkpoint's.
    let (width, height) = (CROP.width, CROP.height);
    let indices: Vec<u16> = (0..(width * height) as usize)
        .map(|i| ((i + 1) % 3) as u16)
        .collect();
    let edited_zmap = dir.join("edited-zmap.tif");
    let mut writer =
        ZMapWriter::create(&edited_zmap, ZMapInfo::new(width, height, 3).unwrap()).unwrap();
    writer.write_tile(full(width, height), &indices).unwrap();
    writer.finish().unwrap();

    let out_path = dir.join("edited-export.tif");
    export_zmap(
        &edited_zmap,
        &zmap_source(&fixture),
        &out_path,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .expect("export_zmap must succeed over an edited Z-Map");

    // Independent oracle: recomposite the same indices over the same
    // sources through the public gather API directly.
    let stack = TiffStack::open_files(&fixture.source_paths).unwrap();
    let warping = WarpingReader::new(
        &stack,
        (FRAME_W, FRAME_H),
        SPP,
        fixture.registration.transforms(fixture.source_paths.len()),
    );
    let sources = Sources {
        reader: &warping,
        frame_count: fixture.source_paths.len() as u32,
        samples_per_pixel: SPP,
        origin: fixture.registration.origin(),
    };
    let mut expected = vec![0u16; indices.len()];
    recomposite(&indices, full(width, height), &sources, &mut expected).unwrap();

    let out_stack = TiffStack::open_files(&[out_path]).unwrap();
    let out_tile = out_stack.read_tile(0, full(width, height)).unwrap();
    assert_eq!(out_tile.samples, expected);
}

#[test]
fn a_missing_source_leaves_no_file_and_no_partial() {
    let fixture = seal_fixture("export_zmap_missing_source", 2, None);
    let dir = fixture.project_path.parent().unwrap();
    let checkpoint_zmap = dir.join("checkpoint-zmap.tif");
    materialize(&fixture, &checkpoint_zmap);

    std::fs::remove_file(&fixture.source_paths[0]).unwrap();

    let out_path = dir.join("refused.tif");
    let err = export_zmap(
        &checkpoint_zmap,
        &zmap_source(&fixture),
        &out_path,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .expect_err("a missing source must refuse the export");
    assert!(!err.to_string().is_empty());
    assert!(!out_path.exists(), "no output file must be left behind");
    assert!(
        !dir.join("refused.tif.partial").exists(),
        "no .partial file must be left behind"
    );
}

#[test]
fn an_unwritable_destination_leaves_no_file_and_no_partial() {
    let fixture = seal_fixture("export_zmap_unwritable", 2, None);
    let dir = fixture.project_path.parent().unwrap();
    let checkpoint_zmap = dir.join("checkpoint-zmap.tif");
    materialize(&fixture, &checkpoint_zmap);

    let out_path = dir.join("no-such-directory").join("out.tif");
    let err = export_zmap(
        &checkpoint_zmap,
        &zmap_source(&fixture),
        &out_path,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .expect_err("an unwritable destination must refuse the export");
    assert!(!err.to_string().is_empty());
    assert!(!out_path.exists());
}
