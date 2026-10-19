// SPDX-License-Identifier: GPL-3.0-or-later

//! Fixture-driven tests: whether a composite can be reconstructed from
//! sources and a stored index map alone, checked against the shipped
//! algorithm on the checked-in crops.

use std::path::{Path, PathBuf};

use composite_from_sources_and_zmap as spike;
use focusforge_core::io::{Rect, TiffStack};
use focusforge_core::tile::TilePlan;
use spike::{FrameSet, Residual};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
}

fn open_fixture(name: &str) -> FrameSet {
    let paths = spike::fixture_frame_paths(&fixtures_dir().join(name)).expect("frames must open");
    FrameSet::open(&paths).expect("frame set must open")
}

fn levels_for(name: &str) -> u32 {
    // mantis-64 (64x64) supports at most 4 levels; mantis-512 ships 5, the
    // level count the reference stack's own default plans at.
    if name == "mantis-64" { 4 } else { 5 }
}

fn full_rect(frames: &FrameSet) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: frames.width,
        height: frames.height,
    }
}

fn read_reference(name: &str, width: u32, height: u32) -> Vec<u16> {
    let path = fixtures_dir().join(name).join("reference.tif");
    let stack = TiffStack::open_files(&[path]).expect("reference must open");
    stack
        .read_tile(
            0,
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
        )
        .expect("reference read")
        .samples
}

/// One tiled geometry (mantis-64's single tile, mantis-512 at 448/5 and at
/// 480/5): the rebuilt per-level selection, collapsed over the recomputed
/// residual, must equal the shipped composite exactly, and its level-0
/// winners must equal the shipped Z-Map index for index.
fn assert_rebuild_matches_shipped(frames: &FrameSet, levels: u32, tile_size: u32) {
    let (shipped_composite, shipped_zmap) =
        spike::shipped_composite_tiled(frames, levels, tile_size)
            .expect("shipped run must succeed");
    let selection =
        spike::tiled_selection(frames, levels, tile_size).expect("rebuild must succeed");

    assert_eq!(
        selection.per_level[0].data, shipped_zmap.samples,
        "tile_size {tile_size}: level-0 winners must equal the shipped Z-Map index for index"
    );

    let (rebuilt, _) = spike::rebuild_composite_tiled(frames, levels, tile_size)
        .expect("rebuild run must succeed");
    let (max, _) = spike::diff(&rebuilt.samples, &shipped_composite.samples);
    assert_eq!(
        max, 0,
        "tile_size {tile_size}: collapsing the rebuilt per-level selection must match the shipped composite exactly"
    );
}

#[test]
fn rebuild_matches_the_shipped_laplacian() {
    let mantis_64 = open_fixture("mantis-64");
    // A tile_size large enough that the 64x64 fixture plans as one
    // unmargined tile at its own max level count.
    assert_rebuild_matches_shipped(&mantis_64, levels_for("mantis-64"), 256);

    let mantis_512 = open_fixture("mantis-512");
    for &tile_size in &[448u32, 480u32] {
        assert_rebuild_matches_shipped(&mantis_512, 5, tile_size);
    }
}

/// Variant B, built from the untiled per-level maps, must equal an untiled
/// run of the shipped algorithm exactly, on both fixtures; its distance to
/// a tiled composite must be non-zero only inside the seam band.
#[test]
fn per_level_render_matches_the_untiled_fuse() {
    for name in ["mantis-64", "mantis-512"] {
        let frames = open_fixture(name);
        let levels = levels_for(name);
        let (untiled_composite, _) =
            spike::untiled_composite(&frames, levels).expect("untiled run must succeed");
        let selection =
            spike::untiled_selection(&frames, levels).expect("untiled rebuild must succeed");
        let rendered = spike::render_b(
            &frames,
            &selection,
            Residual::Recompute,
            full_rect(&frames),
            0,
        )
        .expect("render must succeed");
        let (max, _) = spike::diff(&rendered.samples, &untiled_composite.samples);
        assert_eq!(
            max, 0,
            "{name}: variant B from the untiled per-level maps must equal the untiled fuse exactly"
        );
    }

    let frames = open_fixture("mantis-512");
    let selection = spike::untiled_selection(&frames, 5).expect("untiled rebuild must succeed");
    let untiled_b = spike::render_b(
        &frames,
        &selection,
        Residual::Recompute,
        full_rect(&frames),
        0,
    )
    .expect("render must succeed");
    for &tile_size in &[448u32, 480u32] {
        let (tiled_composite, _) = spike::shipped_composite_tiled(&frames, 5, tile_size)
            .expect("shipped run must succeed");
        let plan =
            TilePlan::new(frames.width, frames.height, tile_size, 5).expect("plan must build");
        let seam = spike::seam_band_diff(&untiled_b, &tiled_composite, &plan, 140);
        println!(
            "per_level_render_matches_the_untiled_fuse: tile_size={tile_size} seam band max={} mean={:.4} outside max={}",
            seam.band_max, seam.band_mean, seam.outside_max
        );
        assert_eq!(
            seam.outside_max, 0,
            "tile_size {tile_size}: the difference outside the 140px seam band must be 0"
        );
    }
}

