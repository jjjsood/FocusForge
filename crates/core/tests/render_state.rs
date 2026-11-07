// SPDX-License-Identifier: GPL-3.0-or-later

//! The one render: a saved unedited checkpoint, a saved op-bearing one and a
//! held result each render any tile, and view and export write the same
//! samples. An unedited state needs no source frame.

mod common;

use std::path::{Path, PathBuf};

use focusforge_core::align::{Registration, Transform};
use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter, frame_paths};
use focusforge_core::params::ParamValues;
use focusforge_core::project::export::{ExportOptions, MetadataOptions};
use focusforge_core::project::{
    self, BlobKind, CheckpointImage, HeldCheckpoint, InMemoryCheckpoint, InMemoryKind, Metrics, Op,
    Plane, Point, Project, ProjectError, SealRequest,
};
use focusforge_core::retouch::blend;
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};

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

/// `path`'s first image: its width, height, samples per pixel and samples.
fn read_tiff(path: &Path) -> (u32, u32, u32, Vec<u16>) {
    let stack = TiffStack::open_files(&[path.to_path_buf()]).unwrap();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let mut samples = vec![0u16; (width * height * spp) as usize];
    stack
        .read_tile_into(0, full(width, height), &mut samples)
        .unwrap();
    (width, height, spp, samples)
}

fn write_tiff(path: &Path, width: u32, height: u32, spp: u32, sample: impl Fn(usize) -> u16) {
    let samples: Vec<u16> = (0..(width * height * spp) as usize).map(sample).collect();
    let mut writer = TiffWriter::create(
        path,
        TiffSpec {
            width,
            height,
            samples_per_pixel: spp,
            description: Some("staged composite".to_string()),
        },
    )
    .unwrap();
    writer.write_tile(full(width, height), &samples).unwrap();
    writer.finish().unwrap();
}

fn fixture_frames() -> Vec<PathBuf> {
    frame_paths(&common::fixtures_dir().join("mantis-64").join("frames")).unwrap()
}

fn seal_held(project_path: &Path, held: &HeldCheckpoint) -> String {
    let entry = InMemoryCheckpoint {
        kind: InMemoryKind::Fused(held.clone()),
        parent: None,
    };
    project::seal_in_memory(project_path, &[entry], &[], None, false)
        .expect("the held result must seal")
        .remove(0)
}

#[test]
fn unedited_export_matches_method_output() {
    let frames = fixture_frames();
    for method in ["laplacian", "weighted-average", "depth-map"] {
        for crop in [false, true] {
            let label = format!("{method} crop={crop}");
            let dir = tmp_dir(&format!(
                "render_state_method_{method}_{}",
                if crop { "cropped" } else { "uncropped" }
            ));
            let held = common::fuse_held(&frames, method, crop, &dir);
            let (_, _, _, expected) = read_tiff(&held.composite);

            let held_out = dir.join("held.tif");
            project::export_held(
                &held,
                &held_out,
                &ExportOptions::default(),
                &MetadataOptions::default(),
            )
            .unwrap_or_else(|err| panic!("{label}: held export: {err}"));
            assert_eq!(
                read_tiff(&held_out).3,
                expected,
                "{label}: held export differs from the fusion's own output"
            );

            let project_path = dir.join(format!("saved.{}", project::PROJECT_EXTENSION));
            let id = seal_held(&project_path, &held);
            let mut project = Project::open(&project_path).unwrap();
            let saved_out = dir.join("saved.tif");
            project::export_checkpoint(
                &mut project,
                &id,
                &saved_out,
                &ExportOptions::default(),
                &MetadataOptions::default(),
            )
            .unwrap_or_else(|err| panic!("{label}: saved export: {err}"));
            assert_eq!(
                read_tiff(&saved_out).3,
                expected,
                "{label}: saved export differs from the fusion's own output"
            );
        }
    }
}

const WIDTH: u32 = 700;
/// Taller than two 4 MiB read bands at `WIDTH` x 3 samples, so a plane
/// arrives in three bands.
const HEIGHT: u32 = 2100;
const SPP: u32 = 3;
const BAND_ROWS: u32 = (4 << 20) / (WIDTH * SPP * 2);
const _: () = assert!(HEIGHT > 2 * BAND_ROWS);

