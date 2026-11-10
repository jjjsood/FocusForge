// SPDX-License-Identifier: GPL-3.0-or-later

//! An edited checkpoint renders as its fused ancestor's composite with each
//! brush stroke blended over it in log order, from the stroke's frame read
//! through the gains and alignment the fusion recorded, and reads no source
//! frame for a tile no stroke meets.

mod common;
#[path = "common/raw_dng.rs"]
mod raw_dng;

use std::path::{Path, PathBuf};

use focusforge_core::algorithm::TileReader;
use focusforge_core::align::{Registration, WarpingReader};
use focusforge_core::equalize::{Equalization, GainReader};
use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter, frame_paths, open_frame_set};
use focusforge_core::params::ParamValues;
use focusforge_core::project::container::Index;
use focusforge_core::project::export::{ExportOptions, MetadataOptions};
use focusforge_core::project::{
    self, BrushMode, CheckpointImage, HeldCheckpoint, InMemoryCheckpoint, InMemoryKind, Metrics,
    Op, Point, Project, ProjectError, SealRequest, StrokeSource, partial_path,
};
use focusforge_core::retouch::blend::{self, Blend};
use focusforge_core::tile::{HostBudget, Sample};
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};

use raw_dng::{DngSpec, write_dng};

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

/// `path`'s first image: its width, height, samples per pixel and samples,
/// a float file narrowed to `u16`.
fn read_tiff(path: &Path) -> (u32, u32, u32, Vec<u16>) {
    let stack = TiffStack::open_staged(path).unwrap();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let mut samples = vec![0u16; (width * height * spp) as usize];
    stack
        .read_tile_into(0, full(width, height), &mut samples)
        .unwrap();
    (width, height, spp, samples)
}

fn write_tiff(path: &Path, width: u32, height: u32, spp: u32, samples: &[u16]) {
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
    writer.write_tile(full(width, height), samples).unwrap();
    writer.finish().unwrap();
}

fn stroke(source_id: u16, path: &[(i32, i32)], radius: f32, softness: f32) -> Op {
    Op::frame_stroke(
        source_id,
        path.iter().map(|&(x, y)| Point { x, y }).collect(),
        radius,
        softness,
    )
}

/// `image` rendered over `tile`, laid out as the tile's own rows.
fn try_render(
    image: &CheckpointImage,
    project: &mut Project,
    tile: Rect,
) -> Result<Vec<u16>, ProjectError> {
    let spp = image.samples_per_pixel as usize;
    let row = tile.width as usize * spp;
    let mut out = vec![0u16; row * tile.height as usize];
    image.render(Some(project), tile, &mut |band, rect| {
        assert_eq!(
            tile.intersect(rect),
            Some(rect),
            "a band lies inside the tile"
        );
        let len = rect.width as usize * spp;
        for r in 0..rect.height as usize {
            let dst = (rect.y - tile.y) as usize * row + r * row + (rect.x - tile.x) as usize * spp;
            out[dst..dst + len].copy_from_slice(&band[r * len..(r + 1) * len]);
        }
        Ok(())
    })?;
    Ok(out)
}

/// The whole image rendered as a grid of `size`-square tiles.
fn render_tiled(image: &CheckpointImage, project: &mut Project, size: u32) -> Vec<u16> {
    let spp = image.samples_per_pixel as usize;
    let mut whole = vec![0u16; (image.width * image.height) as usize * spp];
    for y in (0..image.height).step_by(size as usize) {
        for x in (0..image.width).step_by(size as usize) {
            let tile = Rect {
                x,
                y,
                width: size.min(image.width - x),
                height: size.min(image.height - y),
            };
            let part = try_render(image, project, tile).expect("the tile must render");
            let row = tile.width as usize * spp;
            for r in 0..tile.height as usize {
                let dst = ((tile.y as usize + r) * image.width as usize + tile.x as usize) * spp;
                whole[dst..dst + row].copy_from_slice(&part[r * row..(r + 1) * row]);
            }
        }
    }
    whole
}

