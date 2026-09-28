// SPDX-License-Identifier: GPL-3.0-or-later

//! Its own test binary, holding only the one memory test: resolving an
//! eight-link delta chain must not hold more than a one-link chain's own
//! peak plus one patch's worth of bytes. A counting `#[global_allocator]`
//! is process-wide, so it must never share a binary with a sibling test
//! that would inflate its reading.
//!
//! # Safety
//!
//! `GlobalAlloc` is an `unsafe trait`; there is no way to measure live heap
//! bytes without implementing it. Dev-only measurement scaffolding for this
//! one test binary, never shipped as part of `crates/core`'s library crate.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use focusforge_core::io::{Rect, TiffSpec, TiffWriter};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::project::{
    self, BlobKind, Metrics, Op, Point, Project, SealRequest, ZMapStorage,
};
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

const W: u32 = 1024;
const H: u32 = 1024;
const SPP: u32 = 3;
const FRAMES: u32 = 8;

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

fn composite_samples() -> Vec<u16> {
    (0..(W as usize * H as usize * SPP as usize))
        .map(|i| {
            let x = (i / SPP as usize) % W as usize;
            let channel = i % SPP as usize;
            ((x * 65_535 / W as usize) as u16).wrapping_add(channel as u16 * 97)
        })
        .collect()
}

/// A high-entropy Z-Map: every link's tiny brush edit must reliably beat
/// the full blob, which a piecewise-constant map's own near-zero
/// compressed size would not let it do.
fn zmap_noisy() -> Vec<u16> {
    (0..W * H)
        .map(|i| {
            let x = i % W;
            let y = i / W;
            let h1 = x
                .wrapping_mul(2_654_435_761)
                .wrapping_add(y.wrapping_mul(40_503));
            (h1 % FRAMES) as u16
        })
        .collect()
}

fn stage_planes(dir: &Path) -> (PathBuf, PathBuf) {
    let composite = dir.join("composite.tif");
    let zmap = dir.join("zmap.tif");
    let mut writer = TiffWriter::create(
        &composite,
        TiffSpec {
            width: W,
            height: H,
            samples_per_pixel: SPP,
            description: Some("staged composite".to_string()),
        },
    )
    .unwrap();
    writer.write_tile(full(W, H), &composite_samples()).unwrap();
    writer.finish().unwrap();

    let mut map = ZMapWriter::create(&zmap, ZMapInfo::new(W, H, FRAMES).unwrap()).unwrap();
    map.write_tile(full(W, H), &zmap_noisy()).unwrap();
    map.finish().unwrap();
    (composite, zmap)
}

fn write_sources(dir: &Path, count: usize) -> Vec<PathBuf> {
    (0..count)
        .map(|i| {
            let path = dir.join(format!("frame-{i:02}.tif"));
            let mut writer = TiffWriter::create(
                &path,
                TiffSpec {
                    width: W,
                    height: H,
                    samples_per_pixel: SPP,
                    description: None,
                },
            )
            .unwrap();
            let samples: Vec<u16> = (0..(W as usize * H as usize * SPP as usize))
                .map(|j| ((j + i * 13) % 65_536) as u16)
                .collect();
            writer.write_tile(full(W, H), &samples).unwrap();
            writer.finish().unwrap();
            path
        })
        .collect()
}

fn params() -> ParamValues {
    let mut values = ParamValues::new();
    values.insert("levels".to_string(), ParamValue::Int(5));
    values
}

fn sealed(name: &str) -> (PathBuf, PathBuf) {
    let dir = tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let (composite, zmap) = stage_planes(&staging);
    let sources = write_sources(&dir, 3);
    let values = params();
    let project_path = dir.join(format!("demo.{}", project::PROJECT_EXTENSION));
    project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources: &sources,
        algorithm: "laplacian",
        params: &values,
        align: None,
        crop: None,
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");
    (dir, project_path)
}

fn small_stroke(source_id: u16, x: i32, y: i32) -> Op {
    Op::BrushStroke {
        source_id,
        path: vec![Point { x, y }],
        radius: 6.0,
        softness: 0.0,
    }
}

/// A project whose head is the end of a `links`-long chain of delta-stored
/// checkpoints, each a tiny brush edit at its own distinct spot.
fn chain_project(name: &str, links: usize) -> (PathBuf, PathBuf) {
    let (dir, project_path) = sealed(name);
    for i in 0..links {
        let x = 50 + i as i32 * 100;
        let y = 60 + i as i32 * 80;
        let sealed_ops = project::append_ops_at(
            &project_path,
            vec![small_stroke((i % FRAMES as usize) as u16, x, y)],
            None,
            1_700_000_000 + i as i64,
            ZMapStorage::Smallest,
        )
        .unwrap_or_else(|err| panic!("append #{i} must succeed: {err}"));
        assert_eq!(
            sealed_ops.zmap.kind,
            BlobKind::SparseDelta,
            "link #{i} must land as a delta to build a real chain"
        );
    }
    (dir, project_path)
}

fn resolve_peak(project_path: &Path, out: &Path) -> usize {
    let mut project = Project::open(project_path).unwrap();
    let head = project.checkpoint().clone();
    reset_peak();
    project::materialize_zmap(&mut project, &head, out).unwrap();
    peak_growth_since_reset()
}

#[test]
fn chain_resolution_holds_one_map() {
    let (dir1, one_link) = chain_project("delta_blob_memory_one_link", 1);
    let (dir8, eight_link) = chain_project("delta_blob_memory_eight_link", 8);

    let peak_one = resolve_peak(&one_link, &dir1.join("resolved-one.tif"));
    let peak_eight = resolve_peak(&eight_link, &dir8.join("resolved-eight.tif"));

    // One map's own row-band buffers, generously, is the only per-resolve
    // cost a correct implementation pays regardless of chain length. A
    // per-link full map would show as roughly +2 MiB (1024x1024x2 bytes)
    // per extra link -- seven times this ceiling.
    let ceiling = 1usize << 20;
    assert!(
        peak_eight <= peak_one + ceiling,
        "an eight-link chain's peak {peak_eight} B exceeded a one-link chain's {peak_one} B by \
         more than the {ceiling} B ceiling -- the peak must not grow per link"
    );
}
