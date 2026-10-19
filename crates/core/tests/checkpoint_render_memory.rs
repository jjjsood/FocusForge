// SPDX-License-Identifier: GPL-3.0-or-later

//! Its own test binary: a counting `#[global_allocator]` is process-wide,
//! so it must never share a binary with a sibling test that would inflate
//! its reading. Measuring live heap bytes requires implementing the
//! `unsafe` `GlobalAlloc` trait below.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use focusforge_core::align::Registration;
use focusforge_core::io::{TiffSpec, TiffWriter};
use focusforge_core::params::ParamValues;
use focusforge_core::project::export::{
    ExportOptions, JpegSubsampling, MetadataOptions, PngBits, PngCompression, TiffBits,
    TiffCompression,
};
use focusforge_core::project::{
    self, CheckpointImage, Metrics, Op, Point, Project, ProjectError, SealRequest,
};
use focusforge_core::tile::Rect;
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct CountingAllocator;

// SAFETY: delegates every operation straight to `System`, the platform
// allocator that is already sound; the only addition is byte-counting
// bookkeeping around each call, which touches no allocation itself.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `layout` is exactly what the caller of `GlobalAlloc::alloc`
        // must already guarantee is valid; forwarded unchanged to `System`.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let cur = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(cur, Ordering::SeqCst);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as `alloc` above, forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: same contract as `alloc` above, forwarded unchanged.
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            if new_size >= layout.size() {
                let delta = new_size - layout.size();
                let cur = LIVE.fetch_add(delta, Ordering::SeqCst) + delta;
                PEAK.fetch_max(cur, Ordering::SeqCst);
            } else {
                let delta = layout.size() - new_size;
                LIVE.fetch_sub(delta, Ordering::SeqCst);
            }
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

static BASELINE: AtomicUsize = AtomicUsize::new(0);

fn reset_peak() {
    let live = LIVE.load(Ordering::SeqCst);
    BASELINE.store(live, Ordering::SeqCst);
    PEAK.store(live, Ordering::SeqCst);
}

/// How far live heap bytes grew above the baseline captured at
/// [`reset_peak`].
fn peak_growth_since_reset() -> usize {
    PEAK.load(Ordering::SeqCst)
        .saturating_sub(BASELINE.load(Ordering::SeqCst))
}

const WIDTH: u32 = 1024;
const SPP: u32 = 3;
const FRAMES: u32 = 2;

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

/// A fused root at `height` plus one small edit on top. Returns the project,
/// the root's id and the op-bearing child's id, whose view render takes the
/// blend path rather than the fused path's plain plane stream.
fn seal_op_bearing(name: &str, height: u32) -> (PathBuf, String, String) {
    let dir = tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    let composite_path = staging.join("composite.tif");
    let mut writer = TiffWriter::create(
        &composite_path,
        TiffSpec {
            width: WIDTH,
            height,
            samples_per_pixel: SPP,
            description: Some("staged composite".to_string()),
        },
    )
    .unwrap();
    writer
        .write_tile(
            full(WIDTH, height),
            &vec![100u16; (WIDTH * height * SPP) as usize],
        )
        .unwrap();
    writer.finish().unwrap();

    let zmap_path = staging.join("zmap.tif");
    let mut map =
        ZMapWriter::create(&zmap_path, ZMapInfo::new(WIDTH, height, FRAMES).unwrap()).unwrap();
    map.write_tile(full(WIDTH, height), &vec![0u16; (WIDTH * height) as usize])
        .unwrap();
    map.finish().unwrap();

    let sources: Vec<PathBuf> = (0..FRAMES)
        .map(|i| {
            let path = dir.join(format!("frame-{i:02}.tif"));
            let mut w = TiffWriter::create(
                &path,
                TiffSpec {
                    width: WIDTH,
                    height,
                    samples_per_pixel: SPP,
                    description: None,
                },
            )
            .unwrap();
            w.write_tile(
                full(WIDTH, height),
                &vec![i as u16 * 10; (WIDTH * height * SPP) as usize],
            )
            .unwrap();
            w.finish().unwrap();
            path
        })
        .collect();

    let project_path = dir.join(format!("mem-{height}.{}", project::PROJECT_EXTENSION));
    let sealed = project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite_path,
        staged_zmap: &zmap_path,
        sources: &sources,
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
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
            path: vec![Point { x: 4, y: 4 }],
            radius: 6.0,
            softness: 0.5,
        }],
        None,
        1,
    )
    .expect("append_ops_onto must succeed");

    (project_path, sealed.checkpoint, child.checkpoint)
}

