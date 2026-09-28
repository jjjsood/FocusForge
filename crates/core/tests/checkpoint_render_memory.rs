// SPDX-License-Identifier: GPL-3.0-or-later

//! Its own test binary: a counting `#[global_allocator]` is process-wide,
//! so it must never share a binary with a sibling test that would inflate
//! its reading. Measuring live heap bytes requires implementing the
//! `unsafe` `GlobalAlloc` trait below.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use focusforge_core::io::{TiffSpec, TiffWriter};
use focusforge_core::params::ParamValues;
use focusforge_core::project::{
    self, CheckpointImage, Metrics, Op, Project, SealRequest, ZMapStorage,
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

/// An op-bearing checkpoint at `height`: a fused root plus one small edit on
/// top, so [`CheckpointImage::render`] takes the splice path rather than
/// the fused path's plain plane stream.
fn seal_op_bearing(name: &str, height: u32) -> (PathBuf, String) {
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
        crop: None,
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");

    let child = project::append_ops_onto(
        &project_path,
        &sealed.checkpoint,
        vec![Op::ResetRegion {
            rect: Rect {
                x: 0,
                y: 0,
                width: WIDTH.min(8),
                height: height.min(8),
            },
        }],
        None,
        1,
        ZMapStorage::Bounded,
    )
    .expect("append_ops_onto must succeed");

    (project_path, child.checkpoint)
}

/// Peak live-heap growth across [`CheckpointImage::prepare`] plus
/// [`CheckpointImage::render`] into a discarding sink, at `height`. Asserts
/// the image spans more than one 4 MiB composite band, the case that
/// exercises band-bounded memory rather than a single-shot allocation.
fn peak_growth_for(height: u32) -> usize {
    let (project_path, checkpoint_id) =
        seal_op_bearing(&format!("checkpoint_render_memory_{height}"), height);
    let mut project = Project::open(&project_path).unwrap();

    reset_peak();
    let image = CheckpointImage::prepare(&project, &checkpoint_id).expect("prepare must succeed");
    let mut bands = 0u32;
    image
        .render(&mut project, &mut |_samples, _rect| {
            bands += 1;
            Ok(())
        })
        .expect("render must succeed");
    assert!(
        bands > 1,
        "the image at height {height} must span more than one 4 MiB composite band"
    );
    peak_growth_since_reset()
}

#[test]
fn peak_growth_is_independent_of_height() {
    let short = peak_growth_for(1400);
    let tall = peak_growth_for(5600);
    let diff = short.abs_diff(tall);
    assert!(
        diff < 1 << 20,
        "peak growth differs by {diff} B between heights 1400 ({short} B) and 5600 ({tall} B) \
         -- more than the 1 MiB budget, so the render is not staying band-bounded"
    );
}
