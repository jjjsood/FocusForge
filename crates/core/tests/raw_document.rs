// SPDX-License-Identifier: GPL-3.0-or-later

//! A RAW stack fuses into an `f32` document: samples above white survive the
//! fusion, the save and the reopen, every render path reads the same samples,
//! and a RAW project saved with a clipped 16-bit result is refused.

mod common;
#[path = "common/raw_dng.rs"]
mod raw_dng;

use std::path::{Path, PathBuf};

use focusforge_core::algorithm::TileReader;
use focusforge_core::align::Registration;
use focusforge_core::equalize::UNIT_GAIN;
use focusforge_core::io::RawStack;
use focusforge_core::io::TiffHeader;
use focusforge_core::io::raw::{HighlightClip, LookTransform, dng_colour, highlight_clip};
use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter};
use focusforge_core::params::ParamValues;
use focusforge_core::project::export::{
    ExportOptions, IccStatus, JpegSubsampling, MetadataOptions, MetadataStatus, PngBits,
    PngCompression, TiffBits, TiffCompression, to_8bit,
};
use focusforge_core::project::{
    self, BlobKind, CheckpointImage, HeldCheckpoint, InMemoryCheckpoint, InMemoryKind, Metrics, Op,
    PendingState, Point, Project, ProjectError, SealRequest,
};
use focusforge_core::tile::{Sample, SampleFormat};
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};
use raw_dng::{DngSpec, write_dng};
use rawler::imgop::develop::{Intermediate, ProcessingStep, RawDevelop};
use rawler::imgop::xyz::Illuminant;
use rawler::rawimage::RawPhotometricInterpretation;

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
    dng_frames_with(count, width, height, |_| {})
}

/// [`dng_frames`] with `adjust` applied to every frame's spec before it is
/// written.
fn dng_frames_with(
    count: usize,
    width: u32,
    height: u32,
    adjust: impl Fn(&mut DngSpec),
) -> Vec<tempfile::NamedTempFile> {
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
            adjust(&mut spec);
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

/// A stroked RAW checkpoint and the states that must render the same: the
/// held result, a pending state with the stroke, and a saved root with the
/// stroke appended.
struct StrokedRaw {
    frames: Vec<tempfile::NamedTempFile>,
    width: u32,
    height: u32,
    held: HeldCheckpoint,
    pending: PendingState,
    project: Project,
    edited: String,
    edited_f32: Vec<f32>,
}

fn stroked_raw(name: &str) -> StrokedRaw {
    let (width, height) = (37u32, 23u32);
    let frames = dng_frames(3, width, height);
    let dir = tmp_dir(name);
    let held = common::fuse_held(&paths(&frames), "weighted-average", false, &dir);
    let staged = read_f32(&held.composite);
    assert!(
        staged.iter().any(|&s| s > 65_535.0),
        "headroom precondition"
    );
    let project_path = dir.join(format!("saved.{}", project::PROJECT_EXTENSION));
    let root = seal_held(&project_path, &held);
    let stroke = Op::frame_stroke(
        1,
        vec![Point { x: 4, y: 11 }, Point { x: 32, y: 11 }],
        40.0,
        0.5,
    );
    let edited = project::append_ops_onto(&project_path, &root, vec![stroke.clone()], None, 1)
        .expect("the stroke must append")
        .checkpoint;
    let mut project = Project::open(&project_path).unwrap();
    let pending = PendingState {
        held: Vec::new(),
        composite: held.composite.clone(),
        ops: vec![stroke],
        sources: held.sources.clone(),
        registration: held.registration.clone(),
        reference: 0,
        description: held.composite_description.clone(),
    };
    let image = CheckpointImage::prepare(&project, &edited).unwrap();
    let edited_f32 = render_f32(&image, Some(&mut project), full(width, height));
    assert!(
        edited_f32.iter().any(|&s| s > 65_535.0),
        "the stroke's samples stay unclamped"
    );
    StrokedRaw {
        frames,
        width,
        height,
        held,
        pending,
        project,
        edited,
        edited_f32,
    }
}

fn read_u16(path: &Path) -> Vec<u16> {
    let stack = TiffStack::open_files(&[path.to_path_buf()]).unwrap();
    let (width, height) = stack.dimensions();
    let spp = stack.header(0).samples_per_pixel;
    let mut samples = vec![0u16; (width * height * spp) as usize];
    stack
        .read_tile_into(0, full(width, height), &mut samples)
        .unwrap();
    samples
}

/// The samples of an uncompressed 8-bit TIFF: its strips back to back.
fn read_strips_8bit(path: &Path) -> Vec<u8> {
    let header = TiffHeader::open(path).unwrap();
    assert_eq!(&*header.bits_per_sample, &[8, 8, 8], "{path:?}");
    let bytes = std::fs::read(path).unwrap();
    header
        .strip_offsets
        .iter()
        .zip(&header.strip_byte_counts)
        .flat_map(|(&at, &len)| bytes[at as usize..(at + len) as usize].to_vec())
        .collect()
}

/// A PNG's bit depth and decoded bytes (16-bit samples big-endian).
fn read_png(path: &Path) -> (png::BitDepth, Vec<u8>) {
    let mut reader = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()))
        .read_info()
        .unwrap();
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    (info.bit_depth, buf)
}