/// Peak live-heap growth across [`CheckpointImage::prepare`] plus
/// [`CheckpointImage::render`] of the whole image into a discarding sink, at
/// `height`, of the root (`fused`) or its op-bearing child. Asserts the image
/// spans more than one band, the case that exercises band-bounded memory
/// rather than a single-shot allocation.
fn peak_growth_for(height: u32, fused: bool) -> usize {
    let (project_path, root_id, child_id) =
        seal_op_bearing(&format!("checkpoint_render_memory_{height}"), height);
    let checkpoint_id = if fused { root_id } else { child_id };
    let mut project = Project::open(&project_path).unwrap();

    reset_peak();
    let image = CheckpointImage::prepare(&project, &checkpoint_id).expect("prepare must succeed");
    let mut bands = 0u32;
    image
        .render(Some(&mut project), image.full(), &mut |_samples, _rect| {
            bands += 1;
            Ok(())
        })
        .expect("render must succeed");
    assert!(
        bands > 1,
        "the image at height {height} must span more than one band"
    );
    peak_growth_since_reset()
}

fn assert_independent_of_height(label: &str, fused: bool, short_height: u32) {
    let short = peak_growth_for(short_height, fused);
    let tall = peak_growth_for(5600, fused);
    let diff = short.abs_diff(tall);
    assert!(
        diff < 1 << 20,
        "{label}: peak growth differs by {diff} B between heights {short_height} ({short} B) and \
         5600 ({tall} B) -- more than the 1 MiB budget, so the render is not staying band-bounded"
    );
}

/// Every combination [`ExportOptions`] admits, paired with a short label for
/// assertion messages: 2x3 TIFF (bit depth x compression), 2x3 PNG (bit
/// depth x compression level) and 2 JPEG (chroma subsampling).
fn every_export_option() -> Vec<(String, ExportOptions)> {
    let mut out = Vec::new();
    for (bits_label, bits) in [("16", TiffBits::Sixteen), ("8", TiffBits::Eight)] {
        for (compression_label, compression) in [
            ("none", TiffCompression::None),
            ("lzw", TiffCompression::Lzw),
            ("deflate", TiffCompression::Deflate),
        ] {
            out.push((
                format!("tiff-{bits_label}-{compression_label}"),
                ExportOptions::Tiff { bits, compression },
            ));
        }
    }
    for (bits_label, bits) in [("16", PngBits::Sixteen), ("8", PngBits::Eight)] {
        for (compression_label, compression) in [
            ("fast", PngCompression::Fast),
            ("default", PngCompression::Default),
            ("best", PngCompression::Best),
        ] {
            out.push((
                format!("png-{bits_label}-{compression_label}"),
                ExportOptions::Png { bits, compression },
            ));
        }
    }
    for (label, subsampling) in [
        ("444", JpegSubsampling::S444),
        ("420", JpegSubsampling::S420),
    ] {
        out.push((
            format!("jpeg-{label}"),
            ExportOptions::Jpeg {
                quality: 90,
                subsampling,
            },
        ));
    }
    out
}

/// Peak live-heap growth of one `export_checkpoint` call, writing to a
/// throwaway path removed right after.
fn export_peak_growth(
    project: &mut Project,
    checkpoint_id: &str,
    dir: &std::path::Path,
    name: &str,
    options: &ExportOptions,
) -> usize {
    let out = dir.join(name);
    reset_peak();
    project::export_checkpoint(
        project,
        checkpoint_id,
        &out,
        options,
        &MetadataOptions::default(),
    )
    .expect("export must succeed");
    let growth = peak_growth_since_reset();
    let _ = std::fs::remove_file(&out);
    growth
}

/// One sealed fused project per height, every format's every option
/// exported from it in turn: peak growth for each must stay independent of
/// height, the same band-bounded claim [`assert_independent_of_height`]
/// checks for the plain render.
fn export_growths_at(height: u32) -> Vec<(String, usize)> {
    let (project_path, root_id, _child_id) =
        seal_op_bearing(&format!("checkpoint_render_memory_export_{height}"), height);
    let mut project = Project::open(&project_path).unwrap();
    let dir = tmp_dir(&format!("checkpoint_render_memory_export_out_{height}"));
    every_export_option()
        .into_iter()
        .map(|(label, options)| {
            let name = format!("{label}.out");
            (
                label,
                export_peak_growth(&mut project, &root_id, &dir, &name, &options),
            )
        })
        .collect()
}

// One test, so the two measurements never overlap under the process-wide
// allocator count.
#[test]
fn peak_growth_is_independent_of_height() {
    assert_independent_of_height("op-bearing render", false, 1400);
    assert_independent_of_height("fused render", true, 2800);

    let short = export_growths_at(2800);
    let tall = export_growths_at(5600);
    for ((label, short_growth), (_, tall_growth)) in short.iter().zip(tall.iter()) {
        let diff = short_growth.abs_diff(*tall_growth);
        assert!(
            diff < 1 << 20,
            "{label}: peak growth differs by {diff} B between heights 2800 ({short_growth} B) \
             and 5600 ({tall_growth} B) -- more than the 1 MiB budget, so the export is not \
             staying band-bounded"
        );
    }
}

/// A representative full-size shot shape (44.8 MP at 3 samples per pixel),
/// built as a synthetic sealed project, no real frames read from disk.
/// `#[ignore]`d for its minutes-long build and encode.
const REFERENCE_WIDTH: u32 = 8192;
const REFERENCE_HEIGHT: u32 = 5464;
const REFERENCE_SPP: u32 = 3;