/// A saved root, a saved op-bearing child whose stroke crosses a band
/// boundary, and the root's composite as a held result.
struct Built {
    project_path: PathBuf,
    root: String,
    edited: String,
    held: HeldCheckpoint,
}

fn build(name: &str) -> Built {
    let dir = tmp_dir(name);
    let sources: Vec<PathBuf> = (0..2u32)
        .map(|i| {
            let path = dir.join(format!("frame-{i}.tif"));
            write_tiff(&path, WIDTH, HEIGHT, SPP, |s| {
                (s as u32 * 7 + i * 30_011) as u16
            });
            path
        })
        .collect();
    let composite = dir.join("composite.tif");
    write_tiff(&composite, WIDTH, HEIGHT, SPP, |s| (s % 65_521) as u16);
    let zmap = dir.join("zmap.tif");
    let mut map = ZMapWriter::create(&zmap, ZMapInfo::new(WIDTH, HEIGHT, 2).unwrap()).unwrap();
    map.write_tile(full(WIDTH, HEIGHT), &vec![0u16; (WIDTH * HEIGHT) as usize])
        .unwrap();
    map.finish().unwrap();

    let project_path = dir.join(format!("big.{}", project::PROJECT_EXTENSION));
    let sealed = project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources: &sources,
        algorithm: "weighted_average",
        params: &ParamValues::new(),
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");
    let edited = project::append_ops_onto(
        &project_path,
        &sealed.checkpoint,
        vec![Op::BrushStroke {
            source_id: 1,
            path: vec![
                Point {
                    x: 350,
                    y: BAND_ROWS as i32 - 2,
                },
                Point {
                    x: 350,
                    y: BAND_ROWS as i32 + 2,
                },
            ],
            radius: 40.0,
            softness: 0.5,
        }],
        None,
        1,
    )
    .expect("the stroke must append")
    .checkpoint;
    let held = HeldCheckpoint {
        composite,
        zmap,
        width: WIDTH,
        height: HEIGHT,
        sources,
        algorithm: "weighted_average".to_string(),
        params: ParamValues::new(),
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        metrics: Metrics::default(),
        created_at: 0,
        compressed_composite: None,
    };
    Built {
        project_path,
        root: sealed.checkpoint,
        edited,
        held,
    }
}

/// `image` rendered over `tile`, laid out as the tile's own rows.
fn render_tile(image: &CheckpointImage, project: Option<&mut Project>, tile: Rect) -> Vec<u16> {
    let spp = image.samples_per_pixel as usize;
    let row = tile.width as usize * spp;
    let mut out = vec![0u16; row * tile.height as usize];
    image
        .render(project, tile, &mut |band, rect| {
            assert_eq!(
                tile.intersect(rect),
                Some(rect),
                "a band lies inside the tile"
            );
            let len = rect.width as usize * spp;
            for r in 0..rect.height as usize {
                let dst =
                    (rect.y - tile.y) as usize * row + r * row + (rect.x - tile.x) as usize * spp;
                out[dst..dst + len].copy_from_slice(&band[r * len..(r + 1) * len]);
            }
            Ok(())
        })
        .expect("the tile must render");
    out
}

fn cut(full_image: &[u16], tile: Rect) -> Vec<u16> {
    let spp = SPP as usize;
    let mut out = Vec::new();
    for y in tile.y..tile.y + tile.height {
        let start = (y as usize * WIDTH as usize + tile.x as usize) * spp;
        out.extend_from_slice(&full_image[start..start + tile.width as usize * spp]);
    }
    out
}

fn tiles() -> Vec<Rect> {
    let rect = |x, y, width, height| Rect {
        x,
        y,
        width,
        height,
    };
    vec![
        rect(0, 0, WIDTH, BAND_ROWS),
        rect(0, BAND_ROWS, WIDTH, BAND_ROWS),
        rect(13, 5, 101, 50),
        rect(100, BAND_ROWS - 10, 300, 20),
        rect(0, 2 * BAND_ROWS, WIDTH, HEIGHT - 2 * BAND_ROWS),
        rect(WIDTH - 100, HEIGHT - 100, 100, 100),
        rect(WIDTH - 1, 0, 1, HEIGHT),
        rect(0, HEIGHT - 1, WIDTH, 1),
    ]
}