/// Exports through `export` in every format and checks each file against
/// `wide` clipped once to 16 bits by `clip` (and once more to 8).
fn check_exports(
    dir: &Path,
    label: &str,
    clip: &HighlightClip,
    wide: &[f32],
    (width, height): (u32, u32),
    export: &mut dyn FnMut(&Path, &ExportOptions),
) {
    let mut expect16 = vec![0u16; wide.len()];
    clip.narrow_band(wide, &mut expect16);
    let expect8: Vec<u8> = expect16.iter().map(|&v| to_8bit(v)).collect();
    assert!(
        expect16.contains(&65_535),
        "{label}: a sample at full scale"
    );

    let tiff = |bits| ExportOptions::Tiff {
        bits,
        compression: TiffCompression::None,
    };
    let png = |bits| ExportOptions::Png {
        bits,
        compression: PngCompression::Default,
    };

    let path = dir.join(format!("{label}-16.tif"));
    export(&path, &tiff(TiffBits::Sixteen));
    assert_eq!(read_u16(&path), expect16, "{label}: 16-bit TIFF");

    let path = dir.join(format!("{label}-8.tif"));
    export(&path, &tiff(TiffBits::Eight));
    assert_eq!(read_strips_8bit(&path), expect8, "{label}: 8-bit TIFF");

    let path = dir.join(format!("{label}-16.png"));
    export(&path, &png(PngBits::Sixteen));
    let (depth, bytes) = read_png(&path);
    assert_eq!(depth, png::BitDepth::Sixteen, "{label}");
    let samples: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .collect();
    assert_eq!(samples, expect16, "{label}: 16-bit PNG");

    let path = dir.join(format!("{label}-8.png"));
    export(&path, &png(PngBits::Eight));
    let (depth, bytes) = read_png(&path);
    assert_eq!(depth, png::BitDepth::Eight, "{label}");
    assert_eq!(bytes, expect8, "{label}: 8-bit PNG");

    // With no metadata the JPEG is the 8-bit samples through the encoder.
    let path = dir.join(format!("{label}.jpg"));
    export(
        &path,
        &ExportOptions::Jpeg {
            quality: 90,
            subsampling: JpegSubsampling::S444,
        },
    );
    let mut expected = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut expected, 90);
    encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
    encoder
        .encode(
            &expect8,
            width as u16,
            height as u16,
            jpeg_encoder::ColorType::Rgb,
        )
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), expected, "{label}: JPEG");
}

#[test]
fn raw_exports_clip_once() {
    let mut raw = stroked_raw("raw_document_exports");
    let dir = tmp_dir("raw_document_exports_out");
    let size = (raw.width, raw.height);
    let meta = MetadataOptions {
        exif: false,
        icc: false,
        xmp: false,
        iptc: false,
    };

    let clip = highlight_clip(&paths(&raw.frames)[0], &[])
        .unwrap()
        .expect("a colour RAW source has a clip");
    let staged = read_f32(&raw.held.composite);
    let held = raw.held.clone();
    check_exports(&dir, "held", &clip, &staged, size, &mut |path, options| {
        project::export_held(&held, path, options, &meta).unwrap();
    });

    let pending = raw.pending.clone();
    let wide = render_f32(
        &CheckpointImage::prepare_pending(&pending, None).unwrap(),
        None,
        full(size.0, size.1),
    );
    assert_eq!(bits(&wide), bits(&raw.edited_f32), "pending equals saved");
    check_exports(&dir, "pending", &clip, &wide, size, &mut |path, options| {
        project::export_pending(&pending, None, path, options, &meta).unwrap();
    });

    let wide = raw.edited_f32.clone();
    let edited = raw.edited.clone();
    check_exports(&dir, "saved", &clip, &wide, size, &mut |path, options| {
        project::export_checkpoint(&mut raw.project, &edited, path, options, &meta).unwrap();
    });
}

