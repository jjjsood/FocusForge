// SPDX-License-Identifier: GPL-3.0-or-later

mod common;

use std::path::PathBuf;

use focusforge_core::algorithm::{
    AlgorithmError, AlignAlgorithm, AlignContext, AlignOutcome, AlignStatus, StackAlgorithm,
    TileCtx, TileOutput, TileReader,
};
use focusforge_core::align::{FailurePolicy, Transform, align_stack};
use focusforge_core::fuse::{FusionConsumer, FusionGeometry};
use focusforge_core::io::{
    FrameDesc, FrameError, FrameReader, Rect, TiffError, TiffSpec, TiffStack, TiffWriter,
};
use focusforge_core::params::{ParamDesc, ParamValues};
use focusforge_core::tile::{
    NoopPreprocess, SampleFormat, Tile, TileScheduler, TileSchedulerOptions,
};

fn accepts_tile_reader(_: &dyn TileReader) {}

#[test]
fn trait_object_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<&dyn FrameReader>();
    let stack = TiffStack::open(common::mantis_512_dir()).expect("fixture stack must open");
    let reader: &dyn FrameReader = &stack;
    accepts_tile_reader(reader);
    assert_eq!(reader.stack_context().frame_count, reader.len());
}

struct MemoryReader {
    frames: Vec<Vec<u16>>,
    descs: Vec<FrameDesc>,
}

impl MemoryReader {
    fn new() -> Self {
        let frames = (0..3)
            .map(|frame| {
                (0..(40 * 24 * 3) as usize)
                    .map(|sample| (frame * 10_000 + sample % 9_000) as u16)
                    .collect()
            })
            .collect();
        let descs = (0..3)
            .map(|index| FrameDesc {
                path: PathBuf::from(format!("memory-{index}.tif")),
                file_size: 0,
                image_description: None,
                width: 40,
                height: 24,
                samples_per_pixel: 3,
                strip_offsets: Vec::new(),
                strip_byte_counts: Vec::new(),
                raw_identity: None,
                layout_label: "memory".to_string(),
                layout_signature: vec!["memory".to_string()],
            })
            .collect();
        Self { frames, descs }
    }
}

impl TileReader for MemoryReader {
    fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), AlgorithmError> {
        let source = self
            .frames
            .get(frame)
            .ok_or(AlgorithmError::FrameOutOfRange {
                frame,
                frame_count: self.frames.len(),
            })?;
        let in_bounds = rect.width > 0
            && rect.height > 0
            && rect.x.checked_add(rect.width).is_some_and(|x| x <= 40)
            && rect.y.checked_add(rect.height).is_some_and(|y| y <= 24);
        if !in_bounds {
            return Err(AlgorithmError::Frame(FrameError::Tiff(
                TiffError::OutOfBounds {
                    path: PathBuf::from("memory"),
                    requested: rect,
                    frame: (40, 24),
                },
            )));
        }
        let expected = rect.width as usize * rect.height as usize * 3;
        if out.len() != expected {
            return Err(AlgorithmError::Frame(FrameError::Tiff(
                TiffError::BufferSize {
                    expected,
                    got: out.len(),
                },
            )));
        }
        let stride = 40 * 3;
        let row_len = rect.width as usize * 3;
        for (out_row, source_y) in (rect.y..rect.y + rect.height).enumerate() {
            let source_start = source_y as usize * stride + rect.x as usize * 3;
            let out_start = out_row * row_len;
            out[out_start..out_start + row_len]
                .copy_from_slice(&source[source_start..source_start + row_len]);
        }
        Ok(())
    }
}

impl FrameReader for MemoryReader {
    fn len(&self) -> usize {
        self.frames.len()
    }

    fn dimensions(&self) -> (u32, u32) {
        (40, 24)
    }

    fn frame(&self, index: usize) -> &FrameDesc {
        &self.descs[index]
    }
}

#[test]
fn out_of_bounds_and_buffer_size_errors_pass_through() {
    let source = MemoryReader::new();
    let stack = write_tiff_stack("boundary", &source.frames);
    let reader: &dyn FrameReader = &stack;
    for rect in [
        Rect {
            x: 40,
            y: 0,
            width: 1,
            height: 1,
        },
        Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 1,
        },
        Rect {
            x: u32::MAX,
            y: 0,
            width: 2,
            height: 1,
        },
    ] {
        let mut out = vec![9; 3];
        assert!(matches!(
            reader.read_tile_into(0, rect, &mut out),
            Err(AlgorithmError::Frame(FrameError::Tiff(
                TiffError::OutOfBounds { .. }
            )))
        ));
        assert_eq!(out, vec![9; 3]);
    }
    let full = Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 24,
    };
    for len in [1, 40 * 24 * 3 + 1] {
        let mut out = vec![7; len];
        assert!(matches!(
            reader.read_tile_into(0, full, &mut out),
            Err(AlgorithmError::Frame(FrameError::Tiff(
                TiffError::BufferSize { .. }
            )))
        ));
        assert!(out.iter().all(|sample| *sample == 7));
    }
}

