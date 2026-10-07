// SPDX-License-Identifier: GPL-3.0-or-later

//! [`CheckpointImage`]: a fused checkpoint renders its stored composite, an
//! op-bearing checkpoint its ancestor's composite with each brush stroke
//! blended in from the ancestor's aligned, cropped sources.

use std::path::{Path, PathBuf};

use focusforge_core::algorithm::TileReader;
use focusforge_core::align::{Registration, Transform, WarpingReader};
use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter};
use focusforge_core::params::ParamValues;
use focusforge_core::project::{
    self, AlignRecord, CheckpointImage, Metrics, Op, Point, Project, SealRequest,
};
use focusforge_core::retouch::blend;
use focusforge_core::zmap::{NO_SOURCE, ZMapInfo, ZMapWriter};

const FRAME_W: u32 = 12;
const FRAME_H: u32 = 10;
const SPP: u32 = 1;
const CROP: Rect = Rect {
    x: 3,
    y: 2,
    width: 6,
    height: 5,
};
/// Fixed composite value: distinct from every source pixel (below 2119), so
/// a rendered pixel that still equals it proves the stored composite was
/// used, not a gather.
const COMPOSITE_VALUE: u16 = 5000;

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

fn write_frame(path: &Path, variant: u16) -> Vec<u16> {
    let samples: Vec<u16> = (0..(FRAME_W * FRAME_H) as usize)
        .map(|i| variant * 1000 + i as u16)
        .collect();
    let mut writer = TiffWriter::create(
        path,
        TiffSpec {
            width: FRAME_W,
            height: FRAME_H,
            samples_per_pixel: SPP,
            description: None,
        },
    )
    .unwrap();
    writer.write_tile(full(FRAME_W, FRAME_H), &samples).unwrap();
    writer.finish().unwrap();
    samples
}

/// A root fuse: three source frames, a crop and one non-identity per-frame
/// transform (frame 2 translated by `dx = 1.0`), a uniform composite that
/// matches no source pixel, and an ancestor Z-Map cycling through the three
/// frames.
struct Root {
    project_path: PathBuf,
    source_paths: Vec<PathBuf>,
    transforms: Vec<Transform>,
    root_id: String,
}