/// What blending `ops` over `composite` gives, pixel by pixel: every stroke's
/// frame is `frames`' own read of the composite's extent shifted by `origin`.
fn reference_blend<S: Blend>(
    composite: &[S],
    (width, height, spp): (u32, u32, u32),
    frame_count: u32,
    ops: &[Op],
    frames: &dyn TileReader,
    origin: (u32, u32),
) -> Vec<S> {
    let info = ZMapInfo::new(width, height, frame_count).unwrap();
    let mut out = composite.to_vec();
    let mut frame = vec![S::default(); out.len()];
    for op in ops {
        let frame_index = blend::source_frame(op).unwrap();
        let at = Rect {
            x: origin.0,
            y: origin.1,
            width,
            height,
        };
        S::read_tile(frames, usize::from(frame_index), at, &mut frame).unwrap();
        for y in 0..height {
            for x in 0..width {
                let a = blend::alpha(op, info, x, y).unwrap();
                let at = ((y * width + x) * spp) as usize;
                for i in at..at + spp as usize {
                    out[i] = S::blend(out[i], frame[i], a);
                }
            }
        }
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

fn append(project_path: &Path, parent: &str, ops: Vec<Op>) -> String {
    project::append_ops_onto(project_path, parent, ops, None, 1)
        .expect("the strokes must append")
        .checkpoint
}

#[test]
fn blend_matches_reference_at_every_tile_size() {
    let dir = tmp_dir("retouch_ops_reference");
    let frames = frame_paths(&common::mantis_512_dir().join("frames")).unwrap();
    let held = common::fuse_held(&frames, "weighted-average", true, &dir);
    let project_path = dir.join(format!("mantis.{}", project::PROJECT_EXTENSION));
    let root = seal_held(&project_path, &held);

    let list = std::fs::read_to_string(common::fixtures_dir().join("retouch/stroke-list.json"))
        .expect("the stroke list fixture must read");
    let ops: Vec<Op> = serde_json::from_str(&list).expect("the stroke list must parse");
    assert_eq!(ops.len(), 3);
    let edited = append(&project_path, &root, ops.clone());

    let (width, height, spp, composite) = read_tiff(&held.composite);
    let set = open_frame_set(&held.sources, HostBudget::explicit(1 << 30)).unwrap();
    let gained = GainReader::new(&*set.reader, held.registration.gains());
    let warping = WarpingReader::new(
        &gained,
        set.reader.dimensions(),
        spp,
        held.registration.transforms(held.sources.len()),
    );
    let expected = reference_blend(
        &composite,
        (width, height, spp),
        held.sources.len() as u32,
        &ops,
        &warping,
        held.registration.origin(),
    );
    assert_ne!(expected, composite, "the strokes must change the image");

    let mut project = Project::open(&project_path).unwrap();
    let image = CheckpointImage::prepare(&project, &edited).unwrap();
    for size in [64, 100, 512] {
        let rendered = render_tiled(&image, &mut project, size);
        let differing = rendered
            .iter()
            .zip(&expected)
            .filter(|(r, e)| r != e)
            .count();
        assert_eq!(
            differing, 0,
            "tile size {size}: samples differ from the reference"
        );
    }
}

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
const SPP: u32 = 3;

fn composite_pattern(len: usize) -> Vec<u16> {
    (0..len).map(|s| (s * 3 % 65_521) as u16).collect()
}

/// [`composite_pattern`] as `f32`, reaching above white and below black.
fn composite_pattern_f32(len: usize) -> Vec<f32> {
    composite_pattern(len)
        .into_iter()
        .map(|v| f32::from(v) * 1.5 - 100.0)
        .collect()
}

/// A saved root over `sources` (each `WIDTH` x `HEIGHT` x `SPP`) whose
/// composite is [`composite_pattern`] -- as `f32` for RAW `sources` --
/// sealed with `registration`.
fn seal_synthetic(
    dir: &Path,
    sources: &[PathBuf],
    (width, height, spp): (u32, u32, u32),
    registration: Registration,
) -> (PathBuf, String) {
    let composite = dir.join("composite.tif");
    let len = (width * height * spp) as usize;
    if focusforge_core::io::has_extension(&sources[0], focusforge_core::io::RAW_EXTENSIONS) {
        let mut writer = TiffWriter::create_f32(
            &composite,
            TiffSpec {
                width,
                height,
                samples_per_pixel: spp,
                description: Some("staged composite".to_string()),
            },
        )
        .unwrap();
        writer
            .write_tile_f32(full(width, height), &composite_pattern_f32(len))
            .unwrap();
        writer.finish().unwrap();
    } else {
        write_tiff(&composite, width, height, spp, &composite_pattern(len));
    }
    let zmap = dir.join("zmap.tif");
    let info = ZMapInfo::new(width, height, sources.len() as u32).unwrap();
    let mut map = ZMapWriter::create(&zmap, info).unwrap();
    map.write_tile(full(width, height), &vec![0u16; (width * height) as usize])
        .unwrap();
    map.finish().unwrap();

    let project_path = dir.join(format!("synthetic.{}", project::PROJECT_EXTENSION));
    let sealed = project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources,
        algorithm: "weighted_average",
        params: &ParamValues::new(),
        align: None,
        registration,
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");
    (project_path, sealed.checkpoint)
}

/// Two frames, the second a 0.8-scaled copy of the first with a few
/// full-scale samples, sealed with the gains that undo the scale.
fn gained_pair(name: &str) -> (PathBuf, PathBuf, String, Vec<PathBuf>) {
    let dir = tmp_dir(name);
    let frames_dir = dir.join("frames");
    std::fs::create_dir_all(&frames_dir).unwrap();
    let len = (WIDTH * HEIGHT * SPP) as usize;
    let first: Vec<u16> = (0..len).map(|s| (s * 131 % 40_000 + 100) as u16).collect();
    let second: Vec<u16> = first
        .iter()
        .enumerate()
        .map(|(s, &v)| {
            if s % 97 == 0 || s == (24 * WIDTH as usize + 30) * SPP as usize {
                65_535
            } else {
                (u32::from(v) * 4 / 5) as u16
            }
        })
        .collect();
    let sources: Vec<PathBuf> = [first, second]
        .iter()
        .enumerate()
        .map(|(i, samples)| {
            let path = frames_dir.join(format!("frame-{i}.tif"));
            write_tiff(&path, WIDTH, HEIGHT, SPP, samples);
            path
        })
        .collect();
    let registration = Registration {
        equalization: Some(Equalization {
            max_gain: 2.0,
            target_stops: 0.0,
            gains: vec![65_536, 81_920],
        }),
        ..Registration::default()
    };
    let (project_path, root) = seal_synthetic(&dir, &sources, (WIDTH, HEIGHT, SPP), registration);
    (dir, project_path, root, sources)
}

#[test]
fn frame_source_reads_with_recorded_gain() {
    let (dir, project_path, root, sources) = gained_pair("retouch_ops_gain");
    let edited = append(
        &project_path,
        &root,
        vec![stroke(1, &[(30, 24)], 10.0, 0.0)],
    );

    let mut project = Project::open(&project_path).unwrap();
    let gain = project.fuse_state(&edited).unwrap().registration.gain(1);
    assert_ne!(gain, 65_536, "frame 1 has a recorded gain");
    let (_, _, _, raw) = read_tiff(&sources[1]);
    let (_, _, _, stored) = read_tiff(&dir.join("composite.tif"));

    let image = CheckpointImage::prepare(&project, &edited).unwrap();
    let rendered = try_render(&image, &mut project, image.full()).unwrap();
    let (mut inside, mut saturated, mut differs_from_raw) = (0, 0, 0);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let d2 = (x as i64 - 30).pow(2) + (y as i64 - 24).pow(2);
            for c in 0..SPP as usize {
                let i = ((y * WIDTH + x) * SPP) as usize + c;
                if d2 <= 81 {
                    let expected =
                        ((u64::from(raw[i]) * u64::from(gain) + 32_768) >> 16).min(65_535) as u16;
                    assert_eq!(rendered[i], expected, "({x}, {y}, {c}) inside the stroke");
                    inside += 1;
                    saturated += usize::from(expected == 65_535);
                    differs_from_raw += usize::from(expected != raw[i]);
                } else if d2 >= 121 {
                    assert_eq!(rendered[i], stored[i], "({x}, {y}, {c}) outside the stroke");
                }
            }
        }
    }
    assert!(inside > 0 && saturated > 0 && differs_from_raw > 0);
}