#[test]
fn f32_read_rejects_out_of_frame_rects_and_wrong_lengths() {
    let source = MemoryReader::new();
    let stack = write_tiff_stack("f32-boundary", &source.frames);
    let reader: &dyn TileReader = &stack;
    for rect in [
        Rect {
            x: 40,
            y: 0,
            width: 1,
            height: 1,
        },
        Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 1,
        },
        Rect {
            x: u32::MAX,
            y: 0,
            width: 2,
            height: 1,
        },
    ] {
        let mut out = vec![9.0f32; 3];
        let error = reader
            .read_tile_f32_into(0, rect, &mut out)
            .expect_err("a rect outside the frame is refused");
        assert!(error.to_string().contains("outside"), "{error}");
        assert_eq!(out, vec![9.0; 3]);
        let mut gained = vec![9.0f32; 3];
        reader
            .read_tile_gained_f32_into(0, rect, 1 << 16, &mut gained)
            .expect_err("the gained read refuses it too");
        assert_eq!(gained, vec![9.0; 3]);
        let mut covered = vec![9.0f32; 3];
        reader
            .read_tile_coverage_f32_into(0, rect, &mut covered)
            .expect_err("the coverage read refuses it too");
        assert_eq!(covered, vec![9.0; 3]);
    }
    let full = Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 24,
    };
    for len in [40 * 24 * 3 - 1, 40 * 24 * 3 + 1] {
        let mut out = vec![7.0f32; len];
        let error = reader
            .read_tile_f32_into(0, full, &mut out)
            .expect_err("a wrong length is refused");
        assert!(error.to_string().contains(&len.to_string()), "{error}");
        assert!(out.iter().all(|sample| *sample == 7.0));
    }
}

#[test]
fn f32_read_widens_the_u16_read() {
    let source = MemoryReader::new();
    let stack = write_tiff_stack("f32-widen", &source.frames);
    let reader: &dyn TileReader = &stack;
    assert_eq!(reader.sample_format(), SampleFormat::U16);
    let rect = Rect {
        x: 3,
        y: 2,
        width: 20,
        height: 11,
    };
    let mut narrow = vec![0u16; 20 * 11 * 3];
    reader.read_tile_into(1, rect, &mut narrow).unwrap();
    let mut wide = vec![-1.0f32; narrow.len()];
    reader.read_tile_f32_into(1, rect, &mut wide).unwrap();
    assert!(narrow.iter().zip(&wide).all(|(n, w)| f32::from(*n) == *w));
    let mut covered = vec![-1.0f32; narrow.len()];
    assert!(
        reader
            .read_tile_coverage_f32_into(1, rect, &mut covered)
            .unwrap()
            .is_none()
    );
    assert_eq!(covered, wide);
    let mut gained = vec![-1.0f32; narrow.len()];
    reader
        .read_tile_gained_f32_into(1, rect, 1 << 15, &mut gained)
        .unwrap();
    assert!(narrow.iter().zip(&gained).all(|(n, g)| {
        let half = ((u32::from(*n) << 15) + (1 << 15)) >> 16;
        f32::from(half.min(65_535) as u16) == *g
    }));
}

struct SourceSelector;

impl StackAlgorithm for SourceSelector {
    fn id(&self) -> &str {
        "source-selector"
    }

    fn display_name(&self) -> &str {
        "Source Selector"
    }

    fn params(&self) -> &[ParamDesc] {
        &[]
    }

    fn fuse(&self, ctx: &TileCtx<'_>, _: &ParamValues) -> Result<TileOutput, AlgorithmError> {
        let rect = ctx.rect();
        let spp = ctx.samples_per_pixel()? as usize;
        let mut samples = vec![0; rect.width as usize * rect.height as usize * spp];
        let mut zmap = Vec::with_capacity(rect.width as usize * rect.height as usize);
        for pixel in 0..rect.width as usize * rect.height as usize {
            let source = (rect.x as usize + pixel % rect.width as usize) % ctx.frame_count();
            zmap.push(source as u16);
        }
        ctx.for_each_frame(|f, tile, _| {
            for (pixel, &source) in zmap.iter().enumerate() {
                if source as usize == f {
                    let start = pixel * spp;
                    samples[start..start + spp].copy_from_slice(&tile.samples[start..start + spp]);
                }
            }
            Ok(())
        })?;
        Ok(TileOutput {
            tile: Tile {
                rect,
                samples_per_pixel: spp as u32,
                samples,
            },
            zmap,
        })
    }
}