#[test]
fn tile_render_matches_full_render() {
    let built = build("render_state_tiles");
    let mut project = Project::open(&built.project_path).unwrap();
    let held_image = CheckpointImage::prepare_held(&built.held).unwrap();
    let held_full = render_tile(&held_image, None, held_image.full());
    let mut fulls = Vec::new();
    for (label, id) in [("fused", &built.root), ("op-bearing", &built.edited)] {
        let image = CheckpointImage::prepare(&project, id).unwrap();
        assert_eq!(image.full(), full(WIDTH, HEIGHT));
        let whole = render_tile(&image, Some(&mut project), image.full());
        for tile in tiles() {
            assert_eq!(
                render_tile(&image, Some(&mut project), tile),
                cut(&whole, tile),
                "{label}: tile {tile:?}"
            );
        }
        fulls.push(whole);
    }
    assert_ne!(fulls[0], fulls[1], "the stroke must change the image");
    for tile in tiles() {
        assert_eq!(
            render_tile(&held_image, None, tile),
            cut(&held_full, tile),
            "held: tile {tile:?}"
        );
    }
}

#[test]
fn view_and_export_are_pixel_identical() {
    let built = build("render_state_view_export");
    let dir = built.project_path.parent().unwrap().to_path_buf();
    let mut project = Project::open(&built.project_path).unwrap();
    for (label, id) in [("fused", &built.root), ("op-bearing", &built.edited)] {
        let view = dir.join(format!("{label}-view.tif"));
        let export = dir.join(format!("{label}-export.tif"));
        project::render_checkpoint(&mut project, id, &view).unwrap();
        project::export_checkpoint(
            &mut project,
            id,
            &export,
            &ExportOptions::default(),
            &MetadataOptions::default(),
        )
        .unwrap();
        assert_eq!(read_tiff(&view), read_tiff(&export), "{label}");
    }
    let stored = read_tiff(&built.held.composite);
    let (_, _, _, root_view) = read_tiff(&dir.join("fused-view.tif"));
    assert_eq!(
        root_view, stored.3,
        "an unedited view is the stored composite"
    );
}

