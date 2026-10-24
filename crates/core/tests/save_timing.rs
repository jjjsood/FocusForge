// SPDX-License-Identifier: GPL-3.0-or-later

//! Wall-clock figures, for each fusion method, for saving a fusion, saving one
//! retouch stroke, opening to the first displayed band, exporting the unedited
//! and retouched state and writing the retouched head's Z-Map: a warm median
//! and a cold (page-cache evicted) run each. The fixture test asserts no time;
//! the `#[ignore]`d reference-stack test asserts the warm targets after
//! printing every figure.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use focusforge_core::io::frame_paths;
use focusforge_core::project::export::{ExportOptions, MetadataOptions};
use focusforge_core::project::{
    self, CheckpointImage, HeldCheckpoint, InMemoryCheckpoint, InMemoryKind, Op, Opened, ParentRef,
    Plane, Point, Project,
};

const WARM_RUNS: usize = 3;

const SAVE_AFTER_FUSION: &str = "save-after-fusion";
const SAVE_AFTER_RETOUCH: &str = "save-after-retouch";
const OPEN_TO_FIRST_IMAGE: &str = "open-to-first-image";
const UNEDITED_EXPORT: &str = "unedited-export";
const EDITED_EXPORT: &str = "edited-export";
const ZMAP_MATERIALIZE: &str = "zmap-materialize";

/// The registered stack algorithms every scenario runs for.
const METHODS: [&str; 3] = ["laplacian", "weighted-average", "depth-map"];

/// Warm targets in seconds for the full-frame stack.
const TARGETS: [(&str, f64); 4] = [
    (SAVE_AFTER_FUSION, 2.0),
    (SAVE_AFTER_RETOUCH, 0.5),
    (OPEN_TO_FIRST_IMAGE, 1.0),
    (UNEDITED_EXPORT, 2.0),
];

struct Figure {
    method: &'static str,
    scenario: &'static str,
    warm: f64,
}

/// Drops `paths` from the page cache, returning how many files it handled.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn evict(paths: &[PathBuf]) -> Result<usize, String> {
    use std::os::fd::AsRawFd;
    for path in paths {
        let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        // Only clean pages can be dropped.
        file.sync_all()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        // SAFETY: the descriptor is open for the call and the advice touches no memory.
        let code =
            unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        if code != 0 {
            return Err(format!(
                "posix_fadvise {}: {}",
                path.display(),
                std::io::Error::from_raw_os_error(code)
            ));
        }
    }
    Ok(paths.len())
}

#[cfg(not(target_os = "linux"))]
fn evict(_paths: &[PathBuf]) -> Result<usize, String> {
    Err("eviction unsupported on this platform".to_string())
}

/// Times `timed` warm and cold and prints both figures. `setup` runs outside
/// the timed region, and `evict_set` is dropped from the page cache after it
/// on the cold run; `timed` returns the duration it measured itself.
fn measure<S>(
    method: &'static str,
    scenario: &'static str,
    evict_set: &[PathBuf],
    mut setup: impl FnMut() -> S,
    mut timed: impl FnMut(S) -> Duration,
) -> Figure {
    let _ = timed(setup());
    let mut runs: Vec<f64> = (0..WARM_RUNS)
        .map(|_| timed(setup()).as_secs_f64())
        .collect();
    runs.sort_by(f64::total_cmp);
    let warm = runs[runs.len() / 2];
    println!("{method} {scenario} warm {warm:.3} s");

    // `timed` runs even when eviction is unsupported: a later scenario reads
    // what it leaves behind.
    let state = setup();
    let evicted = evict(evict_set);
    let cold = timed(state).as_secs_f64();
    match evicted {
        Ok(files) => println!("{method} {scenario} cold {cold:.3} s (evicted {files} files)"),
        Err(reason) => println!("{method} {scenario} cold n/a ({reason})"),
    }
    Figure {
        method,
        scenario,
        warm,
    }
}