#[test]
fn raw_view_render_keeps_headroom() {
    let mut raw = stroked_raw("raw_document_view");
    let dir = tmp_dir("raw_document_view_out");
    let view = dir.join("view.tif");
    project::render_checkpoint(&mut raw.project, &raw.edited, &view, None).unwrap();
    assert_eq!(
        bits(&read_f32(&view)),
        bits(&raw.edited_f32),
        "the view render is the f32 render"
    );

    let capped = dir.join("capped.tif");
    let err = project::render_checkpoint(&mut raw.project, &raw.edited, &capped, Some(1))
        .expect_err("a cap below the decode reservation refuses the render");
    assert!(
        err.to_string().contains("memory limit allows"),
        "the refusal names the cap: {err}"
    );
    assert!(!capped.exists(), "a refused render leaves no file");
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
        let clip = highlight_clip(&paths(&frames)[0], &[])
            .unwrap()
            .expect("a colour RAW source has a clip");
        let mut clipped = vec![0u16; staged.len()];
        clip.narrow_band(&staged, &mut clipped);
        assert_eq!(
            render_u16(&image, Some(&mut project), tile),
            clipped,
            "{label}: the 16-bit render clips the f32 one"
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
    let stroke = Op::frame_stroke(
        1,
        vec![Point { x: 4, y: 11 }, Point { x: 32, y: 11 }],
        40.0,
        0.5,
    );
    let edited = project::append_ops_onto(&project_path, &root, vec![stroke.clone()], None, 1)
        .expect("the stroke must append")
        .checkpoint;
    let mut project = Project::open(&project_path).unwrap();

    let pending = PendingState {
        held: Vec::new(),
        composite: held.composite.clone(),
        ops: Vec::new(),
        sources: held.sources.clone(),
        registration: held.registration.clone(),
        reference: 0,
        description: held.composite_description.clone(),
    };
    let pending_stroked = PendingState {
        held: Vec::new(),
        ops: vec![stroke],
        ..pending.clone()
    };
    let held_image = CheckpointImage::prepare_held(&held).unwrap();
    let pending_image = CheckpointImage::prepare_pending(&pending, None).unwrap();
    let pending_stroked_image = CheckpointImage::prepare_pending(&pending_stroked, None).unwrap();
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
        let samples: Vec<f32> = (0..len).map(|i| i as f32 * 0.37 % 90_000.0).collect();
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

// --- linear DNG export -------------------------------------------------------

const MATRIX_D65: [f32; 9] = [0.9, 0.05, -0.1, -0.2, 1.1, 0.08, 0.02, -0.15, 0.8];
const MATRIX_A: [f32; 9] = [1.2, -0.1, 0.0, -0.3, 1.2, 0.1, 0.05, -0.2, 0.9];
const CALIBRATION_D65: [f32; 9] = [1.0122, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.9778];
const CALIBRATION_A: [f32; 9] = [1.05, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.95];
const ANALOG_BALANCE: [f32; 3] = [1.02, 1.0, 0.97];
const ILLUMINANT_A: u16 = 17;
const ILLUMINANT_D65: u16 = 21;
const CAPTURE_TIME: &str = "2026:10:30 12:00:00";

fn colour_frames(count: usize, width: u32, height: u32) -> Vec<tempfile::NamedTempFile> {
    dng_frames_with(count, width, height, |spec| {
        spec.color_matrix = Some(MATRIX_D65);
        spec.orientation = Some(6);
        spec.capture_time = Some(CAPTURE_TIME.to_string());
    })
}

/// One entry of a little-endian TIFF directory: its field type, count and
/// value bytes, offsets already followed.
struct DirEntry {
    tag: u16,
    kind: u16,
    count: u32,
    value: Vec<u8>,
}

impl DirEntry {
    fn shorts(&self) -> Vec<u16> {
        self.value
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect()
    }

    fn longs(&self) -> Vec<u32> {
        self.value
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect()
    }

    fn ascii(&self) -> String {
        String::from_utf8_lossy(&self.value)
            .trim_end_matches('\0')
            .to_string()
    }

    /// RATIONAL or SRATIONAL values as `f64`.
    fn rationals(&self) -> Vec<f64> {
        self.value
            .chunks_exact(8)
            .map(|b| {
                let numerator = [b[0], b[1], b[2], b[3]];
                let denominator = f64::from(u32::from_le_bytes([b[4], b[5], b[6], b[7]]));
                if self.kind == 10 {
                    f64::from(i32::from_le_bytes(numerator)) / denominator
                } else {
                    f64::from(u32::from_le_bytes(numerator)) / denominator
                }
            })
            .collect()
    }
}

struct DngFile {
    bytes: Vec<u8>,
    ifd0: Vec<DirEntry>,
    /// The offset of the directory after IFD0; 0 for none.
    next_ifd: u32,
}

fn read_dir(bytes: &[u8], at: usize) -> (Vec<DirEntry>, u32) {
    assert_eq!(&bytes[..2], b"II");
    let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    let count = u16::from_le_bytes([bytes[at], bytes[at + 1]]) as usize;
    let entries = (0..count)
        .map(|i| {
            let e = at + 2 + i * 12;
            let tag = u16::from_le_bytes([bytes[e], bytes[e + 1]]);
            let kind = u16::from_le_bytes([bytes[e + 2], bytes[e + 3]]);
            let count = word(e + 4);
            let size = match kind {
                3 => 2,
                4 => 4,
                5 | 10 => 8,
                _ => 1,
            } * count as usize;
            let start = if size <= 4 {
                e + 8
            } else {
                word(e + 8) as usize
            };
            DirEntry {
                tag,
                kind,
                count,
                value: bytes[start..start + size].to_vec(),
            }
        })
        .collect();
    (entries, word(at + 2 + count * 12))
}

fn read_dng(path: &Path) -> DngFile {
    let bytes = std::fs::read(path).unwrap();
    let ifd0_at = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let (ifd0, next_ifd) = read_dir(&bytes, ifd0_at);
    DngFile {
        bytes,
        ifd0,
        next_ifd,
    }
}

impl DngFile {
    fn tag(&self, tag: u16) -> Option<&DirEntry> {
        self.ifd0.iter().find(|e| e.tag == tag)
    }

    fn need(&self, tag: u16) -> &DirEntry {
        self.tag(tag)
            .unwrap_or_else(|| panic!("IFD0 carries tag {tag}"))
    }

    fn exif_ifd(&self) -> Option<Vec<DirEntry>> {
        let at = self.tag(34665)?.longs()[0] as usize;
        Some(read_dir(&self.bytes, at).0)
    }

    fn samples(&self) -> Vec<u16> {
        let offsets = self.need(273).longs();
        let counts = self.need(279).longs();
        offsets
            .iter()
            .zip(&counts)
            .flat_map(|(&at, &len)| self.bytes[at as usize..(at + len) as usize].chunks_exact(2))
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect()
    }
}

fn dng_meta(exif: bool) -> MetadataOptions {
    MetadataOptions {
        exif,
        icc: true,
        xmp: true,
        iptc: true,
    }
}

fn assert_close(got: &[f64], want: &[f64], what: &str) {
    assert_eq!(got.len(), want.len(), "{what}");
    for (g, w) in got.iter().zip(want) {
        assert!((g - w).abs() < 1e-4, "{what}: {got:?} vs {want:?}");
    }
}

fn f64s(values: &[f32]) -> Vec<f64> {
    values.iter().map(|&v| f64::from(v)).collect()
}

/// `wide` (look values, three per pixel) carried back through `look`'s
/// inverse, clipped to the sensor range and rounded to 16 bits.
fn camera_16bit(look: &LookTransform, wide: &[f32]) -> Vec<u16> {
    wide.chunks_exact(3)
        .flat_map(|px| {
            let camera = look.inverse([0, 1, 2].map(|c| f64::from(px[c]) / 65_535.0));
            camera.map(|c| (c.clamp(0.0, 1.0) * 65_535.0).round() as u16)
        })
        .collect()
}

fn assert_within(got: &[u16], want: &[u16], tolerance: u16, what: &str) {
    assert_eq!(got.len(), want.len(), "{what}");
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        assert!(
            g.abs_diff(*w) <= tolerance,
            "{what}: sample {i}: {g} vs {w}"
        );
    }
}

fn check_dng_tags(label: &str, second: Option<([f32; 9], u16)>, exif: bool, calibrated: bool) {
    let frames = dng_frames_with(3, 24, 16, |spec| {
        spec.color_matrix = Some(MATRIX_D65);
        spec.color_matrix2 = second;
        if calibrated {
            spec.camera_calibration = [Some(CALIBRATION_D65), second.map(|_| CALIBRATION_A)];
            spec.analog_balance = Some(ANALOG_BALANCE);
        }
        spec.orientation = Some(6);
        spec.capture_time = Some(CAPTURE_TIME.to_string());
    });
    let dir = tmp_dir(&format!("raw_document_dng_tags_{label}"));
    let held = common::fuse_held(&paths(&frames), "weighted-average", false, &dir);
    let out = dir.join("out.dng");
    let outcome =
        project::export_held(&held, &out, &ExportOptions::Dng, &dng_meta(exif)).expect("DNG");
    assert_eq!(outcome.icc, IccStatus::Unsupported, "{label}");
    assert_eq!(outcome.xmp, MetadataStatus::Absent, "{label}");
    assert_eq!(outcome.iptc, MetadataStatus::Absent, "{label}");

    let dng = read_dng(&out);
    assert_eq!(dng.next_ifd, 0, "{label}: one IFD");
    assert_eq!(
        dng.ifd0.iter().filter(|e| e.tag == 262).count(),
        1,
        "{label}"
    );
    assert_eq!(dng.need(254).longs(), [0], "{label}: NewSubFileType");
    assert!(
        dng.ifd0.windows(2).all(|w| w[0].tag < w[1].tag),
        "{label}: IFD0 ascending"
    );
    assert_eq!(dng.need(262).shorts(), [34892], "{label}");
    assert_eq!(dng.need(277).shorts(), [3], "{label}");
    assert_eq!(dng.need(258).shorts(), [16, 16, 16], "{label}");
    assert_eq!(dng.need(50706).value, [1, 4, 0, 0], "{label}: DNGVersion");
    assert!(dng.need(50714).shorts().iter().all(|&b| b == 0), "{label}");
    assert!(
        dng.need(50717).longs().iter().all(|&w| w == 65_535),
        "{label}"
    );
    assert!(dng.tag(34675).is_none(), "{label}: no ICC profile");

    let source = rawler::decode_file(frames[0].path()).unwrap();
    let wb = source.wb_coeffs;
    let neutral: Vec<f64> = (0..3).map(|c| f64::from(wb[1] / wb[c])).collect();
    assert_close(&dng.need(50728).rationals(), &neutral, label);

    let mut expected = vec![(MATRIX_D65, ILLUMINANT_D65)];
    expected.extend(second);
    expected.sort_by_key(|&(_, illuminant)| illuminant != ILLUMINANT_A);
    let calibration_of = |illuminant: u16| match illuminant {
        ILLUMINANT_D65 => CALIBRATION_D65,
        _ => CALIBRATION_A,
    };
    for (slot, (_, illuminant)) in expected.iter().enumerate() {
        let tag = dng.tag(50723 + slot as u16);
        match (calibrated, tag) {
            (true, Some(entry)) => assert_close(
                &entry.rationals(),
                &f64s(&calibration_of(*illuminant)),
                label,
            ),
            (false, None) => {}
            (on, found) => panic!(
                "{label}: calibrated {on}, CameraCalibration{} present {}",
                slot + 1,
                found.is_some()
            ),
        }
    }
    if expected.len() == 1 {
        assert!(
            dng.tag(50724).is_none(),
            "{label}: one slot, one calibration"
        );
    }
    match (calibrated, dng.tag(50727)) {
        (true, Some(entry)) => assert_close(&entry.rationals(), &f64s(&ANALOG_BALANCE), label),
        (false, None) => {}
        (on, found) => panic!(
            "{label}: calibrated {on}, AnalogBalance present {}",
            found.is_some()
        ),
    }
    assert_eq!(dng.need(50721).count, 9, "{label}");
    assert_close(&dng.need(50721).rationals(), &f64s(&expected[0].0), label);
    assert_eq!(dng.need(50778).shorts(), [expected[0].1], "{label}");
    match expected.get(1) {
        Some((matrix, illuminant)) => {
            assert_close(&dng.need(50722).rationals(), &f64s(matrix), label);
            assert_eq!(dng.need(50779).shorts(), [*illuminant], "{label}");
        }
        None => {
            assert!(
                dng.tag(50722).is_none() && dng.tag(50779).is_none(),
                "{label}"
            );
        }
    }

    assert_eq!(dng.need(50708).ascii(), "FocusForge Test Synthetic Bayer");
    assert_eq!(dng.need(271).ascii(), "FocusForge Test", "{label}");
    assert_eq!(dng.need(272).ascii(), "Synthetic Bayer", "{label}");
    assert_eq!(dng.need(274).shorts(), [6], "{label}: Orientation");
    match (exif, dng.exif_ifd()) {
        (true, Some(entries)) => {
            let taken = entries
                .iter()
                .find(|e| e.tag == 36867)
                .expect("capture time");
            assert_eq!(taken.ascii(), CAPTURE_TIME, "{label}");
        }
        (false, None) => {}
        (on, found) => panic!("{label}: exif {on}, EXIF IFD present {}", found.is_some()),
    }

    let image = rawler::decode_file(&out).expect("a converter's reader opens the DNG");
    assert_eq!(image.photometric, RawPhotometricInterpretation::LinearRaw);
    assert_eq!(image.cpp, 3, "{label}");
    assert_close(
        &f64s(&image.wb_coeffs[..3]),
        &f64s(&source.wb_coeffs[..3]),
        label,
    );
    for (matrix, illuminant) in &expected {
        let illuminant = Illuminant::try_from(*illuminant).unwrap();
        assert_close(
            &f64s(&image.color_matrix[&illuminant]),
            &f64s(matrix),
            label,
        );
    }
}

#[test]
fn dng_tags() {
    for exif in [true, false] {
        for calibrated in [true, false] {
            let label = format!("{exif}_{calibrated}");
            check_dng_tags(&format!("one_{label}"), None, exif, calibrated);
            let second = Some((MATRIX_A, ILLUMINANT_A));
            check_dng_tags(&format!("two_{label}"), second, exif, calibrated);
        }
    }
}

/// Rawler's camera-linear development of `path`, three samples per pixel.
fn camera_linear(path: &Path) -> Vec<f32> {
    let image = rawler::decode_file(path).unwrap();
    let steps = [
        ProcessingStep::Rescale,
        ProcessingStep::Demosaic,
        ProcessingStep::FujiRotate,
        ProcessingStep::CropActiveArea,
        ProcessingStep::CropDefault,
    ];
    match RawDevelop::new_with(&steps)
        .develop_intermediate(&image)
        .unwrap()
    {
        Intermediate::ThreeColor(pixels) => pixels.data.into_iter().flatten().collect(),
        _ => panic!("a colour frame develops to three channels"),
    }
}

#[test]
fn dng_identical_frames_round_trip() {
    let (width, height) = (24u32, 16u32);
    let first = colour_frames(1, width, height).remove(0);
    let dir = tmp_dir("raw_document_dng_identical");
    let sources: Vec<PathBuf> = (0..3)
        .map(|i| {
            let copy = dir.join(format!("frame-{i}.dng"));
            std::fs::copy(first.path(), &copy).unwrap();
            copy
        })
        .collect();
    let held = common::fuse_held(&sources, "weighted-average", false, &dir);
    let out = dir.join("identical.dng");
    project::export_held(&held, &out, &ExportOptions::Dng, &dng_meta(true)).unwrap();

    let expected: Vec<u16> = camera_linear(&sources[0])
        .iter()
        .map(|&v| (f64::from(v) * 65_535.0).round().clamp(0.0, 65_535.0) as u16)
        .collect();
    assert_within(
        &read_dng(&out).samples(),
        &expected,
        16,
        "camera-linear samples",
    );

    let source = RawStack::open_files(&sources[..1]).unwrap();
    let developed = RawStack::open_files(&[out]).unwrap();
    let rect = full(width, height);
    let mut want = vec![0f32; (width * height * 3) as usize];
    let mut got = want.clone();
    source.read_tile_f32_into(0, rect, &mut want).unwrap();
    developed.read_tile_f32_into(0, rect, &mut got).unwrap();
    assert!(
        want.iter().any(|&v| v > 65_535.0),
        "a look value above white"
    );
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert!((g - w).abs() < 65.5, "look sample {i}: {g} vs {w}");
    }
}