#[test]
fn unedited_export_needs_no_sources() {
    let dir = tmp_dir("render_state_no_sources");
    let frames_dir = dir.join("frames");
    std::fs::create_dir_all(&frames_dir).unwrap();
    let frames: Vec<PathBuf> = fixture_frames()
        .iter()
        .map(|frame| {
            let copy = frames_dir.join(frame.file_name().unwrap());
            std::fs::copy(frame, &copy).unwrap();
            copy
        })
        .collect();
    let held = common::fuse_held(&frames, "weighted-average", true, &dir);
    let project_path = dir.join(format!("saved.{}", project::PROJECT_EXTENSION));
    let id = seal_held(&project_path, &held);
    let (_, _, _, expected) = read_tiff(&held.composite);

    std::fs::rename(&frames_dir, dir.join("frames-gone")).unwrap();
    let reference = held.sources[held.align.as_ref().map_or(0, |a| a.reference)].clone();

    let held_out = dir.join("held.tif");
    let outcome = project::export_held(
        &held,
        &held_out,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .expect("a held export reads no source frame");
    assert!(
        outcome
            .warnings
            .iter()
            .any(|w| w.contains(&reference.display().to_string())),
        "the held export names the missing reference frame: {:?}",
        outcome.warnings
    );
    assert_eq!(read_tiff(&held_out).3, expected);

    let mut project = Project::open(&project_path).unwrap();
    let saved_out = dir.join("saved.tif");
    let outcome = project::export_checkpoint(
        &mut project,
        &id,
        &saved_out,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .expect("a saved unedited export reads no source frame");
    assert!(
        outcome
            .warnings
            .iter()
            .any(|w| w.contains(&reference.display().to_string())),
        "the saved export names the missing reference frame: {:?}",
        outcome.warnings
    );
    assert_eq!(read_tiff(&saved_out).3, expected);
}

const PENDING_FRAME_W: u32 = 10;
const PENDING_FRAME_H: u32 = 8;
const PENDING_CROP: Rect = Rect {
    x: 2,
    y: 1,
    width: 6,
    height: 5,
};

fn pending_stroke(source_id: u16, path: &[(i32, i32)], radius: f32, softness: f32) -> Op {
    Op::BrushStroke {
        source_id,
        path: path.iter().map(|&(x, y)| Point { x, y }).collect(),
        radius,
        softness,
    }
}

#[test]
fn pending_state_export_equals_the_saved_export() {
    let dir = tmp_dir("render_state_pending");
    let sources: Vec<PathBuf> = (0..3u32)
        .map(|i| {
            let path = dir.join(format!("frame-{i}.tif"));
            write_tiff(&path, PENDING_FRAME_W, PENDING_FRAME_H, 1, |s| {
                (s as u32 + i * 1000) as u16
            });
            path
        })
        .collect();
    let composite = dir.join("composite.tif");
    write_tiff(
        &composite,
        PENDING_CROP.width,
        PENDING_CROP.height,
        1,
        |s| 500 + s as u16,
    );
    let zmap = dir.join("zmap.tif");
    let mut map = ZMapWriter::create(
        &zmap,
        ZMapInfo::new(PENDING_CROP.width, PENDING_CROP.height, 3).unwrap(),
    )
    .unwrap();
    map.write_tile(
        full(PENDING_CROP.width, PENDING_CROP.height),
        &vec![0u16; (PENDING_CROP.width * PENDING_CROP.height) as usize],
    )
    .unwrap();
    map.finish().unwrap();

    let project_path = dir.join(format!("pending.{}", project::PROJECT_EXTENSION));
    let registration = Registration {
        transforms: vec![
            Transform::identity(),
            Transform::translation(1.0, 0.0),
            Transform::translation(-1.0, 1.0),
        ],
        crop: Some(PENDING_CROP),
        equalization: None,
    };
    let root = project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources: &sources,
        algorithm: "weighted_average",
        params: &ParamValues::new(),
        align: Some(project::AlignRecord {
            algorithm: "identity".to_string(),
            params: ParamValues::new(),
            reference: 0,
        }),
        registration,
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed")
    .checkpoint;
    let saved = project::append_ops_onto(
        &project_path,
        &root,
        vec![pending_stroke(1, &[(0, 0), (2, 1)], 2.0, 0.5)],
        None,
        1,
    )
    .expect("the saved stroke must append")
    .checkpoint;

    let mut project = Project::open(&project_path).unwrap();
    let record = project.fuse_state(&saved).unwrap();
    let base = dir.join("base.tif");
    project::render_checkpoint(&mut project, &saved, &base).unwrap();
    let pending_ops = vec![
        pending_stroke(2, &[(1, 1), (4, 3)], 1.5, 0.5),
        pending_stroke(0, &[(3, 0), (3, 4)], 2.0, 0.0),
    ];
    let state = project::PendingState {
        composite: base.clone(),
        ops: pending_ops.clone(),
        sources: sources.clone(),
        registration: record.registration.clone(),
        reference: 0,
        description: "staged composite".to_string(),
    };

    let ffp_before = std::fs::read(&project_path).unwrap();
    let pending_out = dir.join("pending.tif");
    project::export_pending(
        &state,
        &pending_out,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .expect("the pending state must export");
    assert_eq!(
        std::fs::read(&project_path).unwrap(),
        ffp_before,
        "exporting a pending state writes nothing into the project"
    );

    let appended = project::append_ops_onto(&project_path, &saved, pending_ops, None, 2)
        .expect("the pending strokes must append")
        .checkpoint;
    let mut project = Project::open(&project_path).unwrap();
    let saved_out = dir.join("saved.tif");
    project::export_checkpoint(
        &mut project,
        &appended,
        &saved_out,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(&pending_out).unwrap(),
        std::fs::read(&saved_out).unwrap(),
        "the pending export is the saved export"
    );
    assert_ne!(
        read_tiff(&pending_out).3,
        read_tiff(&base).3,
        "the pending strokes must change the image"
    );

    std::fs::remove_file(&sources[1]).unwrap();
    let refused = dir.join("refused.tif");
    let err = project::export_pending(
        &state,
        &refused,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .expect_err("a missing source must refuse the export");
    let ProjectError::ExportSourcesUnavailable { detail, .. } = err else {
        panic!("expected ExportSourcesUnavailable, got {err}");
    };
    assert_eq!(
        detail,
        format!("missing: {}", sources[1].display()),
        "one line per missing source"
    );
    assert!(!refused.exists(), "a refused export leaves no file");
}

/// `image` rendered in `f32` over `tile`, laid out as the tile's own rows.
fn render_tile_f32(image: &CheckpointImage, project: Option<&mut Project>, tile: Rect) -> Vec<f32> {
    let spp = image.samples_per_pixel as usize;
    let row = tile.width as usize * spp;
    let mut out = vec![0f32; row * tile.height as usize];
    image
        .render_f32(project, tile, &mut |band, rect| {
            let len = rect.width as usize * spp;
            for r in 0..rect.height as usize {
                let dst =
                    (rect.y - tile.y) as usize * row + r * row + (rect.x - tile.x) as usize * spp;
                out[dst..dst + len].copy_from_slice(&band[r * len..(r + 1) * len]);
            }
            Ok(())
        })
        .expect("the tile must render in f32");
    out
}

fn cut_f32(plane: &[f32], width: u32, spp: u32, tile: Rect) -> Vec<f32> {
    let mut out = Vec::new();
    for y in tile.y..tile.y + tile.height {
        let start = ((y * width + tile.x) * spp) as usize;
        out.extend_from_slice(&plane[start..start + (tile.width * spp) as usize]);
    }
    out
}

fn bits(samples: &[f32]) -> Vec<u32> {
    samples.iter().map(|s| s.to_bits()).collect()
}

fn narrowed(samples: &[f32]) -> Vec<u16> {
    samples
        .iter()
        .map(|&s| (s.clamp(0.0, 65_535.0) + 0.5).floor() as u16)
        .collect()
}

#[test]
fn f32_held_saved_and_pending_render_agree() {
    let (w, h, spp) = (37u32, 23u32, 3u32);
    let dir = tmp_dir("render_state_f32");
    let sources: Vec<PathBuf> = (0..2u32)
        .map(|i| {
            let path = dir.join(format!("frame-{i}.tif"));
            write_tiff(&path, w, h, spp, |s| {
                (1_000 + (s as u32 * 3 + i * 500) % 4_000) as u16
            });
            path
        })
        .collect();

    let staged: Vec<f32> = (0..(w * h * spp) as usize)
        .map(|i| match i % 11 {
            7 => -250.5 - i as f32,
            3 => i as f32 * 100.25,
            _ => 90_000.0 + (i % 13) as f32 * 1.5,
        })
        .collect();
    assert!(staged.iter().any(|&s| s > 65_535.0), "a sample above white");
    assert!(staged.iter().any(|&s| s < 0.0), "a sample below black");
    let composite = dir.join("composite.tif");
    let mut writer = TiffWriter::create_f32(
        &composite,
        TiffSpec {
            width: w,
            height: h,
            samples_per_pixel: spp,
            description: Some("staged composite".to_string()),
        },
    )
    .unwrap();
    writer.write_tile_f32(full(w, h), &staged).unwrap();
    writer.finish().unwrap();
    let zmap = dir.join("zmap.tif");
    let mut map = ZMapWriter::create(&zmap, ZMapInfo::new(w, h, 2).unwrap()).unwrap();
    map.write_tile(full(w, h), &vec![0u16; (w * h) as usize])
        .unwrap();
    map.finish().unwrap();

    let project_path = dir.join(format!("float.{}", project::PROJECT_EXTENSION));
    let root = project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources: &sources,
        algorithm: "weighted_average",
        params: &ParamValues::new(),
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("a float staged composite must seal")
    .checkpoint;
    let stroke = Op::BrushStroke {
        source_id: 1,
        path: vec![Point { x: 6, y: 11 }, Point { x: 30, y: 11 }],
        radius: 6.0,
        softness: 0.5,
    };
    let edited = project::append_ops_onto(&project_path, &root, vec![stroke.clone()], None, 1)
        .expect("the stroke must append")
        .checkpoint;

    let mut project = Project::open(&project_path).unwrap();
    let kind_of = |hash: &str| {
        project
            .manifest()
            .blobs
            .iter()
            .find(|blob| blob.hash == hash)
            .expect("the manifest lists the blob")
            .kind
    };
    assert_eq!(
        kind_of(&project.manifest().composite.blob),
        BlobKind::FullF32
    );
    assert_eq!(kind_of(&project.manifest().zmap.blob), BlobKind::Full);

    let held = HeldCheckpoint {
        composite: composite.clone(),
        zmap: zmap.clone(),
        width: w,
        height: h,
        sources: sources.clone(),
        algorithm: "weighted_average".to_string(),
        params: ParamValues::new(),
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        metrics: Metrics::default(),
        created_at: 0,
        compressed_composite: None,
    };
    let pending = project::PendingState {
        composite: composite.clone(),
        ops: Vec::new(),
        sources: sources.clone(),
        registration: Registration::default(),
        reference: 0,
        description: "staged composite".to_string(),
    };
    let pending_stroked = project::PendingState {
        ops: vec![stroke.clone()],
        ..pending.clone()
    };
    let held_image = CheckpointImage::prepare_held(&held).unwrap();
    let pending_image = CheckpointImage::prepare_pending(&pending).unwrap();
    let pending_stroked_image = CheckpointImage::prepare_pending(&pending_stroked).unwrap();
    let root_image = CheckpointImage::prepare(&project, &root).unwrap();
    let edited_image = CheckpointImage::prepare(&project, &edited).unwrap();

    let info = ZMapInfo::new(w, h, 2).unwrap();
    let edited_whole = render_tile_f32(&edited_image, Some(&mut project), full(w, h));
    let mut soft_edge = 0;
    for (i, (&after, &before)) in edited_whole.iter().zip(&staged).enumerate() {
        let pixel = i as u32 / spp;
        let alpha = blend::alpha(&stroke, info, pixel % w, pixel / w).unwrap();
        if alpha == 0 {
            assert_eq!(
                after.to_bits(),
                before.to_bits(),
                "sample {i} is outside the stroke"
            );
        } else if alpha < 65_536 && before > 65_535.0 && after > 65_535.0 && after < before {
            soft_edge += 1;
        }
    }
    assert!(
        soft_edge > 0,
        "a soft-edge sample keeps a value above white"
    );
    assert_ne!(
        bits(&edited_whole),
        bits(&staged),
        "the stroke must change the image"
    );

    for tile in [
        full(w, h),
        Rect {
            x: 5,
            y: 3,
            width: 20,
            height: 11,
        },
    ] {
        let expect = cut_f32(&staged, w, spp, tile);
        for (label, image, on_project) in [
            ("held", &held_image, false),
            ("pending", &pending_image, false),
            ("saved root", &root_image, true),
        ] {
            let got = render_tile_f32(image, on_project.then_some(&mut project), tile);
            assert_eq!(bits(&got), bits(&expect), "{label}: tile {tile:?}");
        }
        let expect = cut_f32(&edited_whole, w, spp, tile);
        let saved = render_tile_f32(&edited_image, Some(&mut project), tile);
        let unsaved = render_tile_f32(&pending_stroked_image, None, tile);
        assert_eq!(bits(&saved), bits(&expect), "saved edit: tile {tile:?}");
        assert_eq!(bits(&unsaved), bits(&expect), "pending edit: tile {tile:?}");
    }

    assert_eq!(
        render_tile(&root_image, Some(&mut project), full(w, h)),
        narrowed(&staged),
        "render narrows the f32 samples"
    );
    assert_eq!(
        render_tile(&edited_image, Some(&mut project), full(w, h)),
        narrowed(&edited_whole),
        "render narrows the retouched f32 samples"
    );
    let mut plane = Vec::new();
    project
        .read_plane_into(Plane::Composite, &mut |band, _| {
            plane.extend_from_slice(band)
        })
        .unwrap();
    assert_eq!(
        plane,
        narrowed(&staged),
        "read_plane_into narrows the f32 blob"
    );

    let sixteen = dir.join("sixteen.tif");
    write_tiff(&sixteen, w, h, spp, |s| s as u16);
    let held_sixteen = HeldCheckpoint {
        composite: sixteen,
        ..held
    };
    let image = CheckpointImage::prepare_held(&held_sixteen).unwrap();
    assert!(
        image
            .render_f32(None, full(w, h), &mut |_, _| Ok(()))
            .is_err(),
        "a 16-bit image is refused by render_f32"
    );
}