/// The fused result of `frames` by `algorithm_id` with the GUI's default
/// settings, kept under the target directory so a rerun on unchanged frames
/// skips the fusion. `laplacian` keeps the frames-only directory name.
fn held_result(frames: &[PathBuf], algorithm_id: &str) -> HeldCheckpoint {
    let mut key = blake3::Hasher::new();
    for frame in frames {
        let meta = std::fs::metadata(frame).expect("a frame must be readable");
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        key.update(frame.as_os_str().as_encoded_bytes());
        key.update(&meta.len().to_le_bytes());
        key.update(&mtime.to_le_bytes());
    }
    let key = key.finalize().to_hex();
    let key = &key.as_str()[..16];
    let name = if algorithm_id == "laplacian" {
        key.to_string()
    } else {
        format!("{key}-{algorithm_id}")
    };
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("save_timing_held")
        .join(name);
    let hold = dir.join("held");
    let record = hold.join("held.json");

    if let Some(held) = std::fs::read(&record)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        println!("reusing held result {}", dir.display());
        return held;
    }

    println!("fusing {} frames with {algorithm_id}", frames.len());
    let _ = std::fs::remove_dir_all(&dir);
    let held = common::fuse_held(frames, algorithm_id, true, &dir);
    // Written last: a fusion that stopped early leaves nothing a rerun trusts.
    std::fs::write(
        &record,
        serde_json::to_vec(&held).expect("a held result must serialize"),
    )
    .expect("the held record must be writable");
    held
}

/// Runs the six scenarios over `frames` fused by `method`, leaving its files
/// under `out`.
fn run_scenarios(frames: &[PathBuf], method: &'static str, out: &Path) -> Vec<Figure> {
    std::fs::create_dir_all(out).expect("the output directory must be creatable");
    let held = held_result(frames, method);
    let fused = out.join("fused.ffp");
    let retouched = out.join("retouched.ffp");
    let with_frames = |extra: &[&Path]| -> Vec<PathBuf> {
        frames
            .iter()
            .cloned()
            .chain(extra.iter().map(|p| p.to_path_buf()))
            .collect()
    };

    let mut root_id = String::new();
    let save_after_fusion = measure(
        method,
        SAVE_AFTER_FUSION,
        &with_frames(&[&held.composite, &held.zmap]),
        || {
            let _ = std::fs::remove_file(&fused);
        },
        |()| {
            let entry = InMemoryCheckpoint {
                kind: InMemoryKind::Fused(held.clone()),
                parent: None,
            };
            let start = Instant::now();
            let ids = project::seal_in_memory(&fused, &[entry], &[], None, false)
                .expect("the held result must seal");
            let elapsed = start.elapsed();
            root_id = ids[0].clone();
            elapsed
        },
    );

    let (w, h) = (held.width as i32, held.height as i32);
    let stroke = Op::BrushStroke {
        source_id: 1,
        path: vec![
            Point { x: w / 4, y: h / 2 },
            Point {
                x: 3 * w / 4,
                y: h / 2,
            },
        ],
        radius: w as f32 / 100.0,
        softness: 0.5,
    };
    let mut head_id = String::new();
    let save_after_retouch = measure(
        method,
        SAVE_AFTER_RETOUCH,
        &with_frames(&[&retouched]),
        || {
            std::fs::copy(&fused, &retouched).expect("the saved project must copy");
            // Flushing a file needs a handle with write access on Windows.
            std::fs::OpenOptions::new()
                .write(true)
                .open(&retouched)
                .and_then(|f| f.sync_all())
                .expect("the copy must reach the disk");
        },
        |()| {
            let entry = InMemoryCheckpoint {
                kind: InMemoryKind::Ops {
                    ops: vec![stroke.clone()],
                    created_at: project::now_unix(),
                },
                parent: Some(ParentRef::Saved(root_id.clone())),
            };
            let start = Instant::now();
            let ids = project::seal_in_memory(&retouched, &[entry], &[], None, true)
                .expect("the stroke must seal onto the saved root");
            let elapsed = start.elapsed();
            head_id = ids[0].clone();
            elapsed
        },
    );

    let open_to_first_image = measure(
        method,
        OPEN_TO_FIRST_IMAGE,
        &with_frames(&[&retouched]),
        || (),
        |()| {
            let start = Instant::now();
            let mut project = match Project::open_checked(&retouched).expect("the project opens") {
                Opened::Ready(project) => project,
                Opened::NeedsChoice(pending) => {
                    panic!("sources changed: {:?}", pending.changed())
                }
            };
            let image = CheckpointImage::prepare(&project, &head_id).expect("the head prepares");
            let mut first = None;
            image
                .render(Some(&mut project), image.full(), &mut |_, _| {
                    first.get_or_insert_with(Instant::now);
                    Ok(())
                })
                .expect("the head renders");
            first.expect("the render delivered no band") - start
        },
    );

    let export = |scenario, project_path: &Path, id: &str, name: &str| {
        let target = out.join(name);
        measure(
            method,
            scenario,
            &with_frames(&[project_path]),
            || {
                let _ = std::fs::remove_file(&target);
                Project::open(project_path).expect("the project opens")
            },
            |mut project| {
                let start = Instant::now();
                project::export_checkpoint(
                    &mut project,
                    id,
                    &target,
                    &ExportOptions::default(),
                    &MetadataOptions::default(),
                )
                .expect("the checkpoint must export");
                start.elapsed()
            },
        )
    };
    let unedited_export = export(UNEDITED_EXPORT, &fused, &root_id, "unedited.tif");
    let edited_export = export(EDITED_EXPORT, &retouched, &head_id, "edited.tif");

    let zmap = out.join("zmap.tif");
    let zmap_materialize = measure(
        method,
        ZMAP_MATERIALIZE,
        &with_frames(&[&retouched]),
        || {
            let _ = std::fs::remove_file(&zmap);
            Project::open(&retouched).expect("the project opens")
        },
        |mut project| {
            let head = project.checkpoint().clone();
            let start = Instant::now();
            project::materialize_zmap(&mut project, &head, &zmap)
                .expect("the head's Z-Map must materialize");
            start.elapsed()
        },
    );

    vec![
        save_after_fusion,
        save_after_retouch,
        open_to_first_image,
        unedited_export,
        edited_export,
        zmap_materialize,
    ]
}

