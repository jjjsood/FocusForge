// SPDX-License-Identifier: GPL-3.0-or-later

//! A RAW stack fuses into an `f32` document: samples above white survive the
//! fusion, the save and the reopen, every render path reads the same samples,
//! and a RAW project saved with a clipped 16-bit result is refused.

mod common;
#[path = "common/raw_dng.rs"]
mod raw_dng;

use std::path::{Path, PathBuf};

use focusforge_core::align::Registration;
use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter};
use focusforge_core::params::ParamValues;
use focusforge_core::project::{
    self, BlobKind, CheckpointImage, HeldCheckpoint, InMemoryCheckpoint, InMemoryKind, Metrics, Op,
    PendingState, Point, Project, SealRequest,
};
use focusforge_core::tile::{Sample, SampleFormat};
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};
use raw_dng::{DngSpec, write_dng};

const AS_SHOT_NEUTRAL: [f32; 3] = [0.5, 1.0, 0.625];

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

/// `count` textured frames whose detail sits in a different vertical band per
/// frame, bright enough that the as-shot white balance pushes the red channel
/// past white.
fn dng_frames(count: usize, width: u32, height: u32) -> Vec<tempfile::NamedTempFile> {
    (0..count)
        .map(|frame| {
            let samples = (0..width * height)
                .map(|i| {
                    let (x, y) = (i % width, i / width);
                    let band = (x as usize * count / width as usize).min(count - 1);
                    let amplitude = if band == frame { 380 } else { 50 };
                    let checker = if (x / 2 + y / 2) % 2 == 0 {
                        0
                    } else {
                        amplitude
                    };
                    (2_700 + frame as u32 * 40 + checker) as u16
                })
                .collect();
            let mut spec = DngSpec::new(12, 64, 4095, [0, 1, 1, 2], samples);
            spec.width = width;
            spec.height = height;
            spec.as_shot_neutral = Some(AS_SHOT_NEUTRAL);
            write_dng(&spec, false)
        })
        .collect()
}

fn paths(frames: &[tempfile::NamedTempFile]) -> Vec<PathBuf> {
    frames.iter().map(|f| f.path().to_path_buf()).collect()
}

fn read_f32(path: &Path) -> Vec<f32> {
    let stack = TiffStack::open_staged(path).unwrap();
    assert_eq!(stack.sample_format(), SampleFormat::F32, "{path:?}");
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let mut samples = vec![0f32; (width * height * spp) as usize];
    stack
        .read_tile_f32_into(0, full(width, height), &mut samples)
        .unwrap();
    samples
}

fn bits(samples: &[f32]) -> Vec<u32> {
    samples.iter().map(|s| s.to_bits()).collect()
}

fn narrowed(samples: &[f32]) -> Vec<u16> {
    samples.iter().map(|&s| u16::narrow(s)).collect()
}