#[test]
fn untouched_tile_reads_no_source() {
    let (dir, project_path, root, _) = gained_pair("retouch_ops_untouched");
    let edited = append(&project_path, &root, vec![stroke(1, &[(10, 24)], 8.0, 0.3)]);
    let (_, _, _, stored) = read_tiff(&dir.join("composite.tif"));

    let mut project = Project::open(&project_path).unwrap();
    let image = CheckpointImage::prepare(&project, &edited).unwrap();
    std::fs::rename(dir.join("frames"), dir.join("frames-gone")).unwrap();

    let away = Rect {
        x: 30,
        y: 0,
        width: 34,
        height: HEIGHT,
    };
    let rendered = try_render(&image, &mut project, away).expect("no stroke meets this tile");
    let spp = SPP as usize;
    let mut expected = Vec::new();
    for y in 0..HEIGHT as usize {
        let start = (y * WIDTH as usize + 30) * spp;
        expected.extend_from_slice(&stored[start..start + 34 * spp]);
    }
    assert_eq!(
        rendered, expected,
        "an untouched tile is the stored composite"
    );

    let touched = Rect {
        x: 0,
        y: 20,
        width: 20,
        height: 10,
    };
    match try_render(&image, &mut project, touched) {
        Err(ProjectError::RenderSourcesUnavailable { detail, .. }) => {
            assert!(detail.contains("missing:"), "{detail}");
        }
        other => panic!("a tile the stroke meets needs its sources: {other:?}"),
    }
}