/// One winning frame per pixel serves every channel: a selection with one
/// winner per channel gives a different composite, while the shared-index
/// render reproduces the untiled fuse exactly.
#[test]
fn one_index_serves_every_channel() {
    let frames = open_fixture("mantis-512");
    let (untiled_composite, _) =
        spike::untiled_composite(&frames, 5).expect("untiled run must succeed");

    let selection = spike::untiled_selection(&frames, 5).expect("untiled rebuild must succeed");
    let shared = spike::render_b(
        &frames,
        &selection,
        Residual::Recompute,
        full_rect(&frames),
        0,
    )
    .expect("render must succeed");
    let (shared_max, _) = spike::diff(&shared.samples, &untiled_composite.samples);

    let per_channel =
        spike::per_channel_composite(&frames, 5).expect("per-channel run must succeed");
    let (max, mean) = spike::diff(&per_channel.samples, &untiled_composite.samples);
    println!(
        "one_index_serves_every_channel: shared index max={shared_max}; per-channel selection max={max} mean={mean:.4}"
    );
    assert_eq!(shared_max, 0, "the shared-index render must equal the fuse");
    assert!(
        max > 0,
        "a per-channel selection must differ from the fuse's composite"
    );
}

/// Variant A's distance, recorded the way B's is: against the untiled fuse
/// on both fixtures, and against each `mantis-512` tiled composite split by
/// the 140 px seam band.
#[test]
fn blend_distance_to_the_fuse_is_recorded() {
    for name in ["mantis-64", "mantis-512"] {
        let frames = open_fixture(name);
        let levels = levels_for(name);
        let (untiled_composite, _) =
            spike::untiled_composite(&frames, levels).expect("untiled run must succeed");
        let selection =
            spike::untiled_selection(&frames, levels).expect("untiled rebuild must succeed");
        let a = spike::render_a(
            &frames,
            &selection.per_level[0],
            levels,
            full_rect(&frames),
            0,
        )
        .expect("render must succeed");
        let (max, mean) = spike::diff(&a.samples, &untiled_composite.samples);
        println!(
            "blend_distance_to_the_fuse_is_recorded: {name} vs untiled max={max} mean={mean:.4}"
        );
    }

    let frames = open_fixture("mantis-512");
    for &tile_size in &[448u32, 480u32] {
        let (tiled_composite, tiled_zmap) = spike::shipped_composite_tiled(&frames, 5, tile_size)
            .expect("shipped run must succeed");
        let plan =
            TilePlan::new(frames.width, frames.height, tile_size, 5).expect("plan must build");
        let zmap0 = spike::IndexPlane {
            width: frames.width,
            height: frames.height,
            data: tiled_zmap.samples,
        };
        let a = spike::render_a_tiled(&frames, &zmap0, 5, &plan).expect("render must succeed");
        let seam = spike::seam_band_diff(&a, &tiled_composite, &plan, 140);
        println!(
            "blend_distance_to_the_fuse_is_recorded: mantis-512 tile_size={tile_size} seam band max={} mean={:.4} outside max={}",
            seam.band_max, seam.band_mean, seam.outside_max
        );
    }
}