fn seal_fused(name: &str, width: u32, height: u32, spp: u32, frames: u32) -> (PathBuf, String) {
    let dir = tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    let composite_path = staging.join("composite.tif");
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
            &vec![100u16; (width * height * spp) as usize],
        )
        .unwrap();
    writer.finish().unwrap();

    let zmap_path = staging.join("zmap.tif");
    let mut map =
        ZMapWriter::create(&zmap_path, ZMapInfo::new(width, height, frames).unwrap()).unwrap();
    map.write_tile(full(width, height), &vec![0u16; (width * height) as usize])
        .unwrap();
    map.finish().unwrap();

    let sources: Vec<PathBuf> = (0..frames)
        .map(|i| {
            let path = dir.join(format!("frame-{i:02}.tif"));
            let mut w = TiffWriter::create(
                &path,
                TiffSpec {
                    width,
                    height,
                    samples_per_pixel: spp,
                    description: None,
                },
            )
            .unwrap();
            w.write_tile(
                full(width, height),
                &vec![i as u16 * 10; (width * height * spp) as usize],
            )
            .unwrap();
            w.finish().unwrap();
            path
        })
        .collect();

    let project_path = dir.join(format!(
        "mem-ref-{width}x{height}.{}",
        project::PROJECT_EXTENSION
    ));
    let sealed = project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite_path,
        staged_zmap: &zmap_path,
        sources: &sources,
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");
    (project_path, sealed.checkpoint)
}

#[test]
#[ignore]
fn jpeg_export_peak_at_reference_scale() {
    let (project_path, root_id) = seal_fused(
        "checkpoint_render_memory_jpeg_reference",
        REFERENCE_WIDTH,
        REFERENCE_HEIGHT,
        REFERENCE_SPP,
        2,
    );
    let mut project = Project::open(&project_path).unwrap();
    let dir = tmp_dir("checkpoint_render_memory_jpeg_reference_out");
    let growth = export_peak_growth(
        &mut project,
        &root_id,
        &dir,
        "reference.jpg",
        &ExportOptions::Jpeg {
            quality: 95,
            subsampling: JpegSubsampling::S444,
        },
    );
    println!(
        "JPEG export peak growth at reference scale ({REFERENCE_WIDTH}x{REFERENCE_HEIGHT}x{REFERENCE_SPP}): \
         {growth} B ({:.1} MiB)",
        growth as f64 / (1u64 << 20) as f64
    );
}

#[test]
fn png_and_jpeg_refuse_unsupported_samples_per_pixel() {
    for spp in [2u32, 4] {
        let (project_path, root_id) = seal_fused(
            &format!("checkpoint_render_memory_unsupported_spp_{spp}"),
            16,
            16,
            spp,
            2,
        );
        let mut project = Project::open(&project_path).unwrap();
        let dir = tmp_dir(&format!(
            "checkpoint_render_memory_unsupported_spp_out_{spp}"
        ));

        for (name, options) in [
            (
                "out.png",
                ExportOptions::Png {
                    bits: PngBits::Sixteen,
                    compression: PngCompression::Default,
                },
            ),
            (
                "out.jpg",
                ExportOptions::Jpeg {
                    quality: 95,
                    subsampling: JpegSubsampling::S444,
                },
            ),
        ] {
            let out = dir.join(name);
            let err = project::export_checkpoint(
                &mut project,
                &root_id,
                &out,
                &options,
                &MetadataOptions::default(),
            )
            .expect_err(&format!("spp={spp} {name} must be refused"));
            assert!(
                matches!(err, ProjectError::ExportUnsupportedSamples { .. }),
                "spp={spp} {name}: {err}"
            );
            assert!(!out.exists(), "spp={spp} {name}: no output must be written");
        }

        // TIFF has no such limit.
        let tiff_out = dir.join("out.tif");
        project::export_checkpoint(
            &mut project,
            &root_id,
            &tiff_out,
            &ExportOptions::default(),
            &MetadataOptions::default(),
        )
        .expect("TIFF must accept any samples-per-pixel");
    }
}

#[test]
fn export_checkpoint_validates_options_built_directly_not_through_from_values() {
    let (project_path, root_id, _child_id) =
        seal_op_bearing("checkpoint_render_memory_direct_options", 16);
    let mut project = Project::open(&project_path).unwrap();
    let dir = tmp_dir("checkpoint_render_memory_direct_options_out");
    let out = dir.join("out.jpg");

    // Built directly, bypassing `ExportOptions::from_values`'s own
    // validation -- `export_checkpoint` must still refuse it.
    let options = ExportOptions::Jpeg {
        quality: 300,
        subsampling: JpegSubsampling::S444,
    };
    let err = project::export_checkpoint(
        &mut project,
        &root_id,
        &out,
        &options,
        &MetadataOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(err, ProjectError::ExportOptions { .. }), "{err}");
    assert!(!out.exists());
}
