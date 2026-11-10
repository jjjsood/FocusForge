// SPDX-License-Identifier: GPL-3.0-or-later

//! A stroke painted from a checkpoint blends that checkpoint's own render
//! (its result plus its strokes) over the stroke's footprint: in every brush
//! mode and sample type, through tiles, in a view and in an export, and a
//! reference that cannot be rendered is refused before anything is written.

use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};

use focusforge_core::align::{Registration, WarpingReader};
use focusforge_core::equalize::GainReader;
use focusforge_core::io::{Rect, TiffSpec, TiffStack, TiffWriter, open_frame_set};
use focusforge_core::params::ParamValues;
use focusforge_core::project::export::{ExportOptions, MetadataOptions};
use focusforge_core::project::{
    self, AppendRequest, BrushMode, CheckpointImage, Metrics, Op, ParentRef, PendingState, Point,
    Project, ProjectError, SealRequest, StrokeSource, partial_path,
};
use focusforge_core::retouch::blend::{self, Blend};
use focusforge_core::tile::HostBudget;
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};

const W: u32 = 61;
const H: u32 = 47;
const FRAMES: usize = 3;
/// Where the cropped composite sits in the (larger) source frames.
const CROP: Rect = Rect {
    x: 3,
    y: 2,
    width: W,
    height: H,
};

static CLOCK: AtomicI64 = AtomicI64::new(10);

fn full() -> Rect {
    Rect {
        x: 0,
        y: 0,
        width: W,
        height: H,
    }
}

fn tmp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A sample type the documents are built and rendered in.
trait Doc: Blend + Copy + PartialEq + Debug {
    const WIDE: bool;
    /// `tile` of `image`'s render in this type.
    fn render(image: &CheckpointImage, project: &mut Project, tile: Rect) -> Vec<Self>;
    fn read(path: &Path) -> Vec<Self>;
    /// The sample as the 16-bit view and export narrow it.
    fn narrowed(self) -> u16;
}

fn gather<S: Copy + Default>(out: &mut [S], tile: Rect, spp: usize, band: &[S], rect: Rect) {
    let len = rect.width as usize * spp;
    for (r, line) in band.chunks_exact(len).enumerate() {
        let at =
            ((rect.y - tile.y) as usize + r) * tile.width as usize + (rect.x - tile.x) as usize;
        out[at * spp..at * spp + len].copy_from_slice(line);
    }
}

impl Doc for u16 {
    const WIDE: bool = false;
    fn narrowed(self) -> u16 {
        self
    }
    fn render(image: &CheckpointImage, project: &mut Project, tile: Rect) -> Vec<u16> {
        let spp = image.samples_per_pixel as usize;
        let mut out = vec![0u16; tile.width as usize * tile.height as usize * spp];
        image
            .render(Some(project), tile, &mut |band, rect| {
                gather(&mut out, tile, spp, band, rect);
                Ok(())
            })
            .unwrap();
        out
    }
    fn read(path: &Path) -> Vec<u16> {
        let stack = TiffStack::open_staged(path).unwrap();
        let mut out = vec![0u16; (W * H * stack.header(0).samples_per_pixel) as usize];
        <u16 as focusforge_core::tile::Sample>::read_tile(&stack, 0, full(), &mut out).unwrap();
        out
    }
}

impl Doc for f32 {
    const WIDE: bool = true;
    fn narrowed(self) -> u16 {
        <u16 as focusforge_core::tile::Sample>::narrow(self)
    }
    fn render(image: &CheckpointImage, project: &mut Project, tile: Rect) -> Vec<f32> {
        let spp = image.samples_per_pixel as usize;
        let mut out = vec![0f32; tile.width as usize * tile.height as usize * spp];
        image
            .render_f32(Some(project), tile, &mut |band, rect| {
                gather(&mut out, tile, spp, band, rect);
                Ok(())
            })
            .unwrap();
        out
    }
    fn read(path: &Path) -> Vec<f32> {
        let stack = TiffStack::open_staged(path).unwrap();
        let mut out = vec![0f32; (W * H * stack.header(0).samples_per_pixel) as usize];
        <f32 as focusforge_core::tile::Sample>::read_tile(&stack, 0, full(), &mut out).unwrap();
        out
    }
}