#[test]
fn dng_same_document_as_render() {
    let mut raw = stroked_raw("raw_document_dng_same");
    let dir = tmp_dir("raw_document_dng_same_out");
    let size = full(raw.width, raw.height);
    let look = dng_colour(raw.frames[0].path()).unwrap().look;
    let meta = dng_meta(true);

    let held_path = dir.join("held.dng");
    project::export_held(&raw.held, &held_path, &ExportOptions::Dng, &meta).unwrap();
    let held = read_dng(&held_path).samples();
    let staged = read_f32(&raw.held.composite);
    assert_within(&held, &camera_16bit(&look, &staged), 1, "held");

    let pending_path = dir.join("pending.dng");
    project::export_pending(
        &raw.pending,
        None,
        &pending_path,
        &ExportOptions::Dng,
        &meta,
    )
    .unwrap();
    let pending_wide = render_f32(
        &CheckpointImage::prepare_pending(&raw.pending, None).unwrap(),
        None,
        size,
    );
    assert_within(
        &read_dng(&pending_path).samples(),
        &camera_16bit(&look, &pending_wide),
        1,
        "pending with a stroke",
    );

    let saved_path = dir.join("saved.dng");
    project::export_checkpoint(
        &mut raw.project,
        &raw.edited,
        &saved_path,
        &ExportOptions::Dng,
        &meta,
    )
    .unwrap();
    let saved = read_dng(&saved_path).samples();
    assert_within(
        &saved,
        &camera_16bit(&look, &raw.edited_f32),
        1,
        "saved with a stroke",
    );
    assert_ne!(saved, held, "the stroke changes the DNG");
}