/// Variant B rendered from the stored mean residual must equal the render
/// from the recomputed one, bit for bit, full image and region.
#[test]
fn stored_residual_renders_like_the_recomputed_one() {
    let frames = open_fixture("mantis-512");
    let levels = 5;
    let selection =
        spike::untiled_selection(&frames, levels).expect("untiled rebuild must succeed");
    let full = full_rect(&frames);

    let recomputed = spike::render_b(&frames, &selection, Residual::Recompute, full, 0)
        .expect("render must succeed");
    let stored = spike::render_b(
        &frames,
        &selection,
        Residual::Stored(&selection.residual),
        full,
        0,
    )
    .expect("render must succeed");
    let (max, _) = spike::diff(&recomputed.samples, &stored.samples);
    assert_eq!(
        max, 0,
        "full image: recomputed and stored residual must render identically"
    );

    let region = Rect {
        x: 128,
        y: 128,
        width: 256,
        height: 256,
    };
    let recomputed_region = spike::render_b(&frames, &selection, Residual::Recompute, region, 96)
        .expect("render must succeed");
    let stored_region = spike::render_b(
        &frames,
        &selection,
        Residual::Stored(&selection.residual),
        region,
        96,
    )
    .expect("render must succeed");
    let (max_region, _) = spike::diff(&recomputed_region.samples, &stored_region.samples);
    assert_eq!(
        max_region, 0,
        "region: recomputed and stored residual must render identically"
    );
}

/// On `mantis-512`, a region re-render at a corner and across a tile seam
/// must reproduce the matching crop of the full render: bit-for-bit at a
/// 128px margin, within 1 LSB at 96px. Variant A's difference at both
/// margins is recorded, not asserted.
#[test]
fn region_render_matches_the_full_render() {
    let frames = open_fixture("mantis-512");
    let levels = 5;
    let selection =
        spike::untiled_selection(&frames, levels).expect("untiled rebuild must succeed");
    let full = full_rect(&frames);
    let full_render = spike::render_b(&frames, &selection, Residual::Recompute, full, 0)
        .expect("render must succeed");
    let zmap0 = &selection.per_level[0];

    let regions = [
        (
            "corner",
            Rect {
                x: 0,
                y: 0,
                width: 256,
                height: 256,
            },
        ),
        (
            "across-seam",
            Rect {
                x: 128,
                y: 128,
                width: 256,
                height: 256,
            },
        ),
    ];

    for (label, region) in regions {
        let full_crop = full_render.crop(region);
        for &margin in &[128u32, 96u32] {
            let region_render =
                spike::render_b(&frames, &selection, Residual::Recompute, region, margin)
                    .expect("render must succeed");
            let (max, _) = spike::diff(&region_render.samples, &full_crop.samples);
            if margin == 128 {
                assert_eq!(
                    max, 0,
                    "{label} at 128px margin must match the full render exactly"
                );
            } else {
                assert!(
                    max <= 1,
                    "{label} at 96px margin must match the full render within 1 LSB, got {max}"
                );
            }

            let a_render = spike::render_a(&frames, zmap0, levels, region, margin)
                .expect("render must succeed");
            let (a_max, a_mean) = spike::diff(&a_render.samples, &full_crop.samples);
            println!(
                "region_render_matches_the_full_render: {label} margin={margin} B max={max} A max={a_max} A mean={a_mean:.4}"
            );
        }
    }
}

/// The max and mean distance of today's composite (control), variant A,
/// variant B and the plain gather to each fixture's own `reference.tif`.
#[test]
fn distances_to_the_reference_are_recorded() {
    for name in ["mantis-64", "mantis-512"] {
        let frames = open_fixture(name);
        let levels = levels_for(name);
        let reference = read_reference(name, frames.width, frames.height);
        let full = full_rect(&frames);

        let (today, _) =
            spike::untiled_composite(&frames, levels).expect("today's run must succeed");
        let selection =
            spike::untiled_selection(&frames, levels).expect("untiled rebuild must succeed");
        let b = spike::render_b(&frames, &selection, Residual::Recompute, full, 0)
            .expect("render must succeed");
        let a = spike::render_a(&frames, &selection.per_level[0], levels, full, 0)
            .expect("render must succeed");
        let gather = spike::render_gather(&frames, &selection.per_level[0], full)
            .expect("render must succeed");

        for (label, buf) in [("today", &today), ("B", &b), ("A", &a), ("gather", &gather)] {
            let (max, mean) = spike::diff(&buf.samples, &reference);
            println!(
                "distances_to_the_reference_are_recorded: {name} {label} max={max} mean={mean:.4}"
            );
        }
    }
}
