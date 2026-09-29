// SPDX-License-Identifier: GPL-3.0-or-later

//! [`CheckpointImage`]: the view render (a fused checkpoint's stored
//! composite, an op-bearing checkpoint's ancestor composite spliced with a
//! gather) and the export render (every checkpoint gathered from its own
//! Z-Map over its aligned, cropped sources).

use std::path::{Path, PathBuf};

use focusforge_core::algorithm::TileReader;
use focusforge_core::align::Transform;
use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter, open_frame_set};
use focusforge_core::params::ParamValues;
use focusforge_core::project::{
    self, AlignRecord, AppendRequest, CheckpointImage, Metrics, Op, Point, Project, ProjectError,
    SealRequest, ZMapStorage,
};
use focusforge_core::retouch::{Sources, recomposite};
use focusforge_core::tile::HostBudget;
use focusforge_core::zmap::{NO_SOURCE, ZMapInfo, ZMapReader, ZMapWriter};

#[path = "common/raw_dng.rs"]
mod raw_dng;
use raw_dng::{DngSpec, write_dng};

type Prepare = fn(&Project, &str) -> Result<CheckpointImage, ProjectError>;
const VIEW: Prepare = CheckpointImage::prepare;
const EXPORT: Prepare = CheckpointImage::prepare_export;

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
    ancestor_zmap: Vec<u16>,
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
        transforms: transforms.clone(),
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
        crop: Some(CROP),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");

    Root {
        project_path,
        source_paths,
        transforms,
        ancestor_zmap,
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

fn stroke(source_id: u16, at: i32) -> Vec<Op> {
    vec![Op::BrushStroke {
        source_id,
        path: vec![Point { x: at, y: at }],
        radius: 1.5,
        softness: 0.0,
    }]
}

/// [`seal_root`] plus a re-fuse (same sources, alignment and crop, Z-Map
/// `(i + 1) % 3`) that becomes the head, and one op-bearing child of each
/// fused checkpoint.
struct Branched {
    root: Root,
    refuse_id: String,
    root_child: String,
    refuse_child: String,
}

fn seal_branched(name: &str) -> Branched {
    let root = seal_root(name);
    let staging = root.project_path.parent().unwrap().join("staging");
    let refuse_zmap: Vec<u16> = (0..(CROP.width * CROP.height) as usize)
        .map(|i| ((i + 1) % 3) as u16)
        .collect();
    let (composite_path, zmap_path) = write_staged(&staging, "refuse", &refuse_zmap);
    let refuse_id = project::append(AppendRequest {
        project_path: &root.project_path,
        staged_composite: &composite_path,
        staged_zmap: &zmap_path,
        sources: &root.source_paths,
        algorithm: "weighted_average",
        params: &ParamValues::new(),
        align: Some(AlignRecord {
            algorithm: "identity".to_string(),
            params: ParamValues::new(),
            reference: 0,
            transforms: root.transforms.clone(),
        }),
        crop: Some(CROP),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("re-fuse must succeed")
    .checkpoint;
    let root_child = project::append_ops_onto(
        &root.project_path,
        &root.root_id,
        stroke(1, 1),
        None,
        1,
        ZMapStorage::Bounded,
    )
    .expect("root child must succeed")
    .checkpoint;
    let refuse_child = project::append_ops_onto(
        &root.project_path,
        &refuse_id,
        stroke(2, 3),
        None,
        2,
        ZMapStorage::Bounded,
    )
    .expect("re-fuse child must succeed")
    .checkpoint;
    Branched {
        root,
        refuse_id,
        root_child,
        refuse_child,
    }
}

/// A whole-image `recomposite` of `id`'s materialized Z-Map over `reader`
/// (frames of size `frame`), wrapped in the alignment and crop origin of
/// `id`'s fuse record.
fn gather_oracle(
    project: &mut Project,
    id: &str,
    reader: &dyn TileReader,
    frame: (u32, u32),
) -> Vec<u16> {
    let record = project.fuse_state(id).unwrap();
    let checkpoint = project.history().get(id).unwrap().clone();
    let zmap_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "checkpoint_render_oracle_{}_{id}.tif",
        std::process::id()
    ));
    project::materialize_zmap(project, &checkpoint, &zmap_path).unwrap();
    let map = ZMapReader::open(&zmap_path).unwrap();
    let (width, height) = map.dimensions();
    let mut indices = vec![0u16; (width * height) as usize];
    map.read_tile_into(full(width, height), &mut indices)
        .unwrap();
    std::fs::remove_file(&zmap_path).unwrap();

    let spp = record.composite.samples_per_pixel;
    let frames = record.sources.len();
    let transforms = match &record.align {
        Some(align) => align.transforms.clone(),
        None => vec![Transform::identity(); frames],
    };
    let warping = focusforge_core::align::WarpingReader::new(reader, frame, spp, transforms);
    let sources = Sources {
        reader: &warping,
        frame_count: frames as u32,
        samples_per_pixel: spp,
        origin: record.crop.map(|r| (r.x, r.y)).unwrap_or((0, 0)),
    };
    let mut gathered = vec![0u16; indices.len() * spp as usize];
    recomposite(&indices, full(width, height), &sources, &mut gathered).unwrap();
    gathered
}

/// Collects every band [`CheckpointImage::render`] hands its sink, in the
/// order it arrived, after `prepare` resolved `id`.
fn render_to_bands(
    project: &mut Project,
    id: &str,
    prepare: Prepare,
) -> (CheckpointImageShape, Vec<(Rect, Vec<u16>)>) {
    let image = prepare(project, id).expect("prepare must succeed");
    let shape = CheckpointImageShape {
        width: image.width,
        height: image.height,
        samples_per_pixel: image.samples_per_pixel,
        description: image.description.clone(),
    };
    let mut bands = Vec::new();
    image
        .render(project, &mut |band, rect| {
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
    let (shape, bands) = render_to_bands(&mut project, &root.root_id, VIEW);
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
    let child = project::append_ops_onto(
        &root.project_path,
        &root.root_id,
        vec![Op::ResetRegion {
            rect: full(CROP.width, CROP.height),
        }],
        None,
        1,
        ZMapStorage::Bounded,
    )
    .expect("append_ops_onto must succeed");

    let mut project = Project::open(&root.project_path).unwrap();
    let (_shape, bands) = render_to_bands(&mut project, &child.checkpoint, VIEW);
    let flat = flatten(&bands);
    assert_eq!(
        flat,
        vec![COMPOSITE_VALUE; (CROP.width * CROP.height) as usize]
    );
}

#[test]
fn op_bearing_render_is_ancestor_composite_plus_splice() {
    let root = seal_root("checkpoint_render_splice");
    let child = project::append_ops_onto(
        &root.project_path,
        &root.root_id,
        vec![Op::BrushStroke {
            source_id: 1,
            path: vec![Point { x: 1, y: 1 }],
            radius: 1.5,
            softness: 0.0,
        }],
        None,
        1,
        ZMapStorage::Bounded,
    )
    .expect("append_ops_onto must succeed");

    let mut project = Project::open(&root.project_path).unwrap();
    let child_checkpoint = project.history().get(&child.checkpoint).unwrap().clone();
    let mut zmap_path = std::env::temp_dir();
    zmap_path.push(format!(
        "checkpoint_render_splice_{}.tif",
        std::process::id()
    ));
    project::materialize_zmap(&mut project, &child_checkpoint, &zmap_path).unwrap();
    let reader = focusforge_core::zmap::ZMapReader::open(&zmap_path).unwrap();
    let mut checkpoint_idx = vec![0u16; (CROP.width * CROP.height) as usize];
    reader
        .read_tile_into(full(CROP.width, CROP.height), &mut checkpoint_idx)
        .unwrap();
    std::fs::remove_file(&zmap_path).unwrap();

    let diff_count = checkpoint_idx
        .iter()
        .zip(root.ancestor_zmap.iter())
        .filter(|(a, b)| a != b)
        .count();
    assert!(diff_count > 0, "the stroke must change at least one index");
    assert!(
        diff_count < checkpoint_idx.len(),
        "the stroke must leave at least one index unchanged"
    );

    // The independent oracle: a whole-image recomposite over the same
    // reader, masked by the Z-Map diff.
    let stack = TiffStack::open_files(&root.source_paths).unwrap();
    let sources = Sources {
        reader: &focusforge_core::align::WarpingReader::new(
            &stack,
            (FRAME_W, FRAME_H),
            SPP,
            root.transforms.clone(),
        ),
        frame_count: 3,
        samples_per_pixel: SPP,
        origin: (CROP.x, CROP.y),
    };
    let mut gathered = vec![0u16; checkpoint_idx.len() * SPP as usize];
    recomposite(
        &checkpoint_idx,
        full(CROP.width, CROP.height),
        &sources,
        &mut gathered,
    )
    .unwrap();
    let expected: Vec<u16> = (0..checkpoint_idx.len())
        .map(|i| {
            if checkpoint_idx[i] != root.ancestor_zmap[i] {
                gathered[i]
            } else {
                COMPOSITE_VALUE
            }
        })
        .collect();

    let (shape, bands) = render_to_bands(&mut project, &child.checkpoint, VIEW);
    assert_eq!(
        shape.description, "staged composite",
        "a splice's ImageDescription is its fused ancestor's, not a generic one"
    );
    let flat = flatten(&bands);
    assert_eq!(flat, expected);
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
        crop: None,
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");

    let mut project = Project::open(&project_path).unwrap();
    for (label, prepare) in [("view", VIEW), ("export", EXPORT)] {
        let (_shape, bands) = render_to_bands(&mut project, &sealed.checkpoint, prepare);
        assert!(
            bands.len() > 1,
            "{label}: the image must split into more than one band"
        );
        let mut y = 0u32;
        for (rect, samples) in &bands {
            assert_eq!(rect.x, 0, "{label}: every band is full-width");
            assert_eq!(rect.width, width);
            assert_eq!(
                rect.y, y,
                "{label}: bands arrive top-to-bottom with no gap or overlap"
            );
            assert_eq!(samples.len(), (rect.width * rect.height * spp) as usize);
            y += rect.height;
        }
        assert_eq!(y, height, "{label}: the bands cover the whole image");
    }
}

#[test]
fn export_render_is_the_zmap_gather_for_every_checkpoint() {
    let branched = seal_branched("checkpoint_render_export_gather");
    let root = &branched.root;
    let stack = TiffStack::open_files(&root.source_paths).unwrap();
    let mut project = Project::open(&root.project_path).unwrap();
    assert_eq!(project.checkpoint().id, branched.refuse_child);
    for (label, id) in [
        ("root", &root.root_id),
        ("refuse", &branched.refuse_id),
        ("root child", &branched.root_child),
        ("refuse child", &branched.refuse_child),
    ] {
        let expected = gather_oracle(&mut project, id, &stack, (FRAME_W, FRAME_H));
        assert!(
            expected.iter().all(|&v| v != COMPOSITE_VALUE),
            "{label}: the gather must differ from the stored composite everywhere"
        );
        let (shape, bands) = render_to_bands(&mut project, id, EXPORT);
        assert_eq!((shape.width, shape.height), (CROP.width, CROP.height));
        assert_eq!(shape.samples_per_pixel, SPP);
        assert_eq!(shape.description, "staged composite", "{label}");
        assert_eq!(flatten(&bands), expected, "{label}");
    }
}

#[test]
fn view_render_keeps_the_stored_composite() {
    let root = seal_root("checkpoint_render_view_keeps");
    let stored = vec![COMPOSITE_VALUE; (CROP.width * CROP.height) as usize];
    let mut project = Project::open(&root.project_path).unwrap();
    let (_, exported) = render_to_bands(&mut project, &root.root_id, EXPORT);
    assert_ne!(
        flatten(&exported),
        stored,
        "the export must not read the stored composite"
    );
    let (_, viewed) = render_to_bands(&mut project, &root.root_id, VIEW);
    assert_eq!(flatten(&viewed), stored);
}

#[test]
fn export_render_reads_raw_sources() {
    let dngs: Vec<_> = (0..3u16)
        .map(|i| {
            let samples = (0..64u16).map(|j| 64 + i * 900 + j * 7).collect();
            write_dng(&DngSpec::new(12, 64, 4095, [0, 1, 1, 2], samples), false)
        })
        .collect();
    let source_paths: Vec<PathBuf> = dngs.iter().map(|f| f.path().to_path_buf()).collect();
    let set = open_frame_set(&source_paths, HostBudget::explicit(1 << 30)).unwrap();
    let (width, height) = set.reader.dimensions();
    let spp = set.reader.stack_context().samples_per_pixel;
    assert_eq!(spp, 3, "a developed RAW frame is RGB");

    let dir = tmp_dir("checkpoint_render_raw");
    let composite_path = dir.join("composite.tif");
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
        .write_tile(
            full(width, height),
            &vec![COMPOSITE_VALUE; (width * height * spp) as usize],
        )
        .unwrap();
    writer.finish().unwrap();
    let zmap_path = dir.join("zmap.tif");
    let zmap: Vec<u16> = (0..(width * height) as usize)
        .map(|i| (i % 3) as u16)
        .collect();
    let mut map = ZMapWriter::create(&zmap_path, ZMapInfo::new(width, height, 3).unwrap()).unwrap();
    map.write_tile(full(width, height), &zmap).unwrap();
    map.finish().unwrap();

    let project_path = dir.join(format!("raw.{}", project::PROJECT_EXTENSION));
    let sealed = project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite_path,
        staged_zmap: &zmap_path,
        sources: &source_paths,
        algorithm: "weighted_average",
        params: &ParamValues::new(),
        align: None,
        crop: None,
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");

    let mut project = Project::open(&project_path).unwrap();
    let expected = gather_oracle(
        &mut project,
        &sealed.checkpoint,
        &*set.reader,
        (width, height),
    );
    let (_, bands) = render_to_bands(&mut project, &sealed.checkpoint, EXPORT);
    assert_eq!(flatten(&bands), expected);
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