/// A project of one fused root over `FRAMES` cropped source frames, and the
/// files it was made from.
struct Project1 {
    dir: PathBuf,
    path: PathBuf,
    root: String,
    composite: PathBuf,
    sources: Vec<PathBuf>,
    registration: Registration,
    spp: u32,
}

fn hash(a: usize, b: usize, c: usize, d: usize) -> u16 {
    ((a * 7919 + b * 131 + c * 257 + d * 1009 + (b * c) % 97) % 65_536) as u16
}

fn write_sources(dir: &Path, spp: u32, seed: usize) -> Vec<PathBuf> {
    let (w, h) = (W + 2 * CROP.x, H + 2 * CROP.y);
    (0..FRAMES)
        .map(|i| {
            let path = dir.join(format!("frame-{seed}-{i}.tif"));
            let mut writer = TiffWriter::create(
                &path,
                TiffSpec {
                    width: w,
                    height: h,
                    samples_per_pixel: spp,
                    description: None,
                },
            )
            .unwrap();
            let samples: Vec<u16> = (0..(w * h * spp) as usize)
                .map(|j| {
                    let p = j / spp as usize;
                    hash(
                        i + 1 + seed * 5,
                        p % w as usize,
                        p / w as usize,
                        j % spp as usize,
                    )
                })
                .collect();
            writer
                .write_tile(
                    Rect {
                        x: 0,
                        y: 0,
                        width: w,
                        height: h,
                    },
                    &samples,
                )
                .unwrap();
            writer.finish().unwrap();
            path
        })
        .collect()
}

fn stage_composite(dir: &Path, name: &str, spp: u32, wide: bool) -> PathBuf {
    let path = dir.join(name);
    let spec = TiffSpec {
        width: W,
        height: H,
        samples_per_pixel: spp,
        description: Some("staged composite".to_string()),
    };
    let len = (W * H * spp) as usize;
    if wide {
        let samples: Vec<f32> = (0..len)
            .map(|i| {
                f32::from(hash(0, i % 89, i / 89, i % 3))
                    + 0.25
                    + if i % 11 == 0 { 5000.0 } else { 0.0 }
            })
            .collect();
        let mut writer = TiffWriter::create_f32(&path, spec).unwrap();
        writer.write_tile_f32(full(), &samples).unwrap();
        writer.finish().unwrap();
    } else {
        let samples: Vec<u16> = (0..len).map(|i| hash(0, i % 89, i / 89, i % 3)).collect();
        let mut writer = TiffWriter::create(&path, spec).unwrap();
        writer.write_tile(full(), &samples).unwrap();
        writer.finish().unwrap();
    }
    path
}

fn stage_zmap(dir: &Path) -> PathBuf {
    let path = dir.join("zmap.tif");
    let mut map = ZMapWriter::create(&path, ZMapInfo::new(W, H, FRAMES as u32).unwrap()).unwrap();
    map.write_tile(full(), &vec![0u16; (W * H) as usize])
        .unwrap();
    map.finish().unwrap();
    path
}

fn cropped(rect: Rect) -> Registration {
    Registration {
        crop: Some(rect),
        ..Registration::default()
    }
}

