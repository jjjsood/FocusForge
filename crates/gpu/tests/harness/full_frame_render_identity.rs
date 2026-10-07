// SPDX-License-Identifier: GPL-3.0-or-later

//! Full-frame identity of the unsaved render, the live GPU blend and the saved
//! state reopened from disk, per fusion method, unedited and with strokes; the
//! unedited image also equals the fusion's own output. Reads the stack named by
//! `FOCUSFORGE_REFERENCE_STACK` and leaves its images and projects under
//! `.scratch/full-frame-identity/` for comparison with `focusforge-cli export`.

#[path = "../../../core/tests/common/mod.rs"]
mod core_common;

use std::path::{Path, PathBuf};

use focusforge_core::align::WarpingReader;
use focusforge_core::equalize::GainReader;
use focusforge_core::io::{
    FrameReader, StripCompression, StripSpec, StripWriter, TiffStack, frame_paths,
};
use focusforge_core::project::export::{ExportOptions, MetadataOptions};
use focusforge_core::project::{
    self, HeldCheckpoint, InMemoryCheckpoint, InMemoryKind, Op, ParentRef, PendingState, Point,
    Project,
};
use focusforge_core::retouch::blend;
use focusforge_core::tile::Rect;
use focusforge_core::zmap::ZMapInfo;
use focusforge_gpu::blend::Blender;
use focusforge_gpu::device::shared_device_or_skip;

const METHODS: [&str; 3] = ["laplacian", "weighted-average", "depth-map"];

/// Edge of the live leg's tiles.
const LIVE_TILE: u32 = 300;
/// Rows the images are compared in.
const COMPARE_ROWS: u32 = 128;
/// Bytes a render band holds, as the project's render sizes them.
const RENDER_BAND_BYTES: usize = 4 << 20;

/// Four frame-source strokes over a `width` x `height` image, different in
/// radius and softness. Each path spans more than a live tile and a render
/// band, so it crosses both kinds of edge; the largest overlaps another and
/// one reaches past the right edge.
fn strokes(width: u32, height: u32) -> Vec<Op> {
    let (w, h) = (width as i32, height as i32);
    let stroke =
        |source_id: u16, radius: f32, softness: f32, path: &[(i32, i32)]| Op::BrushStroke {
            source_id,
            path: path.iter().map(|&(x, y)| Point { x, y }).collect(),
            radius,
            softness,
        };
    vec![
        stroke(
            0,
            8.0,
            0.0,
            &[
                (w / 8, h / 8),
                (w / 8 + 450, h / 8 + 260),
                (w / 8 + 900, h / 8 + 120),
            ],
        ),
        stroke(
            20,
            40.0,
            0.25,
            &[
                (w - 600, 3 * h / 4),
                (w - 100, 3 * h / 4 + 250),
                (w + 50, 3 * h / 4 + 150),
            ],
        ),
        stroke(
            40,
            150.0,
            0.5,
            &[
                (w / 2, h / 3),
                (w / 2 + 1000, h / 3 + 700),
                (w / 2 + 300, h / 3 + 1400),
            ],
        ),
        stroke(
            60,
            400.0,
            1.0,
            &[(w / 2 + 500, h / 3 + 400), (w / 2 + 1300, h / 3 + 1200)],
        ),
    ]
}

/// The reference stack's frames, without any result image kept beside them.
fn reference_frames(dir: &Path) -> Vec<PathBuf> {
    frame_paths(dir)
        .expect("the reference stack directory must enumerate")
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("IMG_"))
        })
        .collect()
}