struct IdentityAligner;

impl AlignAlgorithm for IdentityAligner {
    fn id(&self) -> &str {
        "identity"
    }

    fn display_name(&self) -> &str {
        "Identity"
    }

    fn params(&self) -> &[ParamDesc] {
        &[]
    }

    fn align_frame(
        &self,
        ctx: &AlignContext<'_>,
        frame: usize,
        _: &ParamValues,
    ) -> Result<AlignOutcome, AlgorithmError> {
        let mut samples = vec![0; ctx.stack().samples_per_pixel as usize];
        ctx.read_tile_into(
            frame,
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            &mut samples,
        )?;
        assert!(samples.iter().any(|sample| *sample != 0));
        Ok(AlignOutcome {
            transform: Transform::identity(),
            status: AlignStatus::Converged,
            iterations: 0,
            residual: 1.0,
        })
    }
}

static SOURCE_SELECTOR: SourceSelector = SourceSelector;
static IDENTITY_ALIGNER: IdentityAligner = IdentityAligner;

fn write_tiff_stack(label: &str, frames: &[Vec<u16>]) -> TiffStack {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("frame-reader-{label}"));
    std::fs::create_dir_all(&dir).expect("test frame directory must be creatable");
    let mut paths = Vec::new();
    let full = Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 24,
    };
    for (index, samples) in frames.iter().enumerate() {
        let path = dir.join(format!("{index:02}.tif"));
        let mut writer = TiffWriter::create(
            &path,
            TiffSpec {
                width: 40,
                height: 24,
                samples_per_pixel: 3,
                description: None,
            },
        )
        .expect("writer must create frame");
        writer
            .write_tile(full, samples)
            .expect("writer must accept frame");
        writer.finish().expect("writer must finish frame");
        paths.push(path);
    }
    TiffStack::open_files(&paths).expect("written frames must open")
}

fn run_pipeline(label: &str, reader: &dyn FrameReader) -> (Vec<u16>, Vec<u16>) {
    let root = common::workspace_root()
        .join(".scratch")
        .join("frame-reader");
    std::fs::create_dir_all(&root).expect("scratch directory must be creatable");
    let composite_path = root.join(format!("{label}.tif"));
    let zmap_path = root.join(format!("{label}-zmap.tif"));
    let mut options =
        TileSchedulerOptions::explicit_budget(root.join(format!("{label}-tiles")), 1 << 30);
    options.tile_size = 16;
    options.levels = 0;
    let scheduler = TileScheduler::new(reader, reader.stack_context(), 3, &options)
        .expect("scheduler must accept reader");
    let consumer = FusionConsumer::create(
        &SOURCE_SELECTOR,
        ParamValues::new(),
        &composite_path,
        Some(&zmap_path),
        FusionGeometry {
            width: 40,
            height: 24,
            samples_per_pixel: 3,
            frame_count: 3,
            crop: None,
            description: None,
        },
    )
    .expect("consumer must create");
    scheduler
        .run(&NoopPreprocess, &consumer)
        .expect("pipeline must run");
    consumer.finish().expect("consumer must finish");
    let full = Rect {
        x: 0,
        y: 0,
        width: 40,
        height: 24,
    };
    let composite = TiffStack::open_files(&[composite_path])
        .expect("composite must open")
        .read_tile(0, full)
        .expect("composite must read")
        .samples;
    let zmap = TiffStack::open_files(&[zmap_path])
        .expect("z-map must open")
        .read_tile(0, full)
        .expect("z-map must read")
        .samples;
    (composite, zmap)
}

#[test]
fn an_in_memory_reader_drives_the_pipeline() {
    let memory = MemoryReader::new();
    let tiff = write_tiff_stack("source", &memory.frames);
    let values = ParamValues::new();
    let memory_alignment = align_stack(
        &IDENTITY_ALIGNER,
        memory.stack_context(),
        &memory,
        &values,
        FailurePolicy::Strict,
    )
    .expect("memory alignment must run");
    let tiff_alignment = align_stack(
        &IDENTITY_ALIGNER,
        tiff.stack_context(),
        &tiff,
        &values,
        FailurePolicy::Strict,
    )
    .expect("TIFF alignment must run");
    assert_eq!(memory_alignment.frames, tiff_alignment.frames);

    let (memory_composite, memory_zmap) = run_pipeline("memory", &memory);
    let (tiff_composite, tiff_zmap) = run_pipeline("tiff", &tiff);
    assert_eq!(memory_composite, tiff_composite);
    assert_eq!(memory_zmap, tiff_zmap);
    assert!(memory_composite.iter().any(|sample| *sample != 0));
    for frame in 0..3 {
        assert!(memory_zmap.contains(&frame));
    }
}