fn build<S: Doc>(name: &str, spp: u32) -> Project1 {
    let dir = tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let composite = stage_composite(&staging, "composite.tif", spp, S::WIDE);
    let zmap = stage_zmap(&staging);
    let sources = write_sources(&dir, spp, 0);
    let registration = cropped(CROP);
    let path = dir.join(format!("demo.{}", project::PROJECT_EXTENSION));
    let sealed = project::seal(SealRequest {
        project_path: &path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources: &sources,
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        registration: registration.clone(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");
    Project1 {
        dir,
        path,
        root: sealed.checkpoint,
        composite,
        sources,
        registration,
        spp,
    }
}

/// A second fuse appended onto `p`, over `sources` with `registration`, its
/// composite in `wide` samples.
fn append_fuse(
    p: &Project1,
    tag: &str,
    sources: &[PathBuf],
    registration: Registration,
    wide: bool,
) -> String {
    let staging = p.dir.join(format!("staging-{tag}"));
    std::fs::create_dir_all(&staging).unwrap();
    let composite = stage_composite(&staging, "composite.tif", p.spp, wide);
    let zmap = stage_zmap(&staging);
    project::append(AppendRequest {
        project_path: &p.path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources,
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        registration,
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("append must succeed")
    .checkpoint
}

fn append_ops(p: &Project1, parent: &str, ops: Vec<Op>) -> String {
    let at = CLOCK.fetch_add(1, Ordering::SeqCst);
    project::append_ops_onto(&p.path, parent, ops, None, at)
        .expect("the strokes must append")
        .checkpoint
}

fn stroke(
    source: StrokeSource,
    at: &[(i32, i32)],
    radius: f32,
    mode: BrushMode,
    opacity: f32,
) -> Op {
    Op::BrushStroke {
        source,
        path: at.iter().map(|&(x, y)| Point { x, y }).collect(),
        radius,
        softness: 0.5,
        mode,
        opacity,
    }
}

fn frame(n: u16) -> StrokeSource {
    StrokeSource::Frame(n)
}

fn from(id: &str) -> StrokeSource {
    StrokeSource::Checkpoint(id.to_string())
}

/// The image of checkpoint `id`: its fused ancestor's staged composite with
/// its op chain blended over the whole of it in one `apply_strokes` call, each
/// checkpoint stroke reading the full-image oracle of the checkpoint it names.
fn oracle<S: Doc>(p: &Project1, id: &str) -> Vec<S> {
    let project = Project::open(&p.path).unwrap();
    let ops: Vec<Op> = project
        .history()
        .op_chain(id)
        .unwrap()
        .into_iter()
        .cloned()
        .collect();
    oracle_of::<S>(p, &ops)
}

fn oracle_of<S: Doc>(p: &Project1, ops: &[Op]) -> Vec<S> {
    let info = ZMapInfo::new(W, H, FRAMES as u32).unwrap();
    let mut out = S::read(&p.composite);
    let set = open_frame_set(&p.sources, HostBudget::explicit(1 << 30)).unwrap();
    let gained = GainReader::new(&*set.reader, p.registration.gains());
    let warping = WarpingReader::new(
        &gained,
        set.reader.dimensions(),
        p.spp,
        p.registration.transforms(FRAMES),
    );
    let mut rendered: Vec<(Rect, Vec<S>)> = Vec::new();
    let mut placed = Vec::new();
    for op in ops {
        let Some(footprint) = blend::footprint(op, info).unwrap() else {
            continue;
        };
        let Op::BrushStroke { source, .. } = op else {
            unreachable!("brush strokes are the only op")
        };
        let source = match source {
            StrokeSource::Frame(n) => blend::PlacedSource::Frame(usize::from(*n)),
            StrokeSource::Checkpoint(id) => {
                rendered.push((full(), oracle::<S>(p, id)));
                blend::PlacedSource::Rendered(rendered.len() - 1)
            }
        };
        placed.push(blend::PlacedStroke {
            op,
            source,
            footprint,
        });
    }
    blend::apply_strokes(
        info,
        full(),
        &mut out,
        &placed,
        &warping,
        CROP_ORIGIN,
        p.spp as usize,
        &rendered,
        blend::apply::<S>,
    )
    .unwrap();
    out
}

const CROP_ORIGIN: (u32, u32) = (CROP.x, CROP.y);

fn render_full<S: Doc>(p: &Project1, id: &str) -> Vec<S> {
    let mut project = Project::open(&p.path).unwrap();
    let image = CheckpointImage::prepare(&project, id).unwrap();
    S::render(&image, &mut project, image.full())
}

/// A root, a checkpoint `a` of frame strokes over it, and `b` over `a` with a
/// stroke in each mode at each opacity, alternating frames, `a` and the root
/// as sources.
fn mixed<S: Doc>(name: &str, spp: u32) -> (Project1, String, String) {
    let p = build::<S>(name, spp);
    let a = append_ops(
        &p,
        &p.root,
        vec![
            stroke(frame(1), &[(10, 12), (30, 20)], 6.0, BrushMode::Normal, 1.0),
            stroke(frame(2), &[(25, 30), (45, 36)], 5.0, BrushMode::Normal, 0.5),
        ],
    );
    let mut ops = Vec::new();
    let modes = [
        BrushMode::Normal,
        BrushMode::Lighten,
        BrushMode::Darken,
        BrushMode::Detail,
    ];
    for (k, (mode, opacity)) in modes
        .iter()
        .flat_map(|m| [(*m, 1.0), (*m, 0.5)])
        .enumerate()
    {
        let source = match k % 3 {
            0 => frame((k % FRAMES) as u16),
            1 => from(&a),
            _ => from(&p.root),
        };
        let k = k as i32;
        ops.push(stroke(
            source,
            &[(6 + 6 * k, 8), (14 + 5 * k, 38)],
            7.0,
            mode,
            opacity,
        ));
    }
    let b = append_ops(&p, &a, ops);
    (p, a, b)
}

#[test]
fn modes_match_full_image_oracle() {
    fn check<S: Doc>(spp: u32) {
        let (p, a, b) = mixed::<S>(&format!("cs_modes_{}_{spp}", S::WIDE), spp);
        for id in [&a, &b] {
            let want = oracle::<S>(&p, id);
            assert_eq!(
                render_full::<S>(&p, id),
                want,
                "wide {} spp {spp} {id}",
                S::WIDE
            );
        }
        assert_ne!(
            oracle::<S>(&p, &b),
            oracle::<S>(&p, &a),
            "the checkpoint strokes must change something"
        );
    }
    for spp in [1, 3] {
        check::<u16>(spp);
        check::<f32>(spp);
    }
}

#[test]
fn nested_checkpoint_source() {
    let p = build::<u16>("cs_nested", 3);
    let centre = [(30, 20)];
    let b = append_ops(
        &p,
        &p.root,
        vec![stroke(frame(2), &centre, 6.0, BrushMode::Normal, 1.0)],
    );
    let c = append_ops(
        &p,
        &p.root,
        vec![stroke(from(&b), &centre, 10.0, BrushMode::Normal, 1.0)],
    );
    let d = append_ops(
        &p,
        &p.root,
        vec![stroke(from(&c), &centre, 4.0, BrushMode::Normal, 1.0)],
    );
    let root = render_full::<u16>(&p, &p.root);
    let from_b = render_full::<u16>(&p, &b);
    let rendered = render_full::<u16>(&p, &d);
    assert_eq!(rendered, oracle::<u16>(&p, &d));
    let at = ((20 * W + 30) * 3) as usize;
    assert_ne!(from_b[at..at + 3], root[at..at + 3], "b changes its centre");
    assert_eq!(
        rendered[at..at + 3],
        from_b[at..at + 3],
        "d shows b's stroke through c"
    );
}

#[test]
fn painting_from_base_restores_bit_identically() {
    fn check<S: Doc>() {
        let p = build::<S>(&format!("cs_restore_{}", S::WIDE), 3);
        let a = append_ops(
            &p,
            &p.root,
            vec![
                stroke(frame(1), &[(5, 5), (55, 40)], 12.0, BrushMode::Normal, 1.0),
                stroke(frame(2), &[(10, 40), (50, 8)], 9.0, BrushMode::Darken, 0.7),
            ],
        );
        let (x, y, radius) = (30, 22, 10.0f32);
        let restored = append_ops(
            &p,
            &a,
            vec![Op::BrushStroke {
                source: from(&p.root),
                path: vec![Point { x, y }],
                radius,
                softness: 0.0,
                mode: BrushMode::Normal,
                opacity: 1.0,
            }],
        );
        let base = render_full::<S>(&p, &p.root);
        let edited = render_full::<S>(&p, &a);
        let result = render_full::<S>(&p, &restored);
        assert_ne!(base, edited, "the edit must differ from the base");
        let (mut inside, mut outside) = (0, 0);
        for py in 0..H {
            for px in 0..W {
                let d = f64::from(px as i32 - x).hypot(f64::from(py as i32 - y));
                let at = ((py * W + px) * 3) as usize;
                let (got, base, edited) =
                    (&result[at..at + 3], &base[at..at + 3], &edited[at..at + 3]);
                if d <= f64::from(radius) - 1.5 {
                    assert_eq!(got, base, "({px}, {py}) is restored");
                    inside += 1;
                } else if d > f64::from(radius) + 1.5 {
                    assert_eq!(got, edited, "({px}, {py}) keeps the edit");
                    outside += 1;
                }
            }
        }
        assert!(inside > 100 && outside > 100, "{inside} {outside}");
    }
    check::<u16>();
    check::<f32>();
}

#[test]
fn tiled_render_equals_full_render() {
    fn check<S: Doc>() {
        let (p, _, b) = mixed::<S>(&format!("cs_tiled_{}", S::WIDE), 3);
        let mut project = Project::open(&p.path).unwrap();
        let image = CheckpointImage::prepare(&project, &b).unwrap();
        let whole = S::render(&image, &mut project, image.full());
        for (cols, rows) in [(13, 7), (13, 16), (13, 1), (61, 47)] {
            let mut tiled = vec![whole[0]; whole.len()];
            let mut y = 0;
            while y < H {
                let mut x = 0;
                while x < W {
                    let tile = Rect {
                        x,
                        y,
                        width: cols.min(W - x),
                        height: rows.min(H - y),
                    };
                    let part = S::render(&image, &mut project, tile);
                    for r in 0..tile.height {
                        let from = (r * tile.width * 3) as usize;
                        let to = (((tile.y + r) * W + tile.x) * 3) as usize;
                        let len = (tile.width * 3) as usize;
                        tiled[to..to + len].copy_from_slice(&part[from..from + len]);
                    }
                    x += cols;
                }
                y += rows;
            }
            assert!(tiled == whole, "wide {} tiles {cols}x{rows}", S::WIDE);
        }
    }
    check::<u16>();
    check::<f32>();
}

fn pending_over(p: &Project1, ops: Vec<Op>) -> PendingState {
    PendingState {
        composite: p.composite.clone(),
        ops,
        sources: p.sources.clone(),
        registration: p.registration.clone(),
        reference: 0,
        description: "staged composite".to_string(),
    }
}

/// Unsaved strokes painted from the saved checkpoints `a` and `b`, one in
/// Detail mode.
fn pending_strokes(p: &Project1, a: &str, b: &str) -> PendingState {
    pending_over(
        p,
        vec![
            stroke(frame(1), &[(8, 30), (50, 12)], 6.0, BrushMode::Normal, 1.0),
            stroke(from(b), &[(10, 10), (40, 40)], 9.0, BrushMode::Detail, 0.8),
            stroke(from(a), &[(20, 5), (30, 44)], 6.0, BrushMode::Lighten, 1.0),
        ],
    )
}

#[test]
fn export_equals_view() {
    fn check<S: Doc>() {
        let (p, a, b) = mixed::<S>(&format!("cs_export_{}", S::WIDE), 3);
        let mut project = Project::open(&p.path).unwrap();
        let image = CheckpointImage::prepare(&project, &b).unwrap();
        let view = u16::render(&image, &mut project, image.full());
        let out = p.dir.join("saved.tif");
        project::export_checkpoint(
            &mut project,
            &b,
            &out,
            &ExportOptions::default(),
            &MetadataOptions::default(),
        )
        .expect("the export must write");
        assert!(u16::read(&out) == view, "saved export, wide {}", S::WIDE);

        let state = pending_strokes(&p, &a, &b);
        let image = CheckpointImage::prepare_pending(&state, Some(&project)).unwrap();
        let view = u16::render(&image, &mut project, image.full());
        assert_ne!(view, u16::read(&out));
        let narrowed: Vec<u16> = oracle_of::<S>(&p, &state.ops)
            .iter()
            .map(|v| v.narrowed())
            .collect();
        assert!(view == narrowed, "pending view, wide {}", S::WIDE);
        let out = p.dir.join("pending.tif");
        project::export_pending(
            &state,
            Some(&mut project),
            &out,
            &ExportOptions::default(),
            &MetadataOptions::default(),
        )
        .expect("the pending export must write");
        assert!(u16::read(&out) == view, "pending export, wide {}", S::WIDE);
    }
    check::<u16>();
    check::<f32>();
}

#[test]
fn source_budget_split_equals_unsplit_render() {
    fn check<S: Doc>() {
        let (p, a, b) = mixed::<S>(&format!("cs_budget_{}", S::WIDE), 3);
        let mut project = Project::open(&p.path).unwrap();
        let saved = CheckpointImage::prepare(&project, &b).unwrap();
        let state = pending_strokes(&p, &a, &b);
        let pending = CheckpointImage::prepare_pending(&state, Some(&project)).unwrap();
        for (name, image) in [("saved", saved), ("pending", pending)] {
            let tiles = [
                image.full(),
                Rect {
                    x: 9,
                    y: 4,
                    width: 40,
                    height: 31,
                },
            ];
            let unsplit: Vec<Vec<S>> = tiles
                .iter()
                .map(|&t| S::render(&image, &mut project, t))
                .collect();
            let mut image = image;
            // Zero forces single rows; the other leaves a few rows a chunk.
            for budget in [0, 61 * 3 * 4 * 5] {
                image = image.with_source_bytes(budget);
                for (&tile, want) in tiles.iter().zip(&unsplit) {
                    let got = S::render(&image, &mut project, tile);
                    assert!(
                        got == *want,
                        "{name} wide {} budget {budget} {tile:?}",
                        S::WIDE
                    );
                }
            }
        }
    }
    check::<u16>();
    check::<f32>();
}

fn refusal(err: ProjectError) -> (usize, String, String) {
    match err {
        ProjectError::CheckpointSourceRefused {
            index,
            checkpoint,
            reason,
            ..
        } => (index, checkpoint, reason),
        other => panic!("expected a refused checkpoint source, got {other:?}"),
    }
}

#[test]
fn refuses_unrenderable_checkpoint_source() {
    let p = build::<u16>("cs_refused", 3);
    let other_dir = p.dir.join("other-content");
    std::fs::create_dir_all(&other_dir).unwrap();
    let other_sources = write_sources(&other_dir, 3, 1);
    let shifted = append_fuse(
        &p,
        "crop",
        &p.sources,
        cropped(Rect { x: 4, y: 3, ..CROP }),
        false,
    );
    let other = append_fuse(&p, "frames", &other_sources, cropped(CROP), false);
    let wide = append_fuse(&p, "wide", &p.sources, cropped(CROP), true);

    for (reference, reason) in [
        (&shifted, "has different crop"),
        (&other, "has different source frames"),
        (&wide, "holds f32 samples, not 16-bit"),
    ] {
        let bad = append_ops(
            &p,
            &p.root,
            vec![
                stroke(frame(0), &[(5, 5)], 3.0, BrushMode::Normal, 1.0),
                stroke(from(reference), &[(20, 20)], 4.0, BrushMode::Normal, 1.0),
            ],
        );
        let mut project = Project::open(&p.path).unwrap();
        let out = p.dir.join("refused.tif");
        let err = project::render_checkpoint(&mut project, &bad, &out, None).unwrap_err();
        assert_eq!(refusal(err), (1, reference.clone(), reason.to_string()));
        assert!(!out.exists() && !partial_path(&out).exists(), "{reason}");
        let err = project::export_checkpoint(
            &mut project,
            &bad,
            &out,
            &ExportOptions::default(),
            &MetadataOptions::default(),
        )
        .unwrap_err();
        assert_eq!(refusal(err).2, reason);
        assert!(!out.exists() && !partial_path(&out).exists(), "{reason}");
    }

    let project = Project::open(&p.path).unwrap();
    let pending = |ops: Vec<Op>, registration: Registration| PendingState {
        registration,
        ..pending_over(&p, ops)
    };
    let unknown = "0".repeat(32);
    let mut project_mut = Project::open(&p.path).unwrap();
    for (ops, registration, project, reason) in [
        (
            vec![stroke(
                from(&unknown),
                &[(9, 9)],
                3.0,
                BrushMode::Normal,
                1.0,
            )],
            cropped(CROP),
            Some(&project),
            "is not in this project",
        ),
        (
            vec![stroke(
                from(&p.root),
                &[(9, 9)],
                3.0,
                BrushMode::Normal,
                1.0,
            )],
            cropped(Rect { x: 4, ..CROP }),
            Some(&project),
            "has different crop",
        ),
        (
            vec![stroke(from(&wide), &[(9, 9)], 3.0, BrushMode::Normal, 1.0)],
            cropped(CROP),
            Some(&project),
            "holds f32 samples, not 16-bit",
        ),
        (
            vec![
                stroke(frame(1), &[(2, 2)], 3.0, BrushMode::Normal, 1.0),
                stroke(from(&p.root), &[(9, 9)], 3.0, BrushMode::Normal, 1.0),
            ],
            cropped(CROP),
            None,
            "needs its project, and none is open",
        ),
    ] {
        let state = pending(ops, registration);
        let err = match CheckpointImage::prepare_pending(&state, project) {
            Err(err) => err,
            Ok(_) => panic!("{reason}: the state must be refused"),
        };
        let (index, checkpoint, got) = refusal(err);
        assert_eq!(got, reason);
        assert_eq!(index, state.ops.len() - 1);
        let Some(Op::BrushStroke {
            source: StrokeSource::Checkpoint(named),
            ..
        }) = state.ops.last()
        else {
            panic!("the last stroke is painted from a checkpoint");
        };
        assert_eq!(
            &checkpoint, named,
            "{reason}: the refusal names the reference"
        );
        let out = p.dir.join("pending-refused.tif");
        let err = project::export_pending(
            &state,
            project.map(|_| &mut project_mut),
            &out,
            &ExportOptions::default(),
            &MetadataOptions::default(),
        )
        .unwrap_err();
        assert_eq!(refusal(err).2, reason);
        assert!(!out.exists() && !partial_path(&out).exists(), "{reason}");
    }
    let message = ProjectError::CheckpointSourceRefused {
        path: p.path.clone(),
        index: 3,
        checkpoint: "abc".to_string(),
        reason: "has different alignment".to_string(),
    }
    .to_string();
    assert_eq!(
        message,
        "op #3 is painted from checkpoint abc, which has different alignment"
    );
}

#[test]
fn hidden_source_still_renders() {
    let p = build::<u16>("cs_hidden", 3);
    let a = append_ops(
        &p,
        &p.root,
        vec![stroke(
            frame(1),
            &[(20, 20), (40, 30)],
            8.0,
            BrushMode::Normal,
            1.0,
        )],
    );
    append_ops(
        &p,
        &a,
        vec![stroke(frame(2), &[(10, 10)], 5.0, BrushMode::Normal, 1.0)],
    );
    let x = append_ops(
        &p,
        &p.root,
        vec![stroke(
            from(&a),
            &[(25, 22), (38, 28)],
            7.0,
            BrushMode::Normal,
            0.8,
        )],
    );
    let visible = render_full::<u16>(&p, &x);
    assert_eq!(visible, oracle::<u16>(&p, &x));

    project::seal_in_memory(&p.path, &[], &[ParentRef::Saved(a.clone())], None, true).unwrap();
    assert!(
        Project::open(&p.path)
            .unwrap()
            .history()
            .get(&a)
            .unwrap()
            .hidden
    );
    assert_eq!(render_full::<u16>(&p, &x), visible);
}

/// Wall-clock cost of a stroke painted from the fused base against the same
/// stroke painted from a frame, on a 44.8 MP project. Run in release mode:
/// `cargo test --release -p focusforge-core --test checkpoint_source_render -- --ignored --nocapture`.
#[test]
#[ignore = "measurement: prints the render time of a frame stroke and of a base-checkpoint stroke on an 8192x5464x3 project"]
fn checkpoint_source_cost() {
    const BIG_W: u32 = 8192;
    const BIG_H: u32 = 5464;
    const BAND: u32 = 256;
    let dir = tmp_dir("cs_cost");
    let band = |y: u32| Rect {
        x: 0,
        y,
        width: BIG_W,
        height: BAND.min(BIG_H - y),
    };
    let pixels = |r: Rect, seed: usize, spp: usize| -> Vec<u16> {
        (0..r.width as usize * r.height as usize * spp)
            .map(|i| ((i * 31 + seed * 977 + r.y as usize) % 65_536) as u16)
            .collect()
    };
    let write_plane = |path: &Path, seed: usize| {
        let mut writer = TiffWriter::create(
            path,
            TiffSpec {
                width: BIG_W,
                height: BIG_H,
                samples_per_pixel: 3,
                description: Some("staged composite".to_string()),
            },
        )
        .unwrap();
        for y in (0..BIG_H).step_by(BAND as usize) {
            writer
                .write_tile(band(y), &pixels(band(y), seed, 3))
                .unwrap();
        }
        writer.finish().unwrap();
    };
    let composite = dir.join("composite.tif");
    write_plane(&composite, 0);
    let zmap = dir.join("zmap.tif");
    let mut map = ZMapWriter::create(&zmap, ZMapInfo::new(BIG_W, BIG_H, 2).unwrap()).unwrap();
    for y in (0..BIG_H).step_by(BAND as usize) {
        let r = band(y);
        map.write_tile(r, &vec![0u16; (r.width * r.height) as usize])
            .unwrap();
    }
    map.finish().unwrap();
    let sources: Vec<PathBuf> = (1..=2)
        .map(|i| {
            let path = dir.join(format!("frame-{i}.tif"));
            write_plane(&path, i);
            path
        })
        .collect();
    let path = dir.join(format!("big.{}", project::PROJECT_EXTENSION));
    let root = project::seal(SealRequest {
        project_path: &path,
        staged_composite: &composite,
        staged_zmap: &zmap,
        sources: &sources,
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .unwrap()
    .checkpoint;
    let centre = [(4096, 2732)];
    let at = |source| vec![stroke(source, &centre, 100.0, BrushMode::Normal, 1.0)];
    let from_frame = project::append_ops_onto(&path, &root, at(frame(1)), None, 1)
        .unwrap()
        .checkpoint;
    let from_base = project::append_ops_onto(&path, &root, at(from(&root)), None, 2)
        .unwrap()
        .checkpoint;
    let mut project = Project::open(&path).unwrap();
    let mut time = |id: &str| {
        let out = dir.join("out.tif");
        let start = std::time::Instant::now();
        project::render_checkpoint(&mut project, id, &out, None).unwrap();
        start.elapsed().as_secs_f64()
    };
    let (frame_s, base_s) = (time(&from_frame), time(&from_base));
    eprintln!(
        "frame stroke {frame_s:.2} s, base-checkpoint stroke {base_s:.2} s, ratio {:.2}",
        base_s / frame_s
    );
}