#[test]
fn edited_export_names_missing_source() {
    let (dir, project_path, root, sources) = gained_pair("retouch_ops_missing");
    let edited = append(
        &project_path,
        &root,
        vec![stroke(1, &[(30, 24)], 10.0, 0.3)],
    );
    std::fs::remove_file(&sources[1]).unwrap();

    let mut project = Project::open(&project_path).unwrap();
    let out = dir.join("edited.tif");
    let err = project::export_checkpoint(
        &mut project,
        &edited,
        &out,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .expect_err("a stroke whose frame is gone cannot render");
    let text = err.to_string();
    assert!(
        text.contains("missing:") && text.contains("frame-1.tif"),
        "{text}"
    );
    assert!(
        !out.exists() && !partial_path(&out).exists(),
        "nothing is left behind"
    );

    let plain = dir.join("plain.tif");
    project::export_checkpoint(
        &mut project,
        &root,
        &plain,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .expect("the unedited state needs no source frame");
    assert!(plain.exists());
}

#[test]
fn stroke_over_raw_frames() {
    let dngs: Vec<_> = (0..3u16)
        .map(|i| {
            let samples = (0..64u16).map(|j| 64 + i * 900 + j * 7).collect();
            write_dng(&DngSpec::new(12, 64, 4095, [0, 1, 1, 2], samples), false)
        })
        .collect();
    let sources: Vec<PathBuf> = dngs.iter().map(|f| f.path().to_path_buf()).collect();
    let set = open_frame_set(&sources, HostBudget::explicit(1 << 30)).unwrap();
    let (width, height) = set.reader.dimensions();
    let spp = set.reader.stack_context().samples_per_pixel;
    assert_eq!(spp, 3, "a developed RAW frame is RGB");

    let dir = tmp_dir("retouch_ops_raw");
    let (project_path, root) = seal_synthetic(
        &dir,
        &sources,
        (width, height, spp),
        Registration::default(),
    );
    let ops = vec![stroke(2, &[(2, 3), (5, 4)], 3.0, 0.5)];
    let edited = append(&project_path, &root, ops.clone());

    let composite = composite_pattern_f32((width * height * spp) as usize);
    let blended = reference_blend(
        &composite,
        (width, height, spp),
        sources.len() as u32,
        &ops,
        &*set.reader,
        (0, 0),
    );
    assert_ne!(blended, composite, "the stroke must change the image");
    let expected: Vec<u16> = blended.iter().map(|&v| u16::narrow(v)).collect();

    let mut project = Project::open(&project_path).unwrap();
    let view = dir.join("view.tif");
    let export = dir.join("export.tif");
    project::render_checkpoint(&mut project, &edited, &view, None).unwrap();
    project::export_checkpoint(
        &mut project,
        &edited,
        &export,
        &ExportOptions::default(),
        &MetadataOptions::default(),
    )
    .unwrap();
    assert_eq!(
        read_tiff(&view).3,
        expected,
        "the view blends the RAW frame"
    );
    assert_eq!(read_tiff(&view), read_tiff(&export), "view equals export");
}

/// `id` rendered whole.
fn render_state(project_path: &Path, id: &str) -> Vec<u16> {
    let mut project = Project::open(project_path).unwrap();
    let image = CheckpointImage::prepare(&project, id).unwrap();
    render_tiled(&image, &mut project, 32)
}

#[test]
fn states_undo_branch_and_name() {
    let (_, project_path, root, sources) = gained_pair("retouch_ops_states");
    let strokes = [
        stroke(1, &[(10, 10), (20, 14)], 6.0, 0.0),
        stroke(1, &[(30, 24), (40, 30)], 8.0, 0.5),
        // Starts outside the image and leaves it again.
        stroke(1, &[(-12, 40), (20, 44), (80, 40)], 7.0, 0.3),
    ];
    let mut states = vec![root.clone()];
    for (k, op) in strokes.iter().enumerate() {
        let label = (k == 1).then(|| "second".to_string());
        let sealed = project::append_ops_onto(
            &project_path,
            states.last().unwrap(),
            vec![op.clone()],
            label,
            1 + k as i64,
        )
        .expect("the stroke must append");
        states.push(sealed.checkpoint);
    }

    let composite = composite_pattern((WIDTH * HEIGHT * SPP) as usize);
    let registration = Project::open(&project_path)
        .unwrap()
        .fuse_state(&root)
        .unwrap()
        .registration;
    let set = open_frame_set(&sources, HostBudget::explicit(1 << 30)).unwrap();
    let gained = GainReader::new(&*set.reader, registration.gains());
    let warping = WarpingReader::new(
        &gained,
        set.reader.dimensions(),
        SPP,
        registration.transforms(sources.len()),
    );
    let expected = |ops: &[Op]| {
        reference_blend(
            &composite,
            (WIDTH, HEIGHT, SPP),
            sources.len() as u32,
            ops,
            &warping,
            registration.origin(),
        )
    };
    let before: Vec<Vec<u16>> = states
        .iter()
        .map(|id| render_state(&project_path, id))
        .collect();
    for (k, rendered) in before.iter().enumerate() {
        assert_eq!(rendered, &expected(&strokes[..k]), "state {k}");
    }
    assert_ne!(before[0], before[1], "a stroke changes the image");

    let branch = project::append_ops_onto(
        &project_path,
        &states[1],
        vec![strokes[2].clone()],
        Some("branch".to_string()),
        9,
    )
    .expect("a stroke under an older state must append")
    .checkpoint;

    let project = Project::open(&project_path).unwrap();
    let history = project.history();
    let listed: Vec<&str> = history.visible().iter().map(|c| c.id.as_str()).collect();
    for id in states.iter().chain([&branch]) {
        assert!(listed.contains(&id.as_str()), "{id} stays listed");
    }
    let mut under_first: Vec<&str> = history
        .children(&states[1])
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    under_first.sort_unstable();
    let mut both = vec![states[2].as_str(), branch.as_str()];
    both.sort_unstable();
    assert_eq!(
        under_first, both,
        "the branch sits beside the chain's second state"
    );
    assert_eq!(
        history.get(&states[2]).unwrap().label.as_deref(),
        Some("second")
    );
    assert_eq!(
        history.get(&branch).unwrap().label.as_deref(),
        Some("branch")
    );
    let root_blob = history.get(&root).unwrap().zmap_blob.clone();
    for id in &states[1..] {
        assert_eq!(history.get(id).unwrap().zmap_blob, root_blob, "{id}");
    }
    assert_eq!(history.get(&branch).unwrap().zmap_blob, root_blob);
    drop(project);

    for (k, id) in states.iter().enumerate() {
        assert_eq!(
            render_state(&project_path, id),
            before[k],
            "state {k} renders unchanged after the branch"
        );
    }
    assert_eq!(
        render_state(&project_path, &branch),
        expected(&[strokes[0].clone(), strokes[2].clone()]),
        "the branch renders its own prefix"
    );
}

/// Names of the blob records the project file lists.
fn blob_entries(project_path: &Path) -> Vec<String> {
    Index::read(project_path)
        .unwrap()
        .names()
        .filter(|name| name.starts_with("blobs/"))
        .map(str::to_string)
        .collect()
}

#[test]
fn stroke_append_costs_kilobytes() {
    let (_, project_path, root, _) = gained_pair("retouch_ops_cost");
    let path: Vec<(i32, i32)> = (0..64).map(|i| (i % 60, 8 + i % 30)).collect();
    let before_blobs = blob_entries(&project_path);
    let before_manifest = Project::open(&project_path)
        .unwrap()
        .manifest()
        .blobs
        .clone();
    let before_len = std::fs::metadata(&project_path).unwrap().len();

    let edited = append(&project_path, &root, vec![stroke(1, &path, 4.0, 0.2)]);

    assert_eq!(blob_entries(&project_path), before_blobs, "no blob record");
    let project = Project::open(&project_path).unwrap();
    assert_eq!(project.manifest().blobs, before_manifest, "manifest blobs");
    let (_, record_len) = Index::read(&project_path)
        .unwrap()
        .get(&format!("checkpoints/{edited}.json"))
        .expect("the checkpoint record is indexed");
    assert!(record_len < 2048, "the record takes {record_len} bytes");
    let growth = std::fs::metadata(&project_path).unwrap().len() - before_len;
    assert!(growth < 64 * 1024, "the file grew by {growth} bytes");
}

#[test]
fn append_refuses_bad_ops_and_writes_nothing() {
    let (_, project_path, root, _) = gained_pair("retouch_ops_refused");
    let good = stroke(1, &[(10, 10)], 4.0, 0.0);
    let cases = [
        (vec![good.clone(), stroke(2, &[(10, 10)], 4.0, 0.0)], 1),
        (vec![stroke(1, &[(10, 10)], 4.0, 1.5)], 0),
        (vec![stroke(1, &[], 4.0, 0.0)], 0),
        (vec![stroke(1, &[(10, 10)], 0.0, 0.0)], 0),
        (
            vec![good.clone(), good.clone(), stroke(1, &[], 4.0, 0.0)],
            2,
        ),
    ];
    let bytes = std::fs::read(&project_path).unwrap();
    for (ops, at) in cases {
        match project::append_ops_onto(&project_path, &root, ops.clone(), None, 1) {
            Err(ProjectError::Op { index, .. }) => assert_eq!(index, at),
            other => panic!("expected op #{at} to be refused, got {:?}", other.err()),
        }
        match project::append_op_chain(&project_path, Some(&root), ops, None, 1) {
            Err(ProjectError::Op { index, .. }) => assert_eq!(index, at),
            other => panic!("expected op #{at} to be refused, got {:?}", other.err()),
        }
        assert_eq!(
            std::fs::read(&project_path).unwrap(),
            bytes,
            "file untouched"
        );
    }
    match project::append_op_chain(&project_path, None, Vec::new(), None, 1) {
        Err(ProjectError::EmptyOpList { .. }) => {}
        other => panic!("expected an empty list refused, got {:?}", other.err()),
    }
    assert_eq!(
        std::fs::read(&project_path).unwrap(),
        bytes,
        "file untouched"
    );
}

#[test]
fn q16_zero_opacity_changes_nothing() {
    let (_, project_path, root, _) = gained_pair("retouch_ops_q16_zero");
    let faint = Op::BrushStroke {
        source: StrokeSource::Frame(1),
        path: vec![Point { x: 10, y: 10 }, Point { x: 30, y: 20 }],
        radius: 9.0,
        softness: 0.0,
        mode: BrushMode::Normal,
        opacity: 1e-9,
    };
    let ids = project::append_op_chain(&project_path, Some(&root), vec![faint], None, 1)
        .expect("an opacity that rounds to zero is accepted");
    assert_eq!(ids.len(), 1, "one state is recorded");
    assert!(
        render_state(&project_path, &ids[0]) == render_state(&project_path, &root),
        "the state renders sample-identical to its parent"
    );
}

#[test]
fn op_chain_appends_one_state_per_op() {
    let (_, project_path, root, _) = gained_pair("retouch_ops_chain");
    let twin = project_path.with_file_name("twin.ffp");
    std::fs::copy(&project_path, &twin).unwrap();
    let strokes = vec![
        stroke(1, &[(10, 10), (20, 14)], 6.0, 0.0),
        stroke(0, &[(30, 24), (40, 30)], 8.0, 0.5),
        stroke(1, &[(-12, 40), (20, 44), (80, 40)], 7.0, 0.3),
    ];

    let ids = project::append_op_chain(
        &project_path,
        None,
        strokes.clone(),
        Some("strokes".to_string()),
        5,
    )
    .expect("the chain must append");

    assert_eq!(ids.len(), 3);
    let project = Project::open(&project_path).unwrap();
    let history = project.history();
    let mut parent = root.clone();
    for (id, op) in ids.iter().zip(&strokes) {
        let checkpoint = history.get(id).unwrap();
        assert_eq!(checkpoint.parent.as_deref(), Some(parent.as_str()));
        assert_eq!(checkpoint.ops, std::slice::from_ref(op));
        parent = id.clone();
    }
    let labels: Vec<Option<&str>> = ids
        .iter()
        .map(|id| history.get(id).unwrap().label.as_deref())
        .collect();
    assert_eq!(labels, [None, None, Some("strokes")], "the last state only");
    assert_eq!(project.checkpoint().id, ids[2], "the chain moves the head");
    drop(project);

    let mut sequential = root.clone();
    for (id, op) in ids.iter().zip(&strokes) {
        sequential = append(&twin, &sequential, vec![op.clone()]);
        assert_eq!(
            render_state(&project_path, id),
            render_state(&twin, &sequential),
            "a chained state renders as the same stroke appended on its own"
        );
    }

    let branch =
        project::append_op_chain(&project_path, Some(&ids[0]), strokes[1..].to_vec(), None, 6)
            .expect("a chain under an older state must append");
    let project = Project::open(&project_path).unwrap();
    let history = project.history();
    assert_eq!(
        history.get(&branch[0]).unwrap().parent.as_deref(),
        Some(ids[0].as_str())
    );
    assert_eq!(
        history.children(&ids[0]).len(),
        2,
        "the branch sits beside the chain's second state"
    );
    assert_eq!(
        history.get(&ids[1]).unwrap().parent.as_deref(),
        Some(ids[0].as_str())
    );
}

fn stroke_in(
    mode: BrushMode,
    (source_id, path, radius): (u16, &[(i32, i32)], f32),
    opacity: f32,
) -> Op {
    Op::BrushStroke {
        source: StrokeSource::Frame(source_id),
        path: path.iter().map(|&(x, y)| Point { x, y }).collect(),
        radius,
        softness: 0.3,
        mode,
        opacity,
    }
}

/// `ops` blended over the whole of `composite` in one `apply_strokes` call:
/// no tile and no band, so every blur window sees the map.
fn reference_whole<S: Blend>(
    composite: &[S],
    (width, height, spp): (u32, u32, u32),
    frame_count: u32,
    ops: &[Op],
    frames: &dyn TileReader,
    origin: (u32, u32),
) -> Vec<S> {
    let info = ZMapInfo::new(width, height, frame_count).unwrap();
    let placed: Vec<blend::PlacedStroke> = ops
        .iter()
        .filter_map(|op| {
            let frame = blend::source_frame(op).unwrap();
            blend::footprint(op, info)
                .unwrap()
                .map(|footprint| blend::PlacedStroke {
                    op,
                    source: blend::PlacedSource::Frame(usize::from(frame)),
                    footprint,
                })
        })
        .collect();
    let mut out = composite.to_vec();
    blend::apply_strokes(
        info,
        full(width, height),
        &mut out,
        &placed,
        frames,
        origin,
        spp as usize,
        &[],
        blend::apply::<S>,
    )
    .unwrap();
    out
}

/// `rect`'s rows and columns of a `width`-wide plane of `spp` samples.
fn cut<S: Copy>(plane: &[S], width: u32, spp: u32, rect: Rect) -> Vec<S> {
    let (width, spp) = (width as usize, spp as usize);
    (rect.y..rect.y + rect.height)
        .flat_map(|y| {
            let at = (y as usize * width + rect.x as usize) * spp;
            plane[at..at + rect.width as usize * spp].iter().copied()
        })
        .collect()
}

/// The samples `render` hands its sink for `tile`, as the tile's own rows;
/// every band must lie inside the tile, in row order.
fn collect<S: Copy + Default>(
    spp: u32,
    tile: Rect,
    render: impl FnOnce(
        &mut dyn FnMut(&[S], Rect) -> Result<(), ProjectError>,
    ) -> Result<(), ProjectError>,
) -> Vec<S> {
    let spp = spp as usize;
    let row = tile.width as usize * spp;
    let mut out = vec![S::default(); row * tile.height as usize];
    let mut next_row = tile.y;
    render(&mut |band, rect| {
        assert_eq!(
            tile.intersect(rect),
            Some(rect),
            "a band lies inside the tile"
        );
        assert_eq!(rect.y, next_row, "bands arrive in row order");
        next_row += rect.height;
        let len = rect.width as usize * spp;
        for r in 0..rect.height as usize {
            let dst = (rect.y - tile.y) as usize * row + r * row + (rect.x - tile.x) as usize * spp;
            out[dst..dst + len].copy_from_slice(&band[r * len..(r + 1) * len]);
        }
        Ok(())
    })
    .expect("the tile must render");
    assert_eq!(next_row, tile.y + tile.height, "the bands cover the tile");
    out
}

fn differing<S: PartialEq>(a: &[S], b: &[S]) -> usize {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).filter(|(a, b)| a != b).count()
}

/// Two Detail strokes with a Normal one between them, overlapping, around
/// `(cx, cy)`; each Detail margin is `3 * ((16 * radius + 24) / 48)`.
fn detail_pile(cx: i32, cy: i32, radius: f32) -> Vec<Op> {
    let near = radius as i32 / 2;
    vec![
        stroke_in(
            BrushMode::Detail,
            (1, &[(cx - near, cy), (cx + near, cy + 10)], radius),
            1.0,
        ),
        stroke_in(BrushMode::Normal, (0, &[(cx, cy + 5)], radius * 0.6), 1.0),
        stroke_in(
            BrushMode::Detail,
            (1, &[(cx, cy - 10), (cx + 2 * near, cy)], radius),
            0.7,
        ),
    ]
}

#[test]
fn detail_tiled_equals_untiled_on_mantis() {
    let dir = tmp_dir("retouch_ops_detail_mantis");
    let frames = frame_paths(&common::mantis_512_dir().join("frames")).unwrap();
    let held = common::fuse_held(&frames, "weighted-average", true, &dir);
    let project_path = dir.join(format!("mantis.{}", project::PROJECT_EXTENSION));
    let root = seal_held(&project_path, &held);
    let ops = detail_pile(256, 256, 40.0);
    let edited = append(&project_path, &root, ops.clone());

    let (width, height, spp, composite) = read_tiff(&held.composite);
    let set = open_frame_set(&held.sources, HostBudget::explicit(1 << 30)).unwrap();
    let gained = GainReader::new(&*set.reader, held.registration.gains());
    let warping = WarpingReader::new(
        &gained,
        set.reader.dimensions(),
        spp,
        held.registration.transforms(held.sources.len()),
    );
    let expected = reference_whole(
        &composite,
        (width, height, spp),
        held.sources.len() as u32,
        &ops,
        &warping,
        held.registration.origin(),
    );
    assert_ne!(expected, composite, "the strokes must change the image");

    let mut project = Project::open(&project_path).unwrap();
    let image = CheckpointImage::prepare(&project, &edited).unwrap();
    for size in [16, 64, 100, 512] {
        let rendered = render_tiled(&image, &mut project, size);
        assert_eq!(
            differing(&rendered, &expected),
            0,
            "tile size {size}: samples differ from the untiled render"
        );
    }
}

/// A 2-frame source set of `width` x `height` x 3 `u16` TIFFs sealed under a
/// pattern composite.
fn wide_u16_project(
    name: &str,
    width: u32,
    height: u32,
) -> (PathBuf, PathBuf, String, Vec<PathBuf>) {
    let dir = tmp_dir(name);
    let len = (width * height * SPP) as usize;
    let sources: Vec<PathBuf> = (0..2usize)
        .map(|i| {
            let path = dir.join(format!("frame-{i}.tif"));
            let samples: Vec<u16> = (0..len)
                .map(|s| ((s * (5 + 2 * i) + i * 4001) % 60_001) as u16)
                .collect();
            write_tiff(&path, width, height, SPP, &samples);
            path
        })
        .collect();
    let (project_path, root) = seal_synthetic(
        &dir,
        &sources,
        (width, height, SPP),
        Registration::default(),
    );
    (dir, project_path, root, sources)
}

fn pending(dir: &Path, sources: &[PathBuf], ops: &[Op]) -> project::PendingState {
    project::PendingState {
        composite: dir.join("composite.tif"),
        ops: ops.to_vec(),
        sources: sources.to_vec(),
        registration: Registration::default(),
        reference: 0,
        description: "staged composite".to_string(),
    }
}

#[test]
fn detail_bands_below_the_margin_equal_untiled_u16() {
    let (width, height) = (8192, 400);
    let (dir, project_path, root, sources) =
        wide_u16_project("retouch_ops_detail_wide", width, height);
    let ops = detail_pile(3200, 200, 120.0);
    let info = ZMapInfo::new(width, height, 2).unwrap();
    let margin = blend::read_margin(&ops[0], info).unwrap();
    assert!(
        margin > 100,
        "the margin {margin} exceeds the rows of a band"
    );
    let edited = append(&project_path, &root, ops.clone());

    let set = open_frame_set(&sources, HostBudget::explicit(1 << 30)).unwrap();
    let composite = composite_pattern((width * height * SPP) as usize);
    let expected = reference_whole(
        &composite,
        (width, height, SPP),
        2,
        &ops,
        &*set.reader,
        (0, 0),
    );
    assert_ne!(expected, composite, "the strokes must change the image");

    let inner = Rect {
        x: 2900,
        y: 120,
        width: 700,
        height: 140,
    };
    let mut project = Project::open(&project_path).unwrap();
    let saved = CheckpointImage::prepare(&project, &edited).unwrap();
    let state = pending(&dir, &sources, &ops);
    let live = CheckpointImage::prepare_pending(&state, None).unwrap();
    for tile in [full(width, height), inner] {
        let want = cut(&expected, width, SPP, tile);
        let from_saved = collect(SPP, tile, |sink| {
            saved.render(Some(&mut project), tile, sink)
        });
        assert_eq!(differing(&from_saved, &want), 0, "saved render of {tile:?}");
        let from_pending = collect(SPP, tile, |sink| live.render(None, tile, sink));
        assert_eq!(
            differing(&from_pending, &want),
            0,
            "pending render of {tile:?}"
        );
    }
}

#[test]
fn detail_bands_below_the_margin_equal_untiled_f32() {
    let dngs: Vec<_> = (0..2u32)
        .map(|i| {
            let mut spec = DngSpec::new(
                12,
                64,
                4095,
                [0, 1, 1, 2],
                (0..8192 * 400u32)
                    .map(|j| 64 + ((j * (7 + 2 * i) + i * 997) % 3900) as u16)
                    .collect(),
            );
            spec.width = 8192;
            spec.height = 400;
            write_dng(&spec, false)
        })
        .collect();
    let sources: Vec<PathBuf> = dngs.iter().map(|f| f.path().to_path_buf()).collect();
    let set = open_frame_set(&sources, HostBudget::explicit(1 << 30)).unwrap();
    let (width, height) = set.reader.dimensions();
    let spp = set.reader.stack_context().samples_per_pixel;
    assert_eq!(spp, 3, "a developed RAW frame is RGB");
    assert!(width >= 4096 && height >= 300, "{width}x{height}");

    let dir = tmp_dir("retouch_ops_detail_raw_wide");
    let (project_path, root) = seal_synthetic(
        &dir,
        &sources,
        (width, height, spp),
        Registration::default(),
    );
    let ops = detail_pile(width as i32 / 2, height as i32 / 2, 120.0);
    let info = ZMapInfo::new(width, height, 2).unwrap();
    assert!(blend::read_margin(&ops[0], info).unwrap() > 100);
    let edited = append(&project_path, &root, ops.clone());

    let composite = composite_pattern_f32((width * height * spp) as usize);
    let expected = reference_whole(
        &composite,
        (width, height, spp),
        2,
        &ops,
        &*set.reader,
        (0, 0),
    );
    assert_ne!(expected, composite, "the strokes must change the image");

    let inner = Rect {
        x: width / 2 - 300,
        y: height / 2 - 70,
        width: 600,
        height: 140,
    };
    let mut project = Project::open(&project_path).unwrap();
    let saved = CheckpointImage::prepare(&project, &edited).unwrap();
    let state = pending(&dir, &sources, &ops);
    let live = CheckpointImage::prepare_pending(&state, None).unwrap();
    let bits = |plane: &[f32]| plane.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
    for tile in [full(width, height), inner] {
        let want = bits(&cut(&expected, width, spp, tile));
        let from_saved = collect(spp, tile, |sink| {
            saved.render_f32(Some(&mut project), tile, sink)
        });
        assert_eq!(
            differing(&bits(&from_saved), &want),
            0,
            "saved render of {tile:?}"
        );
        let from_pending = collect(spp, tile, |sink| live.render_f32(None, tile, sink));
        assert_eq!(
            differing(&bits(&from_pending), &want),
            0,
            "pending render of {tile:?}"
        );
    }
}

#[test]
fn untouched_tile_reads_no_source_inside_a_detail_margin() {
    let (dir, project_path, root, _) = gained_pair("retouch_ops_untouched_detail");
    let ops = vec![
        stroke(1, &[(10, 24)], 8.0, 0.3),
        stroke_in(BrushMode::Detail, (1, &[(50, 24)], 8.0), 1.0),
    ];
    let edited = append(&project_path, &root, ops);
    let (_, _, _, stored) = read_tiff(&dir.join("composite.tif"));
    let mut project = Project::open(&project_path).unwrap();
    let image = CheckpointImage::prepare(&project, &edited).unwrap();
    std::fs::rename(dir.join("frames"), dir.join("frames-gone")).unwrap();

    // The Detail stroke has pass radius 3, margin 9: x 33..41 lies within the
    // margin of its footprint (x 42..58) and is not written.
    let margin_only = Rect {
        x: 33,
        y: 10,
        width: 8,
        height: 28,
    };
    let rendered = try_render(&image, &mut project, margin_only)
        .expect("a tile no footprint meets reads no frame, margin or not");
    assert_eq!(rendered, cut(&stored, WIDTH, SPP, margin_only));

    let on_detail = Rect {
        x: 40,
        y: 10,
        width: 8,
        height: 28,
    };
    assert!(matches!(
        try_render(&image, &mut project, on_detail),
        Err(ProjectError::RenderSourcesUnavailable { .. })
    ));
}

/// Tiled equals untiled for a Detail stroke whose radius exceeds the tile
/// size, on windows cut from two frames of the reference stack; then the
/// window bytes a 10-stroke Detail pile reads at full stack size.
#[test]
#[ignore = "reads the 16 GB reference stack"]
fn detail_margin_reference_stack() {
    let Some(stack_dir) = common::reference_stack_dir() else {
        return;
    };
    let frames: Vec<PathBuf> = frame_paths(&stack_dir)
        .unwrap()
        .into_iter()
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().contains("IMG_"))
        })
        .collect();
    assert!(frames.len() >= 2, "the reference stack holds its frames");
    let stack = TiffStack::open_files(&frames).unwrap();
    let (stack_width, stack_height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;

    let (window, tile_size) = (640u32, 128u32);
    let at = Rect {
        x: stack_width / 2 - window / 2,
        y: stack_height / 2 - window / 2,
        width: window,
        height: window,
    };
    let dir = tmp_dir("retouch_ops_detail_reference");
    let sources: Vec<PathBuf> = (0..2)
        .map(|i| {
            let mut samples = vec![0u16; (window * window * spp) as usize];
            stack.read_tile_into(i, at, &mut samples).unwrap();
            let path = dir.join(format!("window-{i}.tif"));
            write_tiff(&path, window, window, spp, &samples);
            path
        })
        .collect();
    let (project_path, root) = seal_synthetic(
        &dir,
        &sources,
        (window, window, spp),
        Registration::default(),
    );
    let c = window as i32 / 2;
    let ops = vec![
        stroke_in(
            BrushMode::Detail,
            (1, &[(c - 60, c), (c + 40, c + 20)], 200.0),
            1.0,
        ),
        stroke_in(
            BrushMode::Detail,
            (1, &[(c, c - 30), (c + 80, c)], 200.0),
            0.8,
        ),
    ];
    let info = ZMapInfo::new(window, window, 2).unwrap();
    let margin = blend::read_margin(&ops[0], info).unwrap();
    assert!(margin > tile_size, "the margin {margin} exceeds the tile");
    let edited = append(&project_path, &root, ops.clone());

    let set = open_frame_set(&sources, HostBudget::explicit(1 << 30)).unwrap();
    let composite = composite_pattern((window * window * spp) as usize);
    let expected = reference_whole(
        &composite,
        (window, window, spp),
        2,
        &ops,
        &*set.reader,
        (0, 0),
    );
    assert_ne!(expected, composite, "the strokes must change the image");
    let mut project = Project::open(&project_path).unwrap();
    let image = CheckpointImage::prepare(&project, &edited).unwrap();
    let rendered = render_tiled(&image, &mut project, tile_size);
    assert_eq!(
        differing(&rendered, &expected),
        0,
        "tiled differs from untiled"
    );

    // Ten overlapping strokes of radius 200 stepping 40 px along a line, at
    // the full stack's size: the area one write rect at the pile's end reads.
    let full_info = ZMapInfo::new(stack_width, stack_height, frames.len() as u32).unwrap();
    let (cx, cy) = (stack_width as i32 / 2, stack_height as i32 / 2);
    let pile: Vec<Op> = (0..10)
        .map(|i| stroke_in(BrushMode::Detail, (1, &[(cx + 40 * i, cy)], 200.0), 1.0))
        .collect();
    let placed: Vec<blend::PlacedStroke> = pile
        .iter()
        .map(|op| blend::PlacedStroke {
            op,
            source: blend::PlacedSource::Frame(1),
            footprint: blend::footprint(op, full_info).unwrap().unwrap(),
        })
        .collect();
    let write = Rect {
        x: cx as u32,
        y: cy as u32,
        width: tile_size,
        height: tile_size,
    };
    let extent = blend::read_extent(full_info, &placed, write);
    let bytes = u64::from(extent.width) * u64::from(extent.height) * u64::from(spp) * 2;
    eprintln!(
        "10-stroke Detail pile on {stack_width}x{stack_height}: write {write:?} reads {extent:?}, window {bytes} bytes"
    );
}