#[test]
fn dng_keeps_highlights() {
    let raw = stroked_raw("raw_document_dng_highlights");
    let dir = tmp_dir("raw_document_dng_highlights_out");
    let look = dng_colour(raw.frames[0].path()).unwrap().look;
    let path = dir.join("held.dng");
    project::export_held(&raw.held, &path, &ExportOptions::Dng, &dng_meta(true)).unwrap();
    let dng = read_dng(&path).samples();
    let tiff = narrowed(&read_f32(&raw.held.composite));

    let kept = (0..tiff.len()).any(|i| {
        let pixel = &dng[i / 3 * 3..i / 3 * 3 + 3];
        let camera = [0, 1, 2].map(|c| f64::from(pixel[c]) / 65_535.0);
        tiff[i] == 65_535 && dng[i] < 65_535 && look.forward(camera)[i % 3] > 1.0
    });
    assert!(kept, "a sample the TIFF clips is below white in the DNG");
}

#[test]
fn dng_streams_in_bands() {
    let (width, height) = (512u32, 384u32);
    let frames = colour_frames(2, width, height);
    let dir = tmp_dir("raw_document_dng_bands");
    let project_path = seal_over(&dir, &paths(&frames), (width, height), true);
    let mut project = Project::open(&project_path).unwrap();
    let id = project.history().all()[0].id.clone();
    let out = dir.join("bands.dng");
    project::export_checkpoint(
        &mut project,
        &id,
        &out,
        &ExportOptions::Dng,
        &dng_meta(true),
    )
    .unwrap();

    let dng = read_dng(&out);
    assert!(dng.need(273).count > 1, "more than one strip");
    let look = dng_colour(frames[0].path()).unwrap().look;
    let image = CheckpointImage::prepare(&project, &id).unwrap();
    let wide = render_f32(&image, Some(&mut project), full(width, height));
    assert_within(&dng.samples(), &camera_16bit(&look, &wide), 1, "bands");
}