#[test]
fn timings_on_fixture() {
    let frames = frame_paths(&common::mantis_512_dir().join("frames"))
        .expect("the fixture frames must enumerate");
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("save_timing_fixture");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).expect("the output directory must be creatable");

    for method in METHODS {
        let figures = run_scenarios(&frames, method, &out.join(method));
        let names: Vec<_> = figures.iter().map(|f| f.scenario).collect();
        assert_eq!(
            names,
            [
                SAVE_AFTER_FUSION,
                SAVE_AFTER_RETOUCH,
                OPEN_TO_FIRST_IMAGE,
                UNEDITED_EXPORT,
                EDITED_EXPORT,
                ZMAP_MATERIALIZE
            ],
            "{method}"
        );
        assert!(figures.iter().all(|f| f.warm > 0.0), "{method}");
    }
}

/// Prints the multithreaded compression time and ratio of the reference
/// stack's fused composite at levels 1, 2 and 3.
#[test]
#[ignore = "reads the full reference stack; minutes"]
fn composite_compression_by_level() {
    let Some(frames) = reference_frames() else {
        return;
    };
    let held = held_result(&frames, "laplacian");
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("save_timing_levels");
    let _ = std::fs::remove_dir_all(&out);
    for level in 1..=3 {
        let dir = out.join(level.to_string());
        std::fs::create_dir_all(&dir).expect("the output directory must be creatable");
        let start = Instant::now();
        let staged = project::blob::stage_plane(&held.composite, Plane::Composite, level, &dir)
            .expect("the held composite must compress");
        let seconds = start.elapsed().as_secs_f64();
        println!(
            "level {level}: {seconds:.3} s, {} -> {} bytes, ratio {:.4}",
            staged.uncompressed_len,
            staged.compressed_len,
            staged.uncompressed_len as f64 / staged.compressed_len as f64
        );
        staged.remove();
    }
}

/// The reference stack's frames, or `None` when it is not configured.
fn reference_frames() -> Option<Vec<PathBuf>> {
    let dir = common::reference_stack_dir()?;
    Some(
        frame_paths(&dir)
            .expect("the reference stack directory must enumerate")
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains("IMG_"))
            })
            .collect(),
    )
}

#[test]
#[ignore = "reads the full reference stack; minutes"]
fn timings_on_reference_stack() {
    let Some(frames) = reference_frames() else {
        return;
    };
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("save_timing_reference");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).expect("the output directory must be creatable");

    let figures: Vec<Figure> = METHODS
        .into_iter()
        .flat_map(|method| run_scenarios(&frames, method, &out.join(method)))
        .collect();
    let missed: Vec<String> = figures
        .iter()
        .filter_map(|figure| {
            let &(_, target) = TARGETS.iter().find(|(name, _)| *name == figure.scenario)?;
            (figure.warm > target).then(|| {
                format!(
                    "{} {} warm {:.3} s > {target:.1} s",
                    figure.scenario, figure.method, figure.warm
                )
            })
        })
        .collect();
    assert!(
        missed.is_empty(),
        "warm targets missed: {}",
        missed.join("; ")
    );
}