/// The live canvas's render of `held` with `ops` blended on the device, in
/// `LIVE_TILE` tiles read through the registration's gains and warps, written
/// to `out`. A device failure fails the test.
fn render_live(held: &HeldCheckpoint, ops: &[Op], blender: &Blender, out: &Path) {
    let base = TiffStack::open_files(std::slice::from_ref(&held.composite))
        .expect("the held composite must open");
    let (width, height) = base.dimensions();
    let spp = base.frame(0).samples_per_pixel as usize;
    let info = ZMapInfo::new(width, height, held.sources.len() as u32)
        .expect("the composite extent must be a map extent");

    let mut placed = Vec::new();
    for op in ops {
        let Op::BrushStroke { source_id, .. } = op else {
            panic!("the live leg blends brush strokes only");
        };
        if let Some(footprint) = blend::footprint(op, info).expect("the stroke must be valid") {
            placed.push(blend::PlacedStroke {
                op,
                frame: usize::from(*source_id),
                footprint,
            });
        }
    }

    let sources = TiffStack::open_files(&held.sources).expect("the source frames must open");
    let registration = &held.registration;
    let gained = GainReader::new(&sources, registration.gains());
    let warped = WarpingReader::new(
        &gained,
        sources.dimensions(),
        sources.frame(0).samples_per_pixel,
        registration.transforms(sources.len()),
    );
    let origin = registration.origin();

    let mut writer = StripWriter::create(
        out,
        StripSpec {
            width,
            height,
            samples_per_pixel: spp as u32,
            bits_per_sample: 16,
            compression: StripCompression::None,
            description: None,
            metadata: Default::default(),
        },
    )
    .expect("the live image must be creatable");
    for y in (0..height).step_by(LIVE_TILE as usize) {
        let rows = LIVE_TILE.min(height - y);
        let mut band = vec![0u16; width as usize * rows as usize * spp];
        for x in (0..width).step_by(LIVE_TILE as usize) {
            let tile = Rect {
                x,
                y,
                width: LIVE_TILE.min(width - x),
                height: rows,
            };
            let mut samples = vec![0u16; tile.width as usize * tile.height as usize * spp];
            base.read_tile_into(0, tile, &mut samples)
                .expect("the held composite must read");
            blend::apply_strokes(
                info,
                tile,
                &mut samples,
                &placed,
                &warped,
                origin,
                spp,
                |op, info, out_rect, out, region, src, spp| {
                    blender
                        .apply(op, info, out_rect, out, region, src, spp)
                        .expect("the device must blend every stroke");
                    Ok(())
                },
            )
            .expect("the strokes must blend");
            for (row, line) in samples.chunks_exact(tile.width as usize * spp).enumerate() {
                let start = (row * width as usize + x as usize) * spp;
                band[start..start + line.len()].copy_from_slice(line);
            }
        }
        let bytes: Vec<u8> = band.iter().flat_map(|s| s.to_le_bytes()).collect();
        writer
            .write_rows(&bytes)
            .expect("the live rows must be writable");
    }
    writer.finish().expect("the live image must finish");
}

/// Panics, naming `method`, `state` and both legs, at the first sample where
/// the images at `a` and `b` differ.
fn assert_identical(method: &str, state: &str, a: (&str, &Path), b: (&str, &Path)) {
    let stack = TiffStack::open_files(&[a.1.to_path_buf(), b.1.to_path_buf()])
        .expect("both images must open");
    let (width, height) = stack.dimensions();
    let spp = stack.frame(0).samples_per_pixel as usize;
    let mut left = Vec::new();
    let mut right = Vec::new();
    for y in (0..height).step_by(COMPARE_ROWS as usize) {
        let rect = Rect {
            x: 0,
            y,
            width,
            height: COMPARE_ROWS.min(height - y),
        };
        let len = rect.width as usize * rect.height as usize * spp;
        left.resize(len, 0);
        right.resize(len, 0);
        stack
            .read_tile_into(0, rect, &mut left)
            .expect("the first image must read");
        stack
            .read_tile_into(1, rect, &mut right)
            .expect("the second image must read");
        if let Some(at) = left.iter().zip(&right).position(|(l, r)| l != r) {
            let pixel = at / spp;
            panic!(
                "{method} {state}: {} and {} differ at x={} y={} sample {} ({} vs {})",
                a.0,
                b.0,
                pixel % width as usize,
                y as usize + pixel / width as usize,
                at % spp,
                left[at],
                right[at],
            );
        }
    }
}