fn assert_nothing_written(dir: &Path) {
    let left: Vec<_> = std::fs::read_dir(dir).unwrap().collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

#[test]
fn dng_refused() {
    let (width, height) = (16u32, 12u32);
    let out_dir = tmp_dir("raw_document_dng_refused_out");
    let out = out_dir.join("refused.dng");

    let tiff_dir = tmp_dir("raw_document_dng_refused_tiff");
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
    let mut project = Project::open(&tiff).unwrap();
    let id = project.history().all()[0].id.clone();
    let error = project::export_checkpoint(
        &mut project,
        &id,
        &out,
        &ExportOptions::Dng,
        &dng_meta(true),
    )
    .expect_err("a TIFF project has no camera space");
    assert!(
        matches!(error, ProjectError::ExportDngNeedsRaw { .. }),
        "{error}"
    );
    assert!(error.to_string().contains(out.to_str().unwrap()), "{error}");
    assert_nothing_written(&out_dir);

    type Adjust = fn(&mut DngSpec);
    let cases: [(&str, Adjust, &str); 3] = [
        ("monochrome", |spec| spec.monochrome = true, "monochrome"),
        (
            "no_matrix",
            |spec| spec.omit_color_matrix = true,
            "colour matrix",
        ),
        (
            "no_white_balance",
            |spec| spec.as_shot_neutral = None,
            "white balance",
        ),
    ];
    for (label, adjust, reason) in cases {
        let frames = dng_frames_with(2, width, height, adjust);
        let dir = tmp_dir(&format!("raw_document_dng_refused_{label}"));
        let sealed = seal_over(&dir, &paths(&frames), (width, height), true);
        let mut project = Project::open(&sealed).unwrap();
        let id = project.history().all()[0].id.clone();
        let error = project::export_checkpoint(
            &mut project,
            &id,
            &out,
            &ExportOptions::Dng,
            &dng_meta(true),
        )
        .expect_err(label);
        assert!(
            matches!(error, ProjectError::ExportDngColour { .. }),
            "{label}: {error}"
        );
        let text = error.to_string();
        assert!(text.contains(reason), "{label}: {text}");
        assert!(text.contains(out.to_str().unwrap()), "{label}: {text}");
        assert_nothing_written(&out_dir);
    }

    let frames = colour_frames(2, width, height);
    let dir = tmp_dir("raw_document_dng_refused_unwritable");
    let held = common::fuse_held(&paths(&frames), "weighted-average", false, &dir);
    let unwritable = dir.join("missing").join("out.dng");
    let error = project::export_held(&held, &unwritable, &ExportOptions::Dng, &dng_meta(true))
        .expect_err("the directory does not exist");
    assert!(
        matches!(error, ProjectError::ExportWriteFailed { .. }),
        "{error}"
    );
    assert!(
        error.to_string().contains(unwritable.to_str().unwrap()),
        "{error}"
    );
}

/// Columns and rows of the 8x8 patch at sensor white in [`patch_frames`].
const PATCH_X: std::ops::Range<u32> = 5..13;
const PATCH_Y: std::ops::Range<u32> = 4..12;

/// Writes `[red, green, blue]` sensor values into the RGGB mosaic over
/// `xs` by `ys`.
fn fill_patch(
    spec: &mut DngSpec,
    xs: std::ops::Range<u32>,
    ys: std::ops::Range<u32>,
    [red, green, blue]: [u16; 3],
) {
    for y in ys {
        for x in xs.clone() {
            spec.samples[(y * spec.width + x) as usize] =
                [red, green, blue][(x % 2 + y % 2) as usize];
        }
    }
}

fn white_patch(spec: &mut DngSpec) {
    fill_patch(spec, PATCH_X, PATCH_Y, [4095; 3]);
}

/// `count` textured 37x23 frames with the white patch at sensor saturation.
fn patch_frames(count: usize) -> Vec<tempfile::NamedTempFile> {
    dng_frames_with(count, 37, 23, white_patch)
}

/// Pixels two or more pixels inside the patch.
fn deep_in_patch(x: u32, y: u32) -> bool {
    deep_in(PATCH_X, PATCH_Y, x, y)
}

/// Pixels two or more pixels inside the patch spanning `xs` by `ys`.
fn deep_in(xs: std::ops::Range<u32>, ys: std::ops::Range<u32>, x: u32, y: u32) -> bool {
    (xs.start + 2..xs.end - 2).contains(&x) && (ys.start + 2..ys.end - 2).contains(&y)
}

/// Pixels two or more pixels outside the patch and clear of the last two rows
/// and columns, where the demosaic of an odd-sized frame extrapolates samples
/// thousands of times above white.
fn far_from_patch(x: u32, y: u32, (width, height): (u32, u32)) -> bool {
    !((PATCH_X.start - 1..PATCH_X.end + 1).contains(&x)
        && (PATCH_Y.start - 1..PATCH_Y.end + 1).contains(&y))
        && x < width - 2
        && y < height - 2
}

fn is_neutral(rgb: &[u16]) -> bool {
    rgb[0].abs_diff(rgb[1]) <= 1 && rgb[1].abs_diff(rgb[2]) <= 1
}

#[test]
fn raw_highlight_clip_document() {
    let (width, height) = (37u32, 23u32);
    let frames = patch_frames(3);
    let dir = tmp_dir("raw_highlight_clip_document");
    let held = common::fuse_held(&paths(&frames), "weighted-average", false, &dir);
    let samples = read_f32(&held.composite);
    let clip = highlight_clip(&paths(&frames)[0], &[])
        .unwrap()
        .expect("a colour RAW source has a clip");
    let mut clipped = vec![0u16; samples.len()];
    clip.narrow_band(&samples, &mut clipped);
    let plain = narrowed(&samples);

    let (mut inside, mut outside, mut above_white) = (0, 0, 0);
    for y in 0..height {
        for x in 0..width {
            let at = ((y * width + x) * 3) as usize..((y * width + x) * 3 + 3) as usize;
            if deep_in_patch(x, y) {
                inside += 1;
                assert!(
                    is_neutral(&clipped[at.clone()]),
                    "({x}, {y}): {:?}",
                    &clipped[at]
                );
            } else if far_from_patch(x, y, (width, height)) {
                outside += 1;
                above_white += samples[at.clone()]
                    .iter()
                    .filter(|&&s| s > 65_535.0)
                    .count();
                assert_eq!(&clipped[at.clone()], &plain[at], "({x}, {y})");
            }
        }
    }
    assert!(inside > 0 && outside > 0);
    assert!(
        above_white > 0,
        "the texture must carry samples above white"
    );
}

/// A patch whose green stops at 99 % of the white level (4055 = 64 + 0.99 *
/// 4031) is clipped; a patch with green at 97 % (3974) and red well below it,
/// and the texture, stay as `u16::narrow` leaves them. A mixed-value mosaic
/// demosaics to camera values that differ with pixel parity, so neutrality of
/// the first patch is shown at the look level.
#[test]
fn raw_highlight_clip_below_white_level() {
    let (width, height) = (37u32, 23u32);
    let second_x = 20..28;
    let frames = dng_frames_with(3, width, height, |spec| {
        fill_patch(spec, PATCH_X, PATCH_Y, [4055, 4055, 4000]);
        fill_patch(spec, second_x.clone(), PATCH_Y, [2700, 3974, 3974]);
    });
    let dir = tmp_dir("raw_highlight_clip_below_white_level");
    let held = common::fuse_held(&paths(&frames), "weighted-average", false, &dir);
    let samples = read_f32(&held.composite);
    let clip = highlight_clip(&paths(&frames)[0], &[])
        .unwrap()
        .expect("a colour RAW source has a clip");
    let mut clipped = vec![0u16; samples.len()];
    clip.narrow_band(&samples, &mut clipped);
    let plain = narrowed(&samples);

    let near_second = |x: u32, y: u32| {
        (second_x.start - 1..second_x.end + 1).contains(&x)
            && (PATCH_Y.start - 1..PATCH_Y.end + 1).contains(&y)
    };
    let (mut at_99, mut at_97, mut texture) = (0, 0, 0);
    for y in 0..height {
        for x in 0..width {
            let at = ((y * width + x) * 3) as usize..((y * width + x) * 3 + 3) as usize;
            if deep_in_patch(x, y) {
                at_99 += 1;
                assert_ne!(&clipped[at.clone()], &plain[at], "({x}, {y})");
            } else if deep_in(second_x.clone(), PATCH_Y, x, y) {
                at_97 += 1;
                assert_eq!(&clipped[at.clone()], &plain[at], "({x}, {y})");
            } else if far_from_patch(x, y, (width, height)) && !near_second(x, y) {
                texture += 1;
                assert_eq!(&clipped[at.clone()], &plain[at], "({x}, {y})");
            }
        }
    }
    assert!(at_99 > 0 && at_97 > 0 && texture > 0);
}

#[test]
fn raw_highlight_clip_equalized() {
    let (width, height) = (37u32, 23u32);
    let frames = patch_frames(3);
    let sources = paths(&frames);
    let stack = RawStack::open_files(&sources).unwrap();
    let gains = [
        (0.82 * f64::from(UNIT_GAIN)) as u32,
        UNIT_GAIN,
        (1.2 * f64::from(UNIT_GAIN)) as u32,
    ];
    let frame_samples: Vec<Vec<f32>> = gains
        .iter()
        .enumerate()
        .map(|(frame, &gain)| {
            let mut out = vec![0f32; (width * height * 3) as usize];
            stack
                .read_tile_gained_f32_into(frame, full(width, height), gain, &mut out)
                .unwrap();
            out
        })
        .collect();

    let patch_of = |clip: &focusforge_core::io::raw::HighlightClip, frame: usize| {
        let samples = &frame_samples[frame];
        let mut clipped = vec![0u16; samples.len()];
        clip.narrow_band(samples, &mut clipped);
        let plain = narrowed(samples);
        let mut pixels = Vec::new();
        for y in 0..height {
            for x in 0..width {
                if deep_in_patch(x, y) {
                    let at = ((y * width + x) * 3) as usize;
                    pixels.push((clipped[at..at + 3].to_vec(), plain[at..at + 3].to_vec()));
                }
            }
        }
        assert!(!pixels.is_empty());
        pixels
    };

    let recorded = highlight_clip(&sources[0], &gains).unwrap().unwrap();
    for frame in 0..3 {
        for (clipped, _) in patch_of(&recorded, frame) {
            assert!(is_neutral(&clipped), "frame {frame}: {clipped:?}");
        }
    }

    let unity = highlight_clip(&sources[0], &[]).unwrap().unwrap();
    for frame in [1, 2] {
        for (clipped, _) in patch_of(&unity, frame) {
            assert!(is_neutral(&clipped), "frame {frame}: {clipped:?}");
        }
    }
    for (clipped, plain) in patch_of(&unity, 0) {
        assert_eq!(
            clipped, plain,
            "the 0.82 frame stays below a saturation point of 1"
        );
        assert!(
            !is_neutral(&plain),
            "the unclipped patch is tinted: {plain:?}"
        );
    }
}

#[test]
fn raw_highlight_clip_context() {
    let dir = tmp_dir("raw_highlight_clip_context");
    let tiff = dir.join("frame.tif");
    let mut writer = TiffWriter::create(
        &tiff,
        TiffSpec {
            width: 4,
            height: 4,
            samples_per_pixel: 3,
            description: None,
        },
    )
    .unwrap();
    writer.write_tile(full(4, 4), &[1_000u16; 48]).unwrap();
    writer.finish().unwrap();
    assert!(highlight_clip(&tiff, &[]).unwrap().is_none(), "TIFF");
    assert!(
        highlight_clip(&dir.join("absent.jpg"), &[])
            .unwrap()
            .is_none(),
        "a JPEG path is classified without reading it"
    );

    let monochrome = dng_frames_with(1, 8, 8, |spec| spec.monochrome = true);
    assert!(
        highlight_clip(monochrome[0].path(), &[]).unwrap().is_none(),
        "monochrome"
    );

    let tagless = dng_frames_with(1, 8, 8, |spec| {
        spec.as_shot_neutral = None;
        spec.omit_color_matrix = true;
    });
    let clip = highlight_clip(tagless[0].path(), &[])
        .unwrap()
        .expect("a colour DNG with no colour tags still clips");
    let saturated = [90_000.0, 80_000.0, 70_000.0];
    let clipped = clip.narrow(saturated);
    assert!(is_neutral(&clipped), "{clipped:?}");

    let missing = dir.join("absent.dng");
    assert!(highlight_clip(&missing, &[]).is_err(), "a missing RAW file");
}

fn tiff16() -> ExportOptions {
    ExportOptions::Tiff {
        bits: TiffBits::Sixteen,
        compression: TiffCompression::None,
    }
}

const NO_METADATA: MetadataOptions = MetadataOptions {
    exif: false,
    icc: false,
    xmp: false,
    iptc: false,
};

#[test]
fn raw_exports_clip_highlight() {
    let (width, height) = (37u32, 23u32);
    let frames = patch_frames(3);
    let dir = tmp_dir("raw_exports_clip_highlight");
    let held = common::fuse_held(&paths(&frames), "weighted-average", false, &dir);
    let staged = read_f32(&held.composite);
    let plain = narrowed(&staged);
    let out = dir.join("held.tif");
    project::export_held(&held, &out, &tiff16(), &NO_METADATA).unwrap();
    let exported = read_u16(&out);

    let (mut inside, mut outside, mut tinted_by_clamp) = (0, 0, 0);
    for y in 0..height {
        for x in 0..width {
            let at = ((y * width + x) * 3) as usize..((y * width + x) * 3 + 3) as usize;
            if deep_in_patch(x, y) {
                inside += 1;
                assert!(is_neutral(&exported[at.clone()]), "({x}, {y})");
                tinted_by_clamp += usize::from(!is_neutral(&plain[at]));
            } else if far_from_patch(x, y, (width, height)) {
                outside += 1;
                assert_eq!(&exported[at.clone()], &plain[at], "({x}, {y})");
            }
        }
    }
    assert!(inside > 0 && outside > 0);
    assert!(
        tinted_by_clamp > 0,
        "the per-channel clamp must leave a cast the clip removes"
    );
}

/// What the observer keeps of a run: the last composite preview delivered.
struct CompositePreviews(std::sync::Mutex<Option<focusforge_core::display::StagePreview>>);

impl focusforge_core::pipeline::StageObserver for CompositePreviews {
    fn preview_kinds(&self) -> focusforge_core::display::PreviewKinds {
        focusforge_core::display::PreviewKinds {
            composite: true,
            ..Default::default()
        }
    }

    fn preview(&self, preview: &focusforge_core::display::StagePreview) {
        *self.0.lock().unwrap() = Some(preview.clone());
    }
}

fn read_composite(project: &mut Project) -> Vec<u16> {
    let mut samples = Vec::new();
    project
        .read_plane_into(project::Plane::Composite, &mut |band, _| {
            samples.extend_from_slice(band);
        })
        .unwrap();
    samples
}

#[test]
fn raw_display_matches_export() {
    let (width, height) = (37u32, 23u32);
    let frames = patch_frames(3);
    let dir = tmp_dir("raw_display_matches_export");
    let observer = CompositePreviews(Default::default());
    let held =
        common::fuse_held_observed(&paths(&frames), "weighted-average", false, &dir, &observer);
    let out = dir.join("held.tif");
    project::export_held(&held, &out, &tiff16(), &NO_METADATA).unwrap();
    let export16 = read_u16(&out);

    let project_path = dir.join(format!("saved.{}", project::PROJECT_EXTENSION));
    seal_held(&project_path, &held);
    let mut project = Project::open(&project_path).unwrap();
    assert_eq!(read_composite(&mut project), export16, "plane read");

    let preview = observer
        .0
        .lock()
        .unwrap()
        .take()
        .expect("the run delivers a composite preview");
    assert_eq!((preview.width, preview.height), (width, height), "1:1");
    let mut neutral = 0;
    for y in 0..height {
        for x in 0..width {
            let cell = (y * width + x) as usize;
            let shown = &preview.rgba[cell * 4..cell * 4 + 3];
            let expected: Vec<u8> = export16[cell * 3..cell * 3 + 3]
                .iter()
                .map(|&v| focusforge_core::display::reduce16to8(v, x, y))
                .collect();
            assert_eq!(shown, expected, "({x}, {y})");
            if deep_in_patch(x, y) {
                neutral += 1;
                assert!(
                    shown[0].abs_diff(shown[1]) <= 1 && shown[1].abs_diff(shown[2]) <= 1,
                    "({x}, {y}): {shown:?}"
                );
            }
        }
    }
    assert!(neutral > 0);
}

#[test]
fn raw_reopened_project_clips_like_held() {
    let frames = patch_frames(3);
    let dir = tmp_dir("raw_reopened_project_clips_like_held");
    let held = common::fuse_held(&paths(&frames), "weighted-average", false, &dir);
    let plain = narrowed(&read_f32(&held.composite));
    let out = dir.join("held.tif");
    project::export_held(&held, &out, &tiff16(), &NO_METADATA).unwrap();
    let clipped = read_u16(&out);
    assert_ne!(clipped, plain, "the clip changes this document");

    let project_path = dir.join(format!("saved.{}", project::PROJECT_EXTENSION));
    let root = seal_held(&project_path, &held);
    let check = |expected: &[u16], label: &str| {
        let mut project = Project::open(&project_path).unwrap();
        assert_eq!(
            read_composite(&mut project),
            expected,
            "{label}: plane read"
        );
        let out = dir.join(format!("{label}.tif"));
        project::export_checkpoint(&mut project, &root, &out, &tiff16(), &NO_METADATA).unwrap();
        assert_eq!(read_u16(&out), expected, "{label}: export");
    };
    check(&clipped, "reopened");

    drop(frames);
    check(&plain, "sources gone");
}
