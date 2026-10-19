// SPDX-License-Identifier: GPL-3.0-or-later

//! Measurements on the real 61-frame stack: index-map sizes, full-image,
//! frame-major and region render cost, and each variant's distance to the
//! stacked result. Skips with a neutral message when
//! `FOCUSFORGE_REFERENCE_STACK` is unset.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use composite_from_sources_and_zmap as spike;
use focusforge_core::io::{Rect, TiffStack};
use focusforge_core::tile::TilePlan;
use spike::{Buffer, FrameSet, Residual};

const DEFAULT_TILE_SIZE: u32 = 512;
const LEVELS: u32 = 5;
const SEAM_BAND: u32 = 140;

#[path = "../../../crates/core/tests/common/reference_stack.rs"]
mod reference_stack;
use reference_stack::reference_stack_dir;

fn scratch_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.scratch/composite-from-sources-and-zmap")
}

fn rss_anon_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix("RssAnon:")
            .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
    })
}

/// Runs `f` and prints its wall time, the process's peak resident set
/// (mapped source pages included) and its anonymous resident memory at the
/// start and at the peak, sampled every 10 ms. Anonymous memory excludes
/// the mapped source frames, so its rise is what the renderer allocates.
fn measure<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let anon_start = rss_anon_kb().unwrap_or(0);
    let anon_peak = AtomicU64::new(anon_start);
    let done = AtomicBool::new(false);
    spike::clear_peak_rss();
    let t0 = Instant::now();
    let out = std::thread::scope(|s| {
        s.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                if let Some(kb) = rss_anon_kb() {
                    anon_peak.fetch_max(kb, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let out = f();
        done.store(true, Ordering::Relaxed);
        out
    });
    let mb = |kb: u64| kb as f64 / 1024.0;
    println!(
        "reference_stack_measurements: {label} time={:.1}s peak_rss={:.1}MB anon_start={:.1}MB anon_peak={:.1}MB anon_rise={:.1}MB",
        t0.elapsed().as_secs_f64(),
        mb(spike::peak_rss_kb().unwrap_or(0)),
        mb(anon_start),
        mb(anon_peak.load(Ordering::Relaxed)),
        mb(anon_peak.load(Ordering::Relaxed) - anon_start),
    );
    out
}

fn size_row(label: &str, bytes: &[u8]) -> (u64, u64) {
    let zstd = spike::zstd3_len(bytes).expect("zstd must succeed");
    println!(
        "reference_stack_measurements: {label} raw={}B zstd3={}B",
        bytes.len(),
        zstd
    );
    (bytes.len() as u64, zstd)
}

fn diff_row(label: &str, a: &[u16], b: &[u16]) {
    let (max, mean) = spike::diff(a, b);
    println!("reference_stack_measurements: {label} max={max} mean={mean:.4}");
}

fn seam_row(label: &str, a: &Buffer, b: &Buffer, plan: &TilePlan) {
    let seam = spike::seam_band_diff(a, b, plan, SEAM_BAND);
    println!(
        "reference_stack_measurements: {label} seam band max={} mean={:.4} outside max={}",
        seam.band_max, seam.band_mean, seam.outside_max
    );
}

/// Prints index-map sizes, full-image render cost tile-major and
/// frame-major, the seam-band split against the tiled fuse, each variant's
/// distance to the stacked result, and an interior region render compared
/// with the frame-major render's own crop. Writes today's, B's and A's
/// full-size composites under `.scratch/composite-from-sources-and-zmap/`.
#[test]
#[ignore]
fn reference_stack_measurements() {
    let Some(dir) = reference_stack_dir() else {
        return;
    };

    let paths = spike::reference_frame_paths(&dir).expect("reference frames must enumerate");
    let frames = FrameSet::open(&paths).expect("reference stack must open");
    let full = Rect {
        x: 0,
        y: 0,
        width: frames.width,
        height: frames.height,
    };
    let stacked_path = dir.join("Mantis_Stacked.tif");
    let stacked = TiffStack::open_files(std::slice::from_ref(&stacked_path))
        .expect("stacked result must open");
    assert_eq!(
        stacked.dimensions(),
        (frames.width, frames.height),
        "the stacked result must share the frames' extent"
    );
    println!(
        "reference_stack_measurements: {} frames, {}x{}",
        frames.frame_count(),
        frames.width,
        frames.height
    );
    let plan = TilePlan::new(frames.width, frames.height, DEFAULT_TILE_SIZE, LEVELS)
        .expect("plan must build");
    let scratch = scratch_dir();
    std::fs::create_dir_all(&scratch).expect("scratch dir must be creatable");

    // Today's composite, the control row.
    let (today, today_zmap) = measure("today full render", || {
        spike::shipped_composite_tiled(&frames, LEVELS, DEFAULT_TILE_SIZE)
    })
    .expect("shipped run must succeed");
    size_row("today composite", &spike::le_bytes(&today.samples));
    size_row("level-0 zmap", &spike::le_bytes(&today_zmap.samples));
    drop(today_zmap);
    spike::write_tif(&scratch.join("today.tif"), &today, None).expect("write today.tif");

    // Variant B's stored selection and its size per level.
    let selection = measure("B selection build", || {
        spike::tiled_selection(&frames, LEVELS, DEFAULT_TILE_SIZE)
    })
    .expect("selection must build");
    let mut b_total = (0u64, 0u64);
    for (k, plane) in selection.per_level.iter().enumerate() {
        let (raw, zstd) = size_row(&format!("B level {k}"), &spike::le_bytes(&plane.data));
        b_total = (b_total.0 + raw, b_total.1 + zstd);
    }
    let residual_bytes: Vec<u8> = selection
        .residual
        .data()
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let (raw, zstd) = size_row("B residual", &residual_bytes);
    b_total = (b_total.0 + raw, b_total.1 + zstd);
    println!(
        "reference_stack_measurements: B total raw={}B zstd3={}B",
        b_total.0, b_total.1
    );

    // Variant B, tile-major.
    let margin = plan.margin();
    let b_full = measure(
        &format!("B tile-major render margin={margin} residual=recomputed"),
        || spike::render_b_tiled(&frames, &selection, Residual::Recompute, &plan, margin),
    )
    .expect("render must succeed");
    let b_stored = measure(
        &format!("B tile-major render margin={margin} residual=stored"),
        || {
            spike::render_b_tiled(
                &frames,
                &selection,
                Residual::Stored(&selection.residual),
                &plan,
                margin,
            )
        },
    )
    .expect("render must succeed");
    diff_row(
        "B recomputed vs stored residual",
        &b_full.samples,
        &b_stored.samples,
    );
    drop(b_stored);
    let b_128 = measure("B tile-major render margin=128 residual=recomputed", || {
        spike::render_b_tiled(&frames, &selection, Residual::Recompute, &plan, 128)
    })
    .expect("render must succeed");
    diff_row(
        "B vs today (same tile plan)",
        &b_full.samples,
        &today.samples,
    );
    seam_row("B vs today (same tile plan)", &b_full, &today, &plan);
    spike::write_tif(&scratch.join("b.tif"), &b_full, None).expect("write b.tif");

    // Variant A, tile-major.
    let zmap0 = &selection.per_level[0];
    let a_full = measure(&format!("A tile-major render margin={margin}"), || {
        spike::render_a_tiled(&frames, zmap0, LEVELS, &plan)
    })
    .expect("render must succeed");
    diff_row(
        "A vs today (same tile plan)",
        &a_full.samples,
        &today.samples,
    );
    seam_row("A vs today (same tile plan)", &a_full, &today, &plan);
    spike::write_tif(&scratch.join("a.tif"), &a_full, None).expect("write a.tif");

    // Frame-major: the whole extent as one rectangle, one frame's pyramid
    // live at a time.
    let b_frame_major = measure("B frame-major render residual=recomputed", || {
        spike::render_b(&frames, &selection, Residual::Recompute, full, 0)
    })
    .expect("render must succeed");
    diff_row(
        "B frame-major vs B tile-major margin=128",
        &b_frame_major.samples,
        &b_128.samples,
    );
    diff_row(
        &format!("B frame-major vs B tile-major margin={margin}"),
        &b_frame_major.samples,
        &b_full.samples,
    );
    drop(b_128);
    let a_frame_major = measure("A frame-major render", || {
        spike::render_a(&frames, zmap0, LEVELS, full, 0)
    })
    .expect("render must succeed");
    diff_row(
        &format!("A frame-major vs A tile-major margin={margin}"),
        &a_frame_major.samples,
        &a_full.samples,
    );

    // Distance to the stacked result, whole extent.
    let stacked = stacked.read_tile(0, full).expect("stacked result read");
    for (label, buf) in [("today", &today), ("B", &b_full), ("A", &a_full)] {
        diff_row(
            &format!("{label} vs stacked result (whole extent)"),
            &buf.samples,
            &stacked.samples,
        );
    }
    drop(stacked);

    // A stroke-sized region in the interior, its origin on the 2^levels
    // grid, against the frame-major render's own crop.
    let grid = 1u32 << LEVELS;
    let region = Rect {
        x: frames.width / 2 / grid * grid,
        y: frames.height / 2 / grid * grid,
        width: 256,
        height: 256,
    };
    println!(
        "reference_stack_measurements: interior region x={} y={} 256x256",
        region.x, region.y
    );
    let b_crop = b_frame_major.crop(region);
    let a_crop = a_frame_major.crop(region);
    for margin in [128u32, 96] {
        let piece = measure(
            &format!("B region render margin={margin} residual=recomputed"),
            || spike::render_b(&frames, &selection, Residual::Recompute, region, margin),
        )
        .expect("render must succeed");
        diff_row(
            &format!("B region margin={margin} residual=recomputed vs frame-major crop"),
            &piece.samples,
            &b_crop.samples,
        );
        let piece = measure(
            &format!("B region render margin={margin} residual=stored"),
            || {
                spike::render_b(
                    &frames,
                    &selection,
                    Residual::Stored(&selection.residual),
                    region,
                    margin,
                )
            },
        )
        .expect("render must succeed");
        diff_row(
            &format!("B region margin={margin} residual=stored vs frame-major crop"),
            &piece.samples,
            &b_crop.samples,
        );
        let piece = measure(&format!("A region render margin={margin}"), || {
            spike::render_a(&frames, zmap0, LEVELS, region, margin)
        })
        .expect("render must succeed");
        diff_row(
            &format!("A region margin={margin} vs frame-major crop"),
            &piece.samples,
            &a_crop.samples,
        );
    }
    measure("gather region render", || {
        spike::render_gather(&frames, zmap0, region)
    })
    .expect("render must succeed");
}