fn seal_root(name: &str) -> Root {
    let dir = tmp_dir(name);
    let source_paths: Vec<PathBuf> = (0..3)
        .map(|i| {
            let path = dir.join(format!("frame-{i:02}.tif"));
            write_frame(&path, i as u16);
            path
        })
        .collect();

    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let ancestor_zmap: Vec<u16> = (0..(CROP.width * CROP.height) as usize)
        .map(|i| (i % 3) as u16)
        .collect();
    let (composite_path, zmap_path) = write_staged(&staging, "root", &ancestor_zmap);

    let transforms = vec![
        Transform::identity(),
        Transform::identity(),
        Transform {
            m: [[1.0, 0.0, 1.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        },
    ];
    let align = AlignRecord {
        algorithm: "identity".to_string(),
        params: ParamValues::new(),
        reference: 0,
    };

    let project_path = dir.join(format!("root.{}", project::PROJECT_EXTENSION));
    let sealed = project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite_path,
        staged_zmap: &zmap_path,
        sources: &source_paths,
        algorithm: "weighted_average",
        params: &ParamValues::new(),
        align: Some(align),
        registration: Registration {
            transforms: transforms.clone(),
            crop: Some(CROP),
            equalization: None,
        },
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");

    Root {
        project_path,
        source_paths,
        transforms,
        root_id: sealed.checkpoint,
    }
}

/// A `CROP`-sized staged composite of `COMPOSITE_VALUE` and a staged Z-Map
/// holding `zmap`, both under `staging` with `prefix`.
fn write_staged(staging: &Path, prefix: &str, zmap: &[u16]) -> (PathBuf, PathBuf) {
    let composite_path = staging.join(format!("{prefix}-composite.tif"));
    let mut writer = TiffWriter::create(
        &composite_path,
        TiffSpec {
            width: CROP.width,
            height: CROP.height,
            samples_per_pixel: SPP,
            description: Some("staged composite".to_string()),
        },
    )
    .unwrap();
    writer
        .write_tile(
            full(CROP.width, CROP.height),
            &vec![COMPOSITE_VALUE; (CROP.width * CROP.height) as usize],
        )
        .unwrap();
    writer.finish().unwrap();

    let zmap_path = staging.join(format!("{prefix}-zmap.tif"));
    let mut map = ZMapWriter::create(
        &zmap_path,
        ZMapInfo::new(CROP.width, CROP.height, 3).unwrap(),
    )
    .unwrap();
    map.write_tile(full(CROP.width, CROP.height), zmap).unwrap();
    map.finish().unwrap();
    (composite_path, zmap_path)
}

/// Collects every band [`CheckpointImage::render`] hands its sink, in the
/// order it arrived, after `prepare` resolved `id`.
fn render_to_bands(
    project: &mut Project,
    id: &str,
) -> (CheckpointImageShape, Vec<(Rect, Vec<u16>)>) {
    let image = CheckpointImage::prepare(project, id).expect("prepare must succeed");
    let shape = CheckpointImageShape {
        width: image.width,
        height: image.height,
        samples_per_pixel: image.samples_per_pixel,
        description: image.description.clone(),
    };
    let mut bands = Vec::new();
    image
        .render(Some(project), image.full(), &mut |band, rect| {
            bands.push((rect, band.to_vec()));
            Ok(())
        })
        .expect("render must succeed");
    (shape, bands)
}

struct CheckpointImageShape {
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    description: String,
}

fn flatten(bands: &[(Rect, Vec<u16>)]) -> Vec<u16> {
    bands
        .iter()
        .flat_map(|(_, samples)| samples.clone())
        .collect()
}

#[test]
fn fused_render_needs_no_sources() {
    let root = seal_root("checkpoint_render_fused");
    for path in &root.source_paths {
        std::fs::remove_file(path).unwrap();
    }
    let mut project = Project::open(&root.project_path).unwrap();
    let (shape, bands) = render_to_bands(&mut project, &root.root_id);
    assert_eq!(shape.width, CROP.width);
    assert_eq!(shape.height, CROP.height);
    assert_eq!(shape.samples_per_pixel, SPP);
    assert_eq!(shape.description, "staged composite");
    let flat = flatten(&bands);
    assert_eq!(
        flat,
        vec![COMPOSITE_VALUE; (CROP.width * CROP.height) as usize]
    );
}

#[test]
fn unedited_child_renders_the_ancestor_composite() {
    let root = seal_root("checkpoint_render_unedited");
    // A stroke lying wholly off the image changes no pixel.
    let child = project::append_ops_onto(
        &root.project_path,
        &root.root_id,
        vec![Op::BrushStroke {
            source_id: 1,
            path: vec![Point { x: -40, y: -40 }],
            radius: 3.0,
            softness: 0.5,
        }],
        None,
        1,
    )
    .expect("append_ops_onto must succeed");

    let mut project = Project::open(&root.project_path).unwrap();
    for path in &root.source_paths {
        std::fs::remove_file(path).unwrap();
    }
    let (_shape, bands) = render_to_bands(&mut project, &child.checkpoint);
    let flat = flatten(&bands);
    assert_eq!(
        flat,
        vec![COMPOSITE_VALUE; (CROP.width * CROP.height) as usize]
    );
}

#[test]
fn op_bearing_render_blends_the_stroke_over_the_ancestor_composite() {
    let root = seal_root("checkpoint_render_blend");
    // From the translated frame, so the alignment and the crop origin both matter.
    let stroke = Op::BrushStroke {
        source_id: 2,
        path: vec![Point { x: 2, y: 2 }, Point { x: 4, y: 3 }],
        radius: 3.0,
        softness: 0.5,
    };
    let child = project::append_ops_onto(
        &root.project_path,
        &root.root_id,
        vec![stroke.clone()],
        None,
        1,
    )
    .expect("append_ops_onto must succeed");

    let stack = TiffStack::open_files(&root.source_paths).unwrap();
    let warping = WarpingReader::new(&stack, (FRAME_W, FRAME_H), SPP, root.transforms.clone());
    let area = (CROP.width * CROP.height) as usize;
    let mut frame = vec![0u16; area];
    warping
        .read_tile_into(
            2,
            Rect {
                x: CROP.x,
                y: CROP.y,
                ..full(CROP.width, CROP.height)
            },
            &mut frame,
        )
        .unwrap();
    let info = ZMapInfo::new(CROP.width, CROP.height, 3).unwrap();
    let alphas: Vec<u32> = (0..area as u32)
        .map(|i| blend::alpha(&stroke, info, i % CROP.width, i / CROP.width).unwrap())
        .collect();
    assert!(alphas.contains(&0), "the stroke leaves pixels alone");
    assert!(alphas.contains(&65_536), "the stroke has a core");
    assert!(
        alphas.iter().any(|&a| a > 0 && a < 65_536),
        "the stroke has a soft band"
    );
    let expected: Vec<u16> = (0..area)
        .map(|i| blend::blend_sample(COMPOSITE_VALUE, frame[i], alphas[i]))
        .collect();

    let mut project = Project::open(&root.project_path).unwrap();
    let (shape, bands) = render_to_bands(&mut project, &child.checkpoint);
    assert_eq!(
        shape.description, "staged composite",
        "an edited state's ImageDescription is its fused ancestor's, not a generic one"
    );
    assert_eq!(flatten(&bands), expected);
}

#[test]
fn band_sink_receives_full_width_bands_in_order() {
    // Small enough to build fast, large enough (`width * spp * 2` bytes per
    // row) that `READ_BAND_BYTES` (4 MiB) splits it into more than one band.
    let width = 4096u32;
    let height = 400u32;
    let spp = 3u32;
    let dir = tmp_dir("checkpoint_render_bands");
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    let composite_path = staging.join("composite.tif");
    let composite_samples: Vec<u16> = (0..(width * height * spp) as usize)
        .map(|i| (i % 65_535) as u16)
        .collect();
    let mut writer = TiffWriter::create(
        &composite_path,
        TiffSpec {
            width,
            height,
            samples_per_pixel: spp,
            description: Some("staged composite".to_string()),
        },
    )
    .unwrap();
    writer
        .write_tile(full(width, height), &composite_samples)
        .unwrap();
    writer.finish().unwrap();

    let zmap_path = staging.join("zmap.tif");
    let mut map = ZMapWriter::create(&zmap_path, ZMapInfo::new(width, height, 1).unwrap()).unwrap();
    map.write_tile(
        full(width, height),
        &vec![NO_SOURCE; (width * height) as usize],
    )
    .unwrap();
    map.finish().unwrap();

    let source_path = dir.join("frame-00.tif");
    write_frame_of(&source_path, width, height, spp);

    let project_path = dir.join(format!("bands.{}", project::PROJECT_EXTENSION));
    let sealed = project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite_path,
        staged_zmap: &zmap_path,
        sources: &[source_path],
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");

    let mut project = Project::open(&project_path).unwrap();
    let (_shape, bands) = render_to_bands(&mut project, &sealed.checkpoint);
    assert!(
        bands.len() > 1,
        "the image must split into more than one band"
    );
    let mut y = 0u32;
    for (rect, samples) in &bands {
        assert_eq!(rect.x, 0, "every band is full-width");
        assert_eq!(rect.width, width);
        assert_eq!(
            rect.y, y,
            "bands arrive top-to-bottom with no gap or overlap"
        );
        assert_eq!(samples.len(), (rect.width * rect.height * spp) as usize);
        y += rect.height;
    }
    assert_eq!(y, height, "the bands cover the whole image");
}

fn write_frame_of(path: &Path, width: u32, height: u32, spp: u32) {
    let mut writer = TiffWriter::create(
        path,
        TiffSpec {
            width,
            height,
            samples_per_pixel: spp,
            description: None,
        },
    )
    .unwrap();
    writer
        .write_tile(
            full(width, height),
            &vec![0u16; (width * height * spp) as usize],
        )
        .unwrap();
    writer.finish().unwrap();
}