#[test]
#[ignore = "reads the full reference stack; minutes"]
fn every_render_path_agrees_on_the_reference_stack() {
    let Some(stack_dir) = core_common::reference_stack_dir() else {
        return;
    };
    let Some(shared) = shared_device_or_skip() else {
        return;
    };
    let blender = Blender::new(shared.device(), shared.queue());
    let frames = reference_frames(&stack_dir);
    let out = core_common::workspace_root()
        .join(".scratch")
        .join("full-frame-identity");
    std::fs::create_dir_all(&out).expect("the output directory must be creatable");
    let options = (ExportOptions::default(), MetadataOptions::default());

    for method in METHODS {
        println!("{method}: fusing {} frames", frames.len());
        let held =
            core_common::fuse_held(&frames, method, true, &out.join(format!("{method}-held")));
        let fused = out.join(format!("{method}-fused.tif"));
        std::fs::copy(&held.composite, &fused).expect("the held composite must copy");

        let ops = strokes(held.width, held.height);
        let band_rows = RENDER_BAND_BYTES / (held.width as usize * 3 * 2);
        for op in &ops {
            let Op::BrushStroke { path, .. } = op else {
                unreachable!("strokes builds brush strokes");
            };
            let span = |axis: fn(&Point) -> i32| {
                path.iter().map(axis).max().unwrap() - path.iter().map(axis).min().unwrap()
            };
            assert!(span(|p| p.x) > LIVE_TILE as i32 && span(|p| p.y) > band_rows as i32);
        }

        let project_path = out.join(format!("{method}.ffp"));
        let _ = std::fs::remove_file(&project_path);
        let ids = project::seal_in_memory(
            &project_path,
            &[
                InMemoryCheckpoint {
                    kind: InMemoryKind::Fused(held.clone()),
                    parent: None,
                },
                InMemoryCheckpoint {
                    kind: InMemoryKind::Ops {
                        ops: ops.clone(),
                        created_at: project::now_unix(),
                    },
                    parent: Some(ParentRef::Entry(0)),
                },
            ],
            &[],
            None,
            false,
        )
        .expect("the held result and its strokes must seal");
        std::fs::write(
            out.join(format!("{method}-checkpoints.txt")),
            format!("root {}\nedited {}\n", ids[0], ids[1]),
        )
        .expect("the checkpoint ids must be writable");

        for (state, id, ops) in [
            ("unedited", &ids[0], &[][..]),
            ("edited", &ids[1], &ops[..]),
        ] {
            let leg = |name: &str| out.join(format!("{method}-{state}-{name}.tif"));
            let (display, live, reopened) = (leg("display"), leg("live"), leg("reopened"));

            if ops.is_empty() {
                project::export_held(&held, &display, &options.0, &options.1)
            } else {
                let reference = held.align.as_ref().map_or(0, |a| a.reference);
                let pending = PendingState {
                    composite: held.composite.clone(),
                    ops: ops.to_vec(),
                    sources: held.sources.clone(),
                    registration: held.registration.clone(),
                    reference,
                    description: held.composite_description.clone(),
                };
                project::export_pending(&pending, &display, &options.0, &options.1)
            }
            .expect("the unsaved state must export");

            render_live(&held, ops, &blender, &live);

            let mut reopened_project =
                Project::open(&project_path).expect("the saved project must reopen");
            project::export_checkpoint(
                &mut reopened_project,
                id,
                &reopened,
                &options.0,
                &options.1,
            )
            .expect("the saved state must export");

            assert_identical(method, state, ("display", &display), ("live", &live));
            assert_identical(
                method,
                state,
                ("display", &display),
                ("reopened", &reopened),
            );
            if ops.is_empty() {
                assert_identical(
                    method,
                    state,
                    ("fusion output", &fused),
                    ("display", &display),
                );
            }
            println!("{method} {state}: display, live and reopened images are identical");
        }
    }
}