/// `image` rendered in `f32` over `tile`, laid out as the tile's own rows.
fn render_f32(image: &CheckpointImage, project: Option<&mut Project>, tile: Rect) -> Vec<f32> {
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

fn render_u16(image: &CheckpointImage, project: Option<&mut Project>, tile: Rect) -> Vec<u16> {
    let spp = image.samples_per_pixel as usize;
    let row = tile.width as usize * spp;
    let mut out = vec![0u16; row * tile.height as usize];
    image
        .render(project, tile, &mut |band, rect| {
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

fn cut_f32(plane: &[f32], width: u32, spp: u32, tile: Rect) -> Vec<f32> {
    let mut out = Vec::new();
    for y in tile.y..tile.y + tile.height {
        let start = ((y * width + tile.x) * spp) as usize;
        out.extend_from_slice(&plane[start..start + (tile.width * spp) as usize]);
    }
    out
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

fn blob_kind(project: &Project, hash: &str) -> BlobKind {
    project
        .manifest()
        .blobs
        .iter()
        .find(|blob| blob.hash == hash)
        .expect("the manifest lists the blob")
        .kind
}

#[test]
fn raw_document_keeps_headroom() {
    for (method, count, width, height) in [
        ("laplacian", 3, 64, 48),
        ("laplacian", 5, 64, 48),
        ("laplacian", 3, 37, 23),
        ("weighted-average", 3, 64, 48),
        ("weighted-average", 5, 64, 48),
        ("weighted-average", 3, 37, 23),
    ] {
        let label = format!("{method}, {count} frames, {width}x{height}");
        let frames = dng_frames(count, width, height);
        let dir = tmp_dir(&format!("raw_document_headroom_{method}_{count}_{width}"));
        let held = common::fuse_held(&paths(&frames), method, false, &dir);

        let staged = read_f32(&held.composite);
        assert!(
            staged.iter().any(|&s| s > 65_535.0),
            "{label}: a staged sample above white"
        );
        assert!(
            held.metrics.composite_lapvar.is_some(),
            "{label}: the metrics read the float composite"
        );

        let project_path = dir.join(format!("saved.{}", project::PROJECT_EXTENSION));
        let id = seal_held(&project_path, &held);
        let mut project = Project::open(&project_path).unwrap();
        assert_eq!(
            blob_kind(&project, &project.manifest().composite.blob),
            BlobKind::FullF32,
            "{label}"
        );
        assert_eq!(
            blob_kind(&project, &project.manifest().zmap.blob),
            BlobKind::Full,
            "{label}"
        );

        let image = CheckpointImage::prepare(&project, &id).unwrap();
        let tile = full(width, height);
        let wide = render_f32(&image, Some(&mut project), tile);
        assert_eq!(bits(&wide), bits(&staged), "{label}: saved f32 render");
        assert_eq!(
            render_u16(&image, Some(&mut project), tile),
            narrowed(&staged),
            "{label}: the 16-bit render narrows the f32 one"
        );
    }
}

#[test]
fn one_render_for_held_saved_and_pending() {
    let (width, height) = (37u32, 23u32);
    let frames = dng_frames(3, width, height);
    let sources = paths(&frames);
    let dir = tmp_dir("raw_document_one_render");
    let held = common::fuse_held(&sources, "weighted-average", false, &dir);
    let staged = read_f32(&held.composite);
    assert!(
        staged.iter().any(|&s| s > 65_535.0),
        "headroom precondition"
    );

    let project_path = dir.join(format!("saved.{}", project::PROJECT_EXTENSION));
    let root = seal_held(&project_path, &held);
    let stroke = Op::BrushStroke {
        source_id: 1,
        path: vec![Point { x: 4, y: 11 }, Point { x: 32, y: 11 }],
        radius: 40.0,
        softness: 0.5,
    };
    let edited = project::append_ops_onto(&project_path, &root, vec![stroke.clone()], None, 1)
        .expect("the stroke must append")
        .checkpoint;
    let mut project = Project::open(&project_path).unwrap();

    let pending = PendingState {
        composite: held.composite.clone(),
        ops: Vec::new(),
        sources: held.sources.clone(),
        registration: held.registration.clone(),
        reference: 0,
        description: held.composite_description.clone(),
    };
    let pending_stroked = PendingState {
        ops: vec![stroke],
        ..pending.clone()
    };
    let held_image = CheckpointImage::prepare_held(&held).unwrap();
    let pending_image = CheckpointImage::prepare_pending(&pending).unwrap();
    let pending_stroked_image = CheckpointImage::prepare_pending(&pending_stroked).unwrap();
    let root_image = CheckpointImage::prepare(&project, &root).unwrap();
    let edited_image = CheckpointImage::prepare(&project, &edited).unwrap();

    let edited_whole = render_f32(&edited_image, Some(&mut project), full(width, height));
    assert_ne!(bits(&edited_whole), bits(&staged), "the stroke changes it");
    assert!(
        edited_whole.iter().any(|&s| s > 65_535.0),
        "the stroke's samples stay unclamped"
    );

    for tile in [
        full(width, height),
        Rect {
            x: 5,
            y: 3,
            width: 20,
            height: 11,
        },
    ] {
        let expect = cut_f32(&staged, width, 3, tile);
        for (label, image, on_project) in [
            ("held", &held_image, false),
            ("pending", &pending_image, false),
            ("saved", &root_image, true),
        ] {
            let got = render_f32(image, on_project.then_some(&mut project), tile);
            assert_eq!(bits(&got), bits(&expect), "{label}: tile {tile:?}");
        }
        let expect = cut_f32(&edited_whole, width, 3, tile);
        let saved = render_f32(&edited_image, Some(&mut project), tile);
        let unsaved = render_f32(&pending_stroked_image, None, tile);
        assert_eq!(bits(&saved), bits(&expect), "saved edit: tile {tile:?}");
        assert_eq!(bits(&unsaved), bits(&expect), "pending edit: tile {tile:?}");
    }
}

/// Seals a project over `sources` whose composite is a `width` x `height` RGB
/// plane written as `f32` when `wide` and as `u16` otherwise.
fn seal_over(dir: &Path, sources: &[PathBuf], (width, height): (u32, u32), wide: bool) -> PathBuf {
    let spec = TiffSpec {
        width,
        height,
        samples_per_pixel: 3,
        description: Some("staged composite".to_string()),
    };
    let composite = dir.join("composite.tif");
    let len = (width * height * 3) as usize;
    if wide {
        let samples: Vec<f32> = (0..len).map(|i| 70_000.0 + i as f32).collect();
        let mut writer = TiffWriter::create_f32(&composite, spec).unwrap();
        writer
            .write_tile_f32(full(width, height), &samples)
            .unwrap();
        writer.finish().unwrap();
    } else {
        let samples: Vec<u16> = (0..len).map(|i| (i * 3 % 60_000) as u16).collect();
        let mut writer = TiffWriter::create(&composite, spec).unwrap();
        writer.write_tile(full(width, height), &samples).unwrap();
        writer.finish().unwrap();
    }
    let zmap = dir.join("zmap.tif");
    let info = ZMapInfo::new(width, height, sources.len() as u32).unwrap();
    let mut map = ZMapWriter::create(&zmap, info).unwrap();
    map.write_tile(full(width, height), &vec![0u16; (width * height) as usize])
        .unwrap();
    map.finish().unwrap();
    let project_path = dir.join(format!("seal.{}", project::PROJECT_EXTENSION));
    project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources,
        algorithm: "weighted_average",
        params: &ParamValues::new(),
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("the composite must seal");
    project_path
}

#[test]
fn legacy_raw_project_refused() {
    let (width, height) = (16u32, 12u32);
    let frames = dng_frames(2, width, height);
    let sources = paths(&frames);

    let legacy_dir = tmp_dir("raw_document_legacy");
    let legacy = seal_over(&legacy_dir, &sources, (width, height), false);
    let error = Project::open(&legacy)
        .err()
        .expect("a clipped RAW result is refused");
    let text = error.to_string();
    assert!(
        text.contains(legacy.to_str().unwrap()) && text.contains("stack its source frames again"),
        "{text}"
    );

    let wide_dir = tmp_dir("raw_document_wide");
    let wide = seal_over(&wide_dir, &sources, (width, height), true);
    Project::open(&wide).expect("a RAW project with a float result opens");

    let tiff_dir = tmp_dir("raw_document_tiff");
    let tiff_sources: Vec<PathBuf> = (0..2)
        .map(|i| {
            let path = tiff_dir.join(format!("frame-{i}.tif"));
            let mut writer = TiffWriter::create(
                &path,
                TiffSpec {
                    width,
                    height,
                    samples_per_pixel: 3,
                    description: None,
                },
            )
            .unwrap();
            writer
                .write_tile(full(width, height), &vec![1_000 * (i + 1) as u16; 576])
                .unwrap();
            writer.finish().unwrap();
            path
        })
        .collect();
    let tiff = seal_over(&tiff_dir, &tiff_sources, (width, height), false);
    Project::open(&tiff).expect("a TIFF project with a 16-bit result opens");
}
