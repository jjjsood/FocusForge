// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli export`'s per-format options: TIFF bit depth and
//! compression, PNG bit depth and compression, JPEG quality and chroma
//! subsampling, extension inference and every refusal. A single-frame
//! gradient project, sized off a multiple of 16 and spanning more than one
//! export band, gives compression and 8-bit rounding something to do.

mod common;
#[path = "../../core/tests/common/raw_dng.rs"]
mod raw_dng;

use std::path::{Path, PathBuf};
use std::process::Command;

use focusforge_core::align::{Registration, Transform};
use focusforge_core::io::test_frames::{
    FrameMetadata, RawEntry, ascii_entry, longs_bo, rational_bo, write_frame_with_metadata,
};
use focusforge_core::io::{
    ByteOrder, Rect, SegmentOrigin, TiffSegmentReader, TiffSpec, TiffWriter, read_source_metadata,
};
use focusforge_core::params::ParamValues;
use focusforge_core::project::export::to_8bit;
use focusforge_core::project::{self, AlignRecord, Metrics, Op, Point, SealRequest};
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};
use raw_dng::{DngSpec, write_dng};

fn bin() -> Command {
    common::cli_bin("export_formats")
}

struct CliOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

fn run_cli_in(dir: &Path, args: &[&str]) -> CliOutput {
    let output = bin()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run binary");
    CliOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn partial_of(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap().to_os_string();
    name.push(".partial");
    path.with_file_name(name)
}

fn assert_refused(dir: &Path, args: &[&str], out: &Path) {
    let result = run_cli_in(dir, args);
    assert!(
        !result.success,
        "must fail for {args:?} -- stdout: {}",
        result.stdout
    );
    assert!(!out.exists(), "a refusal must leave no output: {out:?}");
    assert!(
        !partial_of(out).exists(),
        "a refusal must leave no .partial: {out:?}"
    );
}

/// Not a multiple of 16.
const EW: u32 = 300;
/// More than one 256-row export band.
const EH: u32 = 300;

fn full(width: u32, height: u32) -> Rect {
    Rect {
        x: 0,
        y: 0,
        width,
        height,
    }
}

/// A smooth two-axis gradient, offset per channel, so LZW/Deflate/PNG
/// filtering have real redundancy to exploit and every 16-bit value in
/// range appears somewhere for the 8-bit rounding to bite on.
fn gradient_samples(width: u32, height: u32, spp: u32) -> Vec<u16> {
    let denom = u64::from(width + height).saturating_sub(2).max(1);
    let mut out = Vec::with_capacity((width * height * spp) as usize);
    for y in 0..height {
        for x in 0..width {
            let base = ((u64::from(x + y) * 65535) / denom) as u16;
            for c in 0..spp {
                out.push(base.wrapping_add((c as u16).wrapping_mul(4096)));
            }
        }
    }
    out
}

fn write_tiff(path: &Path, width: u32, height: u32, spp: u32, samples: &[u16], desc: Option<&str>) {
    let mut writer = TiffWriter::create(
        path,
        TiffSpec {
            width,
            height,
            samples_per_pixel: spp,
            description: desc.map(str::to_string),
        },
    )
    .unwrap();
    writer.write_tile(full(width, height), samples).unwrap();
    writer.finish().unwrap();
}

/// A one-frame synthetic project at [`EW`]x[`EH`], `spp` samples per pixel,
/// gathering the whole gradient frame (the Z-Map is uniformly zero, its
/// only frame): a fused `root` plus one op-bearing `child` on top of it.
struct Synth {
    _dir: common::TmpDir,
    project_path: PathBuf,
    root: String,
    child: String,
}

fn build_synth(name: &str, spp: u32) -> Synth {
    let dir = common::tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    let samples = gradient_samples(EW, EH, spp);
    let frame_path = dir.join("frame-00.tif");
    write_tiff(&frame_path, EW, EH, spp, &samples, None);

    let composite_path = staging.join("composite.tif");
    write_tiff(
        &composite_path,
        EW,
        EH,
        spp,
        &samples,
        Some("staged composite"),
    );

    let zmap_path = staging.join("zmap.tif");
    let mut map = ZMapWriter::create(&zmap_path, ZMapInfo::new(EW, EH, 1).unwrap()).unwrap();
    map.write_tile(full(EW, EH), &vec![0u16; (EW * EH) as usize])
        .unwrap();
    map.finish().unwrap();

    let project_path = dir.join(format!("synth.{}", project::PROJECT_EXTENSION));
    let sealed = project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite_path,
        staged_zmap: &zmap_path,
        sources: &[frame_path],
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");
    let root = sealed.checkpoint;

    let child = project::append_ops_onto(
        &project_path,
        &root,
        vec![Op::BrushStroke {
            source_id: 0,
            path: vec![Point { x: 2, y: 2 }],
            radius: 1.5,
            softness: 0.0,
        }],
        None,
        1,
    )
    .expect("append_ops_onto must succeed")
    .checkpoint;

    Synth {
        _dir: dir,
        project_path,
        root,
        child,
    }
}

/// A one-frame project shaped past JPEG's 65535-pixel-per-side limit, kept
/// otherwise tiny (height 2, spp 1) so the fixture itself stays cheap; the
/// refusal this feeds must fire before any pixel is rendered.
fn build_oversized(name: &str) -> (common::TmpDir, PathBuf) {
    let dir = common::tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let width = 65536u32;
    let height = 2u32;
    let samples = vec![0u16; (width * height) as usize];
    let frame_path = dir.join("frame-00.tif");
    write_tiff(&frame_path, width, height, 1, &samples, None);
    let composite_path = staging.join("composite.tif");
    write_tiff(
        &composite_path,
        width,
        height,
        1,
        &samples,
        Some("staged composite"),
    );
    let zmap_path = staging.join("zmap.tif");
    let mut map = ZMapWriter::create(&zmap_path, ZMapInfo::new(width, height, 1).unwrap()).unwrap();
    map.write_tile(full(width, height), &vec![0u16; (width * height) as usize])
        .unwrap();
    map.finish().unwrap();
    let project_path = dir.join(format!("oversized.{}", project::PROJECT_EXTENSION));
    project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite_path,
        staged_zmap: &zmap_path,
        sources: &[frame_path],
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");
    (dir, project_path)
}

/// Reassembles a plain 16-bit TIFF (any compression `segment.rs` accepts)
/// into one row-major sample array, through `core`'s own segment decoder.
fn read_full_16bit(path: &Path, width: u32, height: u32, spp: u32) -> Vec<u16> {
    let reader = TiffSegmentReader::open(path).unwrap();
    assert_eq!(reader.header().width, width);
    assert_eq!(reader.header().height, height);
    assert_eq!(reader.header().samples_per_pixel, spp);
    let mut out = vec![0u16; (width * height * spp) as usize];
    for i in 0..reader.segment_count() {
        let seg = reader.decode(i).unwrap();
        let SegmentOrigin::Strip { y } = seg.origin else {
            panic!("expected a strip-organised TIFF")
        };
        for row in 0..seg.valid_height {
            let src_start = (row * seg.stored_width * spp) as usize;
            let src_len = (seg.valid_width * spp) as usize;
            let dst_start = ((y + row) * width * spp) as usize;
            out[dst_start..dst_start + src_len]
                .copy_from_slice(&seg.samples[src_start..src_start + src_len]);
        }
    }
    out
}

// The default equals an explicit `--format tiff --tiff-bits 16
// --tiff-compression none` request.
#[test]
fn explicit_tiff_16_none_equals_the_default() {
    let synth = build_synth("export_formats_default_equiv", 3);
    let dir = common::tmp_dir("export_formats_default_equiv_out");
    let cwd = synth.project_path.parent().unwrap();
    for (label, id) in [("root", &synth.root), ("child", &synth.child)] {
        let default_out = dir.join(format!("{label}-default.tif"));
        let explicit_out = dir.join(format!("{label}-explicit.tif"));
        let r1 = run_cli_in(
            cwd,
            &[
                "export",
                synth.project_path.to_str().unwrap(),
                "--checkpoint",
                id,
                "-o",
                default_out.to_str().unwrap(),
            ],
        );
        assert!(r1.success, "{label} default: {}", r1.stderr);
        let r2 = run_cli_in(
            cwd,
            &[
                "export",
                synth.project_path.to_str().unwrap(),
                "--checkpoint",
                id,
                "--format",
                "tiff",
                "--tiff-bits",
                "16",
                "--tiff-compression",
                "none",
                "-o",
                explicit_out.to_str().unwrap(),
            ],
        );
        assert!(r2.success, "{label} explicit: {}", r2.stderr);
        assert_eq!(
            std::fs::read(&default_out).unwrap(),
            std::fs::read(&explicit_out).unwrap(),
            "{label}: an explicit tiff/16/none request must equal the default"
        );
    }
}

// LZW and Deflate round-trip losslessly through core's own reader,
// for 1 and 3 samples per pixel, and each file's Compression tag matches
// what was asked for.
#[test]
fn compressed_tiff_round_trips_losslessly() {
    for spp in [1u32, 3] {
        let synth = build_synth(&format!("export_formats_compressed_spp{spp}"), spp);
        let cwd = synth.project_path.parent().unwrap();
        let dir = common::tmp_dir(&format!("export_formats_compressed_out_spp{spp}"));
        let uncompressed = dir.join("none.tif");
        let r = run_cli_in(
            cwd,
            &[
                "export",
                synth.project_path.to_str().unwrap(),
                "-o",
                uncompressed.to_str().unwrap(),
            ],
        );
        assert!(r.success, "spp{spp} uncompressed: {}", r.stderr);
        let expected = read_full_16bit(&uncompressed, EW, EH, spp);

        for (name, tag) in [("lzw", 5u16), ("deflate", 8u16)] {
            let out = dir.join(format!("{name}.tif"));
            let r = run_cli_in(
                cwd,
                &[
                    "export",
                    synth.project_path.to_str().unwrap(),
                    "--format",
                    "tiff",
                    "--tiff-compression",
                    name,
                    "-o",
                    out.to_str().unwrap(),
                ],
            );
            assert!(r.success, "spp{spp} {name}: {}", r.stderr);
            let reader = TiffSegmentReader::open(&out).unwrap();
            assert_eq!(
                reader.header().compression,
                tag,
                "spp{spp} {name}: wrong Compression tag"
            );
            drop(reader);
            let got = read_full_16bit(&out, EW, EH, spp);
            assert_eq!(
                got, expected,
                "spp{spp} {name}: samples must round-trip losslessly"
            );
        }
    }
}

// 8-bit TIFF and JPEG share one 16->8 conversion; an independent
// reader must see exactly `to_8bit` applied to the 16-bit export.
#[test]
fn eight_bit_tiff_is_the_documented_conversion() {
    let synth = build_synth("export_formats_8bit", 3);
    let cwd = synth.project_path.parent().unwrap();
    let dir = common::tmp_dir("export_formats_8bit_out");
    let sixteen = dir.join("16.tif");
    let r = run_cli_in(
        cwd,
        &[
            "export",
            synth.project_path.to_str().unwrap(),
            "-o",
            sixteen.to_str().unwrap(),
        ],
    );
    assert!(r.success, "{}", r.stderr);
    let expected: Vec<u8> = read_full_16bit(&sixteen, EW, EH, 3)
        .into_iter()
        .map(to_8bit)
        .collect();

    let eight = dir.join("8.tif");
    let r = run_cli_in(
        cwd,
        &[
            "export",
            synth.project_path.to_str().unwrap(),
            "--format",
            "tiff",
            "--tiff-bits",
            "8",
            "-o",
            eight.to_str().unwrap(),
        ],
    );
    assert!(r.success, "{}", r.stderr);

    let file = std::fs::File::open(&eight).unwrap();
    let mut decoder = tiff::decoder::Decoder::new(file).unwrap();
    assert_eq!(decoder.dimensions().unwrap(), (EW, EH));
    let image = decoder.read_image().unwrap();
    let got = match image {
        tiff::decoder::DecodingResult::U8(bytes) => bytes,
        other => panic!("expected 8-bit samples, got {other:?}"),
    };
    assert_eq!(
        got, expected,
        "8-bit TIFF must hold to_8bit of every 16-bit sample"
    );
}

// PNG round-trips losslessly at 16 and 8 bit, 1 and 3 samples per
// pixel, at every compression level; `best` is no larger than `fast`.
#[test]
fn png_round_trips_losslessly() {
    for spp in [1u32, 3] {
        let synth = build_synth(&format!("export_formats_png_spp{spp}"), spp);
        let cwd = synth.project_path.parent().unwrap();
        let dir = common::tmp_dir(&format!("export_formats_png_out_spp{spp}"));
        let sixteen_tiff = dir.join("16.tif");
        let r = run_cli_in(
            cwd,
            &[
                "export",
                synth.project_path.to_str().unwrap(),
                "-o",
                sixteen_tiff.to_str().unwrap(),
            ],
        );
        assert!(r.success, "spp{spp}: {}", r.stderr);
        let expected16 = read_full_16bit(&sixteen_tiff, EW, EH, spp);
        let expected8: Vec<u8> = expected16.iter().copied().map(to_8bit).collect();

        let expected_color = if spp >= 3 {
            png::ColorType::Rgb
        } else {
            png::ColorType::Grayscale
        };

        let mut sizes: Vec<(String, u64)> = Vec::new();
        for bits in ["16", "8"] {
            for compression in ["fast", "default", "best"] {
                let out = dir.join(format!("{bits}-{compression}.png"));
                let r = run_cli_in(
                    cwd,
                    &[
                        "export",
                        synth.project_path.to_str().unwrap(),
                        "--format",
                        "png",
                        "--png-bits",
                        bits,
                        "--png-compression",
                        compression,
                        "-o",
                        out.to_str().unwrap(),
                    ],
                );
                assert!(r.success, "spp{spp} {bits}/{compression}: {}", r.stderr);

                let file = std::fs::File::open(&out).unwrap();
                let decoder = png::Decoder::new(std::io::BufReader::new(file));
                let mut reader = decoder.read_info().unwrap();
                let info = reader.info();
                assert_eq!(info.width, EW, "spp{spp} {bits}/{compression}");
                assert_eq!(info.height, EH, "spp{spp} {bits}/{compression}");
                assert_eq!(
                    info.color_type, expected_color,
                    "spp{spp} {bits}/{compression}"
                );
                let expected_depth = if bits == "16" {
                    png::BitDepth::Sixteen
                } else {
                    png::BitDepth::Eight
                };
                assert_eq!(
                    info.bit_depth, expected_depth,
                    "spp{spp} {bits}/{compression}"
                );

                let mut buf = vec![0u8; reader.output_buffer_size().unwrap()];
                reader.next_frame(&mut buf).unwrap();
                if bits == "16" {
                    let got: Vec<u16> = buf
                        .chunks_exact(2)
                        .map(|c| u16::from_be_bytes([c[0], c[1]]))
                        .collect();
                    assert_eq!(got, expected16, "spp{spp} {bits}/{compression}");
                } else {
                    assert_eq!(buf, expected8, "spp{spp} {bits}/{compression}");
                }

                if bits == "16" {
                    sizes.push((
                        compression.to_string(),
                        std::fs::metadata(&out).unwrap().len(),
                    ));
                }
            }
        }
        let fast = sizes.iter().find(|(c, _)| c == "fast").unwrap().1;
        let best = sizes.iter().find(|(c, _)| c == "best").unwrap().1;
        assert!(
            best <= fast,
            "spp{spp}: best ({best} B) must be no larger than fast ({fast} B)"
        );
    }
}

// `--format` is inferred from `-o`'s extension, contradictions and
// unrecognised extensions are refused, and `--zmap` is never inferred.
#[test]
fn format_follows_the_output_extension() {
    let synth = build_synth("export_formats_extension", 3);
    let cwd = synth.project_path.parent().unwrap();
    let dir = common::tmp_dir("export_formats_extension_out");

    for (ext, magic) in [
        ("tif", &b"II"[..]),
        ("TIFF", &b"II"[..]),
        ("png", &b"\x89PNG"[..]),
        ("jpg", &b"\xFF\xD8"[..]),
        ("JPEG", &b"\xFF\xD8"[..]),
    ] {
        let out = dir.join(format!("out.{ext}"));
        let r = run_cli_in(
            cwd,
            &[
                "export",
                synth.project_path.to_str().unwrap(),
                "-o",
                out.to_str().unwrap(),
            ],
        );
        assert!(r.success, ".{ext}: {}", r.stderr);
        let bytes = std::fs::read(&out).unwrap();
        assert!(
            bytes.starts_with(magic),
            ".{ext} must produce {magic:?}, got {:?}",
            &bytes[..magic.len().min(bytes.len())]
        );
    }

    let no_ext = dir.join("no-extension");
    let r = run_cli_in(
        cwd,
        &[
            "export",
            synth.project_path.to_str().unwrap(),
            "-o",
            no_ext.to_str().unwrap(),
        ],
    );
    assert!(r.success, "{}", r.stderr);
    assert!(std::fs::read(&no_ext).unwrap().starts_with(b"II"));

    let contradiction = dir.join("wrong.jpg");
    assert_refused(
        cwd,
        &[
            "export",
            synth.project_path.to_str().unwrap(),
            "-o",
            contradiction.to_str().unwrap(),
            "--format",
            "tiff",
        ],
        &contradiction,
    );

    let unrecognised = dir.join("out.webp");
    assert_refused(
        cwd,
        &[
            "export",
            synth.project_path.to_str().unwrap(),
            "-o",
            unrecognised.to_str().unwrap(),
        ],
        &unrecognised,
    );

    // `--zmap` is never inferred: a `.png`-named zmap path still gets the
    // plain 16-bit TIFF map.
    let zmap_out = dir.join("zmap.png");
    let r = run_cli_in(
        cwd,
        &[
            "export",
            synth.project_path.to_str().unwrap(),
            "--zmap",
            zmap_out.to_str().unwrap(),
        ],
    );
    assert!(r.success, "{}", r.stderr);
    assert!(std::fs::read(&zmap_out).unwrap().starts_with(b"II"));
}

/// The sampling factors (H, V) of every component named by a JPEG's SOF0
/// marker, read directly off the byte stream -- independent of any decoder
/// library's own component bookkeeping.
fn sof0_sampling_factors(bytes: &[u8]) -> Vec<(u8, u8)> {
    let mut i = 2; // past SOI
    while i + 4 <= bytes.len() {
        assert_eq!(bytes[i], 0xFF, "expected a marker at byte {i}");
        let marker = bytes[i + 1];
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if marker == 0xC0 || marker == 0xC2 {
            let seg = &bytes[i + 4..i + 2 + len];
            let num_components = seg[5] as usize;
            return (0..num_components)
                .map(|c| {
                    let hv = seg[6 + c * 3 + 1];
                    (hv >> 4, hv & 0x0F)
                })
                .collect();
        }
        i += 2 + len;
    }
    panic!("no SOF0/SOF2 marker found");
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse: f64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = f64::from(x) - f64::from(y);
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

// JPEG honours quality and chroma subsampling.
#[test]
fn jpeg_honours_quality_and_subsampling() {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    for spp in [1u32, 3] {
        let synth = build_synth(&format!("export_formats_jpeg_spp{spp}"), spp);
        let cwd = synth.project_path.parent().unwrap();
        let dir = common::tmp_dir(&format!("export_formats_jpeg_out_spp{spp}"));
        let sixteen = dir.join("16.tif");
        let r = run_cli_in(
            cwd,
            &[
                "export",
                synth.project_path.to_str().unwrap(),
                "-o",
                sixteen.to_str().unwrap(),
            ],
        );
        assert!(r.success, "spp{spp}: {}", r.stderr);
        let expected8: Vec<u8> = read_full_16bit(&sixteen, EW, EH, spp)
            .into_iter()
            .map(to_8bit)
            .collect();
        let out_colorspace = if spp >= 3 {
            ColorSpace::RGB
        } else {
            ColorSpace::Luma
        };

        let mut psnr_444: Vec<(i64, f64)> = Vec::new();
        for (subsampling, expect_factors) in [("444", (1u8, 1u8)), ("420", (2u8, 2u8))] {
            for quality in [1i64, 90, 100] {
                let out = dir.join(format!("q{quality}-{subsampling}.jpg"));
                let r = run_cli_in(
                    cwd,
                    &[
                        "export",
                        synth.project_path.to_str().unwrap(),
                        "--format",
                        "jpeg",
                        "--jpeg-quality",
                        &quality.to_string(),
                        "--jpeg-subsampling",
                        subsampling,
                        "-o",
                        out.to_str().unwrap(),
                    ],
                );
                assert!(r.success, "spp{spp} q{quality}/{subsampling}: {}", r.stderr);
                let bytes = std::fs::read(&out).unwrap();

                let factors = sof0_sampling_factors(&bytes);
                let luma = factors[0];
                let expected_luma = if spp >= 3 { expect_factors } else { (1, 1) };
                assert_eq!(
                    luma, expected_luma,
                    "spp{spp} q{quality}/{subsampling}: luma sampling factors"
                );
                if spp >= 3 && subsampling == "444" {
                    for &chroma in &factors[1..] {
                        assert_eq!(chroma, (1, 1), "spp{spp} q{quality}/444: chroma factors");
                    }
                }

                let mut decoder = JpegDecoder::new_with_options(
                    ZCursor::new(&bytes),
                    DecoderOptions::default().jpeg_set_out_colorspace(out_colorspace),
                );
                let pixels = decoder.decode().unwrap();
                let info = decoder.info().unwrap();
                assert_eq!(
                    (u32::from(info.width), u32::from(info.height)),
                    (EW, EH),
                    "spp{spp} q{quality}/{subsampling}"
                );

                let value = psnr(&pixels, &expected8);
                if subsampling == "444" {
                    psnr_444.push((quality, value));
                }
                if quality == 100 && subsampling == "444" {
                    assert!(
                        value >= 40.0,
                        "spp{spp} q100/444: PSNR {value} dB must be at least 40 dB"
                    );
                }
            }
        }
        psnr_444.sort_by_key(|(q, _)| *q);
        for pair in psnr_444.windows(2) {
            assert!(
                pair[1].1 >= pair[0].1,
                "spp{spp}: PSNR must rise with quality: {:?}",
                psnr_444
            );
        }
    }
}

// Every boundary, CLI input and encoder-limit refusal leaves neither
// the output nor a `.partial`; `--zmap` is unaffected by `--format`.
#[test]
fn refused_format_options_leave_nothing() {
    let dir = common::tmp_dir("export_formats_refused");
    let missing_project = "nonexistent.ffp";

    // These all fail purely at flag-parsing time, before the project is
    // even opened, so a nonexistent project path is fine.
    let cases: &[(&str, &[&str])] = &[
        ("unknown --format", &["--format", "webp"]),
        (
            "an option for another format",
            &["--format", "tiff", "--jpeg-quality", "80"],
        ),
        (
            "an option for another format via inference",
            &["--jpeg-quality", "80"], // -o out.tif infers tiff below
        ),
        (
            "--jpeg-quality 0",
            &["--format", "jpeg", "--jpeg-quality", "0"],
        ),
        (
            "--jpeg-quality 101",
            &["--format", "jpeg", "--jpeg-quality", "101"],
        ),
        ("--tiff-bits 12", &["--format", "tiff", "--tiff-bits", "12"]),
        ("--png-bits 12", &["--format", "png", "--png-bits", "12"]),
        (
            "--png-compression with --format jpeg",
            &["--format", "jpeg", "--png-compression", "fast"],
        ),
        (
            "an unknown compression name",
            &["--format", "tiff", "--tiff-compression", "bogus"],
        ),
    ];
    for (label, extra) in cases {
        let out = dir.join(format!("{}.tif", label.replace(' ', "_")));
        let mut args = vec!["export", missing_project, "-o"];
        let out_str = out.to_str().unwrap();
        args.push(out_str);
        args.extend_from_slice(extra);
        assert_refused(&dir, &args, &out);
        let _ = label;
    }

    // JPEG's 65535-pixel-per-side limit, checked before any pixel renders.
    let (_oversized_dir, oversized_project) = build_oversized("export_formats_refused_oversized");
    let out = dir.join("oversized.jpg");
    assert_refused(
        oversized_project.parent().unwrap(),
        &[
            "export",
            oversized_project.to_str().unwrap(),
            "--format",
            "jpeg",
            "-o",
            out.to_str().unwrap(),
        ],
        &out,
    );

    // An unwritable destination. A read-only directory is a Unix permission; Windows has no
    // mode to refuse with.
    let synth = build_synth("export_formats_refused_unwritable", 3);
    #[cfg(unix)]
    {
        let readonly_dir = dir.join("readonly");
        std::fs::create_dir_all(&readonly_dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&readonly_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let out = readonly_dir.join("out.tif");
        let result = run_cli_in(
            synth.project_path.parent().unwrap(),
            &[
                "export",
                synth.project_path.to_str().unwrap(),
                "-o",
                out.to_str().unwrap(),
            ],
        );
        assert!(!result.success);
        std::fs::set_permissions(&readonly_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!out.exists());
        assert!(!partial_of(&out).exists());
    }

    // `--zmap` alongside any `--format` still writes the plain 16-bit map.
    let plain_zmap = dir.join("plain-zmap.tif");
    let r = run_cli_in(
        synth.project_path.parent().unwrap(),
        &[
            "export",
            synth.project_path.to_str().unwrap(),
            "--zmap",
            plain_zmap.to_str().unwrap(),
        ],
    );
    assert!(r.success, "{}", r.stderr);
    let format_zmap = dir.join("format-zmap.tif");
    let r = run_cli_in(
        synth.project_path.parent().unwrap(),
        &[
            "export",
            synth.project_path.to_str().unwrap(),
            "--zmap",
            format_zmap.to_str().unwrap(),
            "--format",
            "jpeg",
            "--jpeg-quality",
            "50",
        ],
    );
    assert!(r.success, "{}", r.stderr);
    assert_eq!(
        std::fs::read(&plain_zmap).unwrap(),
        std::fs::read(&format_zmap).unwrap(),
        "--zmap must be unaffected by --format"
    );
}

// `--json` reports the format and its resolved options, both `null`
// when only `--zmap` was written.
#[test]
fn json_reports_format_and_options() {
    let synth = build_synth("export_formats_json", 3);
    let cwd = synth.project_path.parent().unwrap();
    let dir = common::tmp_dir("export_formats_json_out");

    let image = dir.join("out.png");
    let r = run_cli_in(
        cwd,
        &[
            "export",
            synth.project_path.to_str().unwrap(),
            "-o",
            image.to_str().unwrap(),
            "--format",
            "png",
            "--png-bits",
            "8",
            "--png-compression",
            "best",
            "--json",
        ],
    );
    assert!(r.success, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).expect("--json parses");
    assert_eq!(report["format"], serde_json::json!("png"));
    assert_eq!(report["format_options"]["png-bits"], serde_json::json!("8"));
    assert_eq!(
        report["format_options"]["png-compression"],
        serde_json::json!("best")
    );

    let zmap = dir.join("zmap-only.tif");
    let r = run_cli_in(
        cwd,
        &[
            "export",
            synth.project_path.to_str().unwrap(),
            "--zmap",
            zmap.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(r.success, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).expect("--json parses");
    assert_eq!(report["format"], serde_json::json!(null));
    assert_eq!(report["format_options"], serde_json::json!(null));
}

// ---------------------------------------------------------------------------
// A source frame's EXIF, ICC, XMP and IPTC carried into export.
// ---------------------------------------------------------------------------

/// A source frame size for the metadata tests: distinct dimensions, smaller
/// than [`EW`]/[`EH`] since these exercise metadata rather than banding.
const MW: u32 = 48;
const MH: u32 = 24;

/// Bytes long enough (>4) to force out-of-line TIFF storage; not a real ICC
/// profile, since every test here only checks byte-identity.
fn test_icc(salt: u8) -> Vec<u8> {
    (0..64u32).map(|i| (i as u8).wrapping_add(salt)).collect()
}

/// One GPS coordinate as three `RATIONAL`s (degrees, minutes, seconds).
fn dms(deg: u32, min: u32, sec: u32) -> Vec<u8> {
    [
        rational_bo(ByteOrder::Little, deg, 1),
        rational_bo(ByteOrder::Little, min, 1),
        rational_bo(ByteOrder::Little, sec, 1),
    ]
    .concat()
}

/// A source frame carrying a realistic EXIF IFD (`Make`, `Model`,
/// `DateTimeOriginal`, `ExposureTime`, `FNumber`) and a GPS IFD, plus
/// whichever of an ICC profile, an XMP packet and an IPTC block are given.
/// `exif_dims`, when given, is the size the source's own `ExifImageWidth`/
/// `Height` claim -- deliberately different from an export's actual size,
/// so the rewrite rule has something to prove.
fn rich_frame_metadata(
    make: &str,
    exif_dims: Option<(u32, u32)>,
    icc: Option<&[u8]>,
    xmp: Option<&[u8]>,
    iptc: Option<&[u8]>,
) -> FrameMetadata {
    let mut ifd0: Vec<RawEntry> = vec![
        (271, 2, ascii_entry(make)),
        (272, 2, ascii_entry("Test Body")),
        (36867, 2, ascii_entry("2024:05:01 10:00:00")), // DateTimeOriginal
    ];
    if let Some(bytes) = icc {
        ifd0.push((34675, 7, bytes.to_vec())); // ICC profile, UNDEFINED
    }
    if let Some(bytes) = xmp {
        ifd0.push((700, 1, bytes.to_vec())); // XMP, BYTE
    }
    if let Some(bytes) = iptc {
        ifd0.push((33723, 7, bytes.to_vec())); // IPTC, UNDEFINED
    }
    let mut exif: Vec<RawEntry> = vec![
        (33434, 5, rational_bo(ByteOrder::Little, 1, 200)), // ExposureTime
        (33437, 5, rational_bo(ByteOrder::Little, 28, 10)), // FNumber
    ];
    if let Some((w, h)) = exif_dims {
        exif.push((40962, 4, longs_bo(ByteOrder::Little, &[w])));
        exif.push((40963, 4, longs_bo(ByteOrder::Little, &[h])));
    }
    let gps: Vec<RawEntry> = vec![
        (1, 2, ascii_entry("N")), // GPSLatitudeRef
        (2, 5, dms(37, 46, 30)),  // GPSLatitude
        (3, 2, ascii_entry("W")), // GPSLongitudeRef
        (4, 5, dms(122, 25, 10)), // GPSLongitude
    ];
    FrameMetadata {
        ifd0,
        exif,
        gps,
        ..Default::default()
    }
}

struct MetaSynth {
    _dir: common::TmpDir,
    project_path: PathBuf,
    root: String,
    child: String,
    frame_paths: Vec<PathBuf>,
}

/// A project like [`Synth`] but whose sources are written through
/// [`write_frame_with_metadata`] rather than the plain [`TiffWriter`], so
/// each can carry its own EXIF/GPS/ICC/XMP/IPTC. The Z-Map is uniformly zero
/// (frame 0 contributes every pixel) regardless of `align_reference`: the
/// alignment reference and the Z-Map's own per-pixel source choice are
/// independent, and an export only ever reads the former.
fn build_metadata_synth(
    name: &str,
    frames: &[FrameMetadata],
    align_reference: Option<usize>,
) -> MetaSynth {
    let dir = common::tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    let mut frame_paths = Vec::new();
    for (i, meta) in frames.iter().enumerate() {
        let path = dir.join(format!("frame-{i:02}.tif"));
        write_frame_with_metadata(&path, MW, MH, meta).expect("source frame must write");
        frame_paths.push(path);
    }

    let composite_path = staging.join("composite.tif");
    write_tiff(
        &composite_path,
        MW,
        MH,
        3,
        &gradient_samples(MW, MH, 3),
        Some("staged composite"),
    );
    let zmap_path = staging.join("zmap.tif");
    let mut map = ZMapWriter::create(
        &zmap_path,
        ZMapInfo::new(MW, MH, frame_paths.len() as u32).unwrap(),
    )
    .unwrap();
    map.write_tile(full(MW, MH), &vec![0u16; (MW * MH) as usize])
        .unwrap();
    map.finish().unwrap();

    let align = align_reference.map(|reference| AlignRecord {
        algorithm: "test".to_string(),
        params: ParamValues::new(),
        reference,
    });
    let registration = if align.is_some() {
        Registration {
            transforms: vec![Transform::identity(); frame_paths.len()],
            crop: None,
            equalization: None,
        }
    } else {
        Registration::default()
    };

    let project_path = dir.join(format!("meta.{}", project::PROJECT_EXTENSION));
    let sealed = project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite_path,
        staged_zmap: &zmap_path,
        sources: &frame_paths,
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align,
        registration,
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");
    let root = sealed.checkpoint;

    let child = project::append_ops_onto(
        &project_path,
        &root,
        vec![Op::BrushStroke {
            source_id: 0,
            path: vec![Point { x: 2, y: 2 }],
            radius: 1.5,
            softness: 0.0,
        }],
        None,
        1,
    )
    .expect("append_ops_onto must succeed")
    .checkpoint;

    MetaSynth {
        _dir: dir,
        project_path,
        root,
        child,
        frame_paths,
    }
}

/// A one-source RAW project, sized to [`DngSpec::new`]'s own 8x8 default so
/// its `samples` vector needs no resizing.
fn build_raw_metadata_synth(name: &str, make: &str, model: &str) -> (common::TmpDir, PathBuf) {
    let dir = common::tmp_dir(name);
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();

    let mut spec = DngSpec::new(12, 0, 4095, [0, 1, 1, 2], vec![1000u16; 64]);
    spec.make = make.to_string();
    spec.model = model.to_string();
    let dng = write_dng(&spec, false);
    let frame_path = dir.join("00.dng");
    std::fs::copy(dng.path(), &frame_path).expect("copy RAW frame");

    let composite_path = staging.join("composite.tif");
    write_tiff(
        &composite_path,
        8,
        8,
        3,
        &gradient_samples(8, 8, 3),
        Some("staged composite"),
    );
    let zmap_path = staging.join("zmap.tif");
    let mut map = ZMapWriter::create(&zmap_path, ZMapInfo::new(8, 8, 1).unwrap()).unwrap();
    map.write_tile(full(8, 8), &[0u16; 64]).unwrap();
    map.finish().unwrap();

    let project_path = dir.join(format!("raw.{}", project::PROJECT_EXTENSION));
    project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite_path,
        staged_zmap: &zmap_path,
        sources: &[frame_path],
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");
    (dir, project_path)
}

/// Rewrites `tag`'s declared element count in `path`'s little-endian IFD0
/// to `count`, without touching anything else in the file -- the "truncated
/// entry" shape a reader must survive: a declared size the file no longer
/// backs.
fn corrupt_ifd0_entry_count(path: &Path, tag: u16, count: u32) {
    let mut bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[0..2], b"II", "helper assumes a little-endian frame");
    let ifd0_at = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let n = u16::from_le_bytes(bytes[ifd0_at..ifd0_at + 2].try_into().unwrap()) as usize;
    for i in 0..n {
        let e = ifd0_at + 2 + i * 12;
        if u16::from_le_bytes(bytes[e..e + 2].try_into().unwrap()) == tag {
            bytes[e + 4..e + 8].copy_from_slice(&count.to_le_bytes());
            std::fs::write(path, &bytes).unwrap();
            return;
        }
    }
    panic!("tag {tag} not found in IFD0 of {}", path.display());
}

// --- reading embedded metadata back out of PNG/JPEG, independent of
// `core`'s own writer -------------------------------------------------------

/// One APPn JPEG segment's marker number (0 for APP0 .. 15 for APP15) and
/// payload, excluding the 2-byte length field.
fn jpeg_app_segments(bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 2; // past SOI
    while i + 4 <= bytes.len() {
        assert_eq!(bytes[i], 0xFF, "expected a marker at byte {i}");
        let marker = bytes[i + 1];
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        if marker == 0xDA {
            break; // start of scan: nothing of interest follows
        }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if (0xE0..=0xEF).contains(&marker) {
            out.push((marker - 0xE0, bytes[i + 4..i + 2 + len].to_vec()));
        }
        i += 2 + len;
    }
    out
}

/// The bytes of a JPEG's embedded `Exif\0\0` APP1 segment, sans header.
fn jpeg_exif_blob(bytes: &[u8]) -> Option<Vec<u8>> {
    jpeg_app_segments(bytes)
        .into_iter()
        .find_map(|(marker, payload)| {
            (marker == 1 && payload.starts_with(b"Exif\0\0")).then(|| payload[6..].to_vec())
        })
}

/// The JPEG's embedded ICC profile, reassembled from one or more APP2
/// `ICC_PROFILE\0` segments in chunk order (the 1-indexed sequence byte at
/// offset 12).
fn jpeg_icc_profile(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut chunks: Vec<(u8, Vec<u8>)> = jpeg_app_segments(bytes)
        .into_iter()
        .filter_map(|(marker, payload)| {
            (marker == 2 && payload.starts_with(b"ICC_PROFILE\0"))
                .then(|| (payload[12], payload[14..].to_vec()))
        })
        .collect();
    if chunks.is_empty() {
        return None;
    }
    chunks.sort_by_key(|(seq, _)| *seq);
    Some(chunks.into_iter().flat_map(|(_, data)| data).collect())
}

/// The JPEG's embedded XMP packet: the APP1 segment prefixed with the XMP
/// namespace header rather than `Exif\0\0`.
fn jpeg_xmp_packet(bytes: &[u8]) -> Option<Vec<u8>> {
    const HEADER: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
    jpeg_app_segments(bytes)
        .into_iter()
        .find_map(|(marker, payload)| {
            (marker == 1 && payload.starts_with(HEADER)).then(|| payload[HEADER.len()..].to_vec())
        })
}

/// The JPEG's embedded IPTC block, unwrapped from its APP13 `Photoshop
/// 3.0\0` / `8BIM` 0x0404 resource -- exactly the shape
/// `core::io::photoshop_iptc_segment` writes.
fn jpeg_iptc_block(bytes: &[u8]) -> Option<Vec<u8>> {
    jpeg_app_segments(bytes)
        .into_iter()
        .find_map(|(marker, payload)| {
            if marker != 13 || !payload.starts_with(b"Photoshop 3.0\0") {
                return None;
            }
            let rest = &payload[14..];
            assert_eq!(&rest[0..4], b"8BIM");
            assert_eq!(u16::from_be_bytes([rest[4], rest[5]]), 0x0404);
            // A 2-byte empty Pascal name, then a 4-byte big-endian size.
            let size = u32::from_be_bytes(rest[8..12].try_into().unwrap()) as usize;
            Some(rest[12..12 + size].to_vec())
        })
}

/// One IFD entry read back from a standalone EXIF blob.
struct BlobEntry {
    tag: u16,
    value: Vec<u8>,
}

fn blob_field_size(field_type: u16) -> usize {
    match field_type {
        1 | 2 | 7 => 1,
        3 | 8 => 2,
        5 | 10 => 8,
        _ => 4,
    }
}

fn blob_read_ifd(bytes: &[u8], at: usize) -> Vec<BlobEntry> {
    let n = u16::from_le_bytes([bytes[at], bytes[at + 1]]) as usize;
    (0..n)
        .map(|i| {
            let e = at + 2 + i * 12;
            let tag = u16::from_le_bytes([bytes[e], bytes[e + 1]]);
            let field_type = u16::from_le_bytes([bytes[e + 2], bytes[e + 3]]);
            let count = u32::from_le_bytes(bytes[e + 4..e + 8].try_into().unwrap());
            let total = blob_field_size(field_type) * count as usize;
            let value = if total <= 4 {
                bytes[e + 8..e + 8 + total].to_vec()
            } else {
                let off = u32::from_le_bytes(bytes[e + 8..e + 12].try_into().unwrap()) as usize;
                bytes[off..off + total].to_vec()
            };
            BlobEntry { tag, value }
        })
        .collect()
}

/// A standalone EXIF blob (a JPEG APP1 `Exif` segment sans header, or a PNG
/// `eXIf` chunk -- the same `II`/IFD0-at-8 bytes `core::io::exif_blob`
/// builds, always little-endian) split into its IFD0, EXIF sub-IFD and GPS
/// sub-IFD entries, read independently of `core`'s own IFD reader.
fn read_exif_blob(bytes: &[u8]) -> (Vec<BlobEntry>, Vec<BlobEntry>, Vec<BlobEntry>) {
    assert_eq!(
        &bytes[0..2],
        b"II",
        "core always writes a little-endian blob"
    );
    let ifd0_at = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let mut ifd0 = Vec::new();
    let mut exif = Vec::new();
    let mut gps = Vec::new();
    for entry in blob_read_ifd(bytes, ifd0_at) {
        let offset = || u32::from_le_bytes(entry.value[..4].try_into().unwrap()) as usize;
        match entry.tag {
            34665 => exif = blob_read_ifd(bytes, offset()),
            34853 => gps = blob_read_ifd(bytes, offset()),
            _ => ifd0.push(entry),
        }
    }
    (ifd0, exif, gps)
}

/// The three IFDs a source's metadata reads back into, as plain `(tag,
/// bytes)` pairs -- the one shape every format's read-back converges to, so
/// TIFF/PNG/JPEG can be asserted against with the same helpers.
struct Metadata {
    ifd0: Vec<(u16, Vec<u8>)>,
    exif: Vec<(u16, Vec<u8>)>,
    gps: Vec<(u16, Vec<u8>)>,
}

impl Metadata {
    fn find_ifd0(&self, tag: u16) -> Option<&[u8]> {
        self.ifd0
            .iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, v)| v.as_slice())
    }
    fn find_exif(&self, tag: u16) -> Option<&[u8]> {
        self.exif
            .iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, v)| v.as_slice())
    }
    fn find_gps(&self, tag: u16) -> Option<&[u8]> {
        self.gps
            .iter()
            .find(|(t, _)| *t == tag)
            .map(|(_, v)| v.as_slice())
    }
}

fn read_back_tiff(path: &Path) -> Metadata {
    let m = read_source_metadata(path);
    let cv =
        |v: Vec<focusforge_core::io::TagEntry>| v.into_iter().map(|e| (e.tag, e.value)).collect();
    Metadata {
        ifd0: cv(m.ifd0),
        exif: cv(m.exif),
        gps: cv(m.gps),
    }
}

fn read_back_blob(blob: &[u8]) -> Metadata {
    let (ifd0, exif, gps) = read_exif_blob(blob);
    let cv = |v: Vec<BlobEntry>| v.into_iter().map(|e| (e.tag, e.value)).collect();
    Metadata {
        ifd0: cv(ifd0),
        exif: cv(exif),
        gps: cv(gps),
    }
}

fn read_back_png_exif(path: &Path) -> Metadata {
    let file = std::fs::File::open(path).unwrap();
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let reader = decoder.read_info().unwrap();
    let bytes = reader
        .info()
        .exif_metadata
        .clone()
        .expect("PNG must carry an eXIf chunk")
        .into_owned();
    read_back_blob(&bytes)
}

fn read_back_jpeg_exif(path: &Path) -> Metadata {
    let bytes = std::fs::read(path).unwrap();
    let blob = jpeg_exif_blob(&bytes).expect("JPEG must carry an Exif APP1 segment");
    read_back_blob(&blob)
}

// Every format carries the alignment reference frame's EXIF, with its own
// size rewritten over whatever the source's EXIF dimension tags claimed.
#[test]
fn export_carries_source_exif_in_every_format() {
    let meta = rich_frame_metadata("TestCam", Some((9999, 8888)), None, None, None);
    let synth = build_metadata_synth("export_formats_exif_every_format", &[meta], None);
    let cwd = synth.project_path.parent().unwrap();
    let dir = common::tmp_dir("export_formats_exif_every_format_out");

    let cases: &[(&str, &str, &[&str])] = &[
        ("tiff", "16.tif", &["--tiff-bits", "16"]),
        ("tiff", "8.tif", &["--tiff-bits", "8"]),
        ("png", "16.png", &["--png-bits", "16"]),
        ("png", "8.png", &["--png-bits", "8"]),
        ("jpeg", "q.jpg", &[]),
    ];
    for (format, name, extra) in cases {
        let out = dir.join(format!("{format}-{name}"));
        let mut args = vec![
            "export",
            synth.project_path.to_str().unwrap(),
            "--format",
            format,
            "-o",
        ];
        let out_str = out.to_str().unwrap();
        args.push(out_str);
        args.extend_from_slice(extra);
        let r = run_cli_in(cwd, &args);
        assert!(r.success, "{format}/{name}: {}", r.stderr);

        let read = match *format {
            "tiff" => read_back_tiff(&out),
            "png" => read_back_png_exif(&out),
            "jpeg" => read_back_jpeg_exif(&out),
            other => panic!("unexpected format {other}"),
        };

        assert_eq!(
            read.find_ifd0(271).unwrap(),
            b"TestCam\0",
            "{format}/{name}: Make"
        );
        assert_eq!(
            read.find_ifd0(272).unwrap(),
            b"Test Body\0",
            "{format}/{name}: Model"
        );
        assert_eq!(
            read.find_exif(36867).unwrap(),
            b"2024:05:01 10:00:00\0",
            "{format}/{name}: DateTimeOriginal"
        );
        assert_eq!(
            read.find_exif(33434).unwrap(),
            rational_bo(ByteOrder::Little, 1, 200),
            "{format}/{name}: ExposureTime"
        );
        assert_eq!(
            read.find_exif(33437).unwrap(),
            rational_bo(ByteOrder::Little, 28, 10),
            "{format}/{name}: FNumber"
        );
        assert_eq!(
            read.find_gps(1).unwrap(),
            b"N\0",
            "{format}/{name}: GPSLatitudeRef"
        );
        assert_eq!(
            read.find_gps(2).unwrap(),
            dms(37, 46, 30),
            "{format}/{name}: GPSLatitude"
        );

        let width = u32::from_le_bytes(read.find_exif(40962).unwrap()[..4].try_into().unwrap());
        let height = u32::from_le_bytes(read.find_exif(40963).unwrap()[..4].try_into().unwrap());
        assert_eq!(
            (width, height),
            (MW, MH),
            "{format}/{name}: ExifImageWidth/Height must be the export's own size, \
             not the source's declared (9999, 8888)"
        );
    }
}

// The EXIF frame is the fuse's alignment reference frame when aligned,
// frame 0 when not -- for both a fused and an op-bearing checkpoint.
#[test]
fn export_exif_comes_from_the_reference_frame() {
    let aligned = build_metadata_synth(
        "export_formats_exif_reference_aligned",
        &[
            rich_frame_metadata("Frame0Cam", None, None, None, None),
            rich_frame_metadata("Frame1Cam", None, None, None, None),
        ],
        Some(1),
    );
    let cwd = aligned.project_path.parent().unwrap();
    let dir = common::tmp_dir("export_formats_exif_reference_aligned_out");
    for (label, checkpoint) in [("root", &aligned.root), ("child", &aligned.child)] {
        let out = dir.join(format!("{label}.tif"));
        let r = run_cli_in(
            cwd,
            &[
                "export",
                aligned.project_path.to_str().unwrap(),
                "--checkpoint",
                checkpoint,
                "-o",
                out.to_str().unwrap(),
                "--json",
            ],
        );
        assert!(r.success, "{label}: {}", r.stderr);
        let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
        assert_eq!(report["metadata"]["frame"], serde_json::json!(1), "{label}");
        let frame_path = report["metadata"]["frame_path"].as_str().unwrap();
        let expected_name = aligned.frame_paths[1]
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(frame_path.ends_with(expected_name), "{label}: {frame_path}");
        let read = read_back_tiff(&out);
        assert_eq!(read.find_ifd0(271).unwrap(), b"Frame1Cam\0", "{label}");
    }

    let unaligned = build_metadata_synth(
        "export_formats_exif_reference_unaligned",
        &[
            rich_frame_metadata("Frame0Cam", None, None, None, None),
            rich_frame_metadata("Frame1Cam", None, None, None, None),
        ],
        None,
    );
    let out_dir = common::tmp_dir("export_formats_exif_reference_unaligned_out");
    let out = out_dir.join("out.tif");
    let r = run_cli_in(
        unaligned.project_path.parent().unwrap(),
        &[
            "export",
            unaligned.project_path.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(r.success, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(report["metadata"]["frame"], serde_json::json!(0));
    let read = read_back_tiff(&out);
    assert_eq!(read.find_ifd0(271).unwrap(), b"Frame0Cam\0");
}

// The embedded ICC profile follows the source -- byte-identical for a
// TIFF source's own profile, the built-in sRGB for a RAW source, none for a
// profileless TIFF source, off when the switch is off.
#[test]
fn export_icc_origin_follows_the_source() {
    let icc = test_icc(7);
    let with_profile = rich_frame_metadata("ICCCam", None, Some(&icc), None, None);
    let synth = build_metadata_synth("export_formats_icc_source", &[with_profile], None);
    let cwd = synth.project_path.parent().unwrap();
    let dir = common::tmp_dir("export_formats_icc_source_out");

    for (format, ext) in [("tiff", "tif"), ("png", "png"), ("jpeg", "jpg")] {
        let out = dir.join(format!("{format}.{ext}"));
        let r = run_cli_in(
            cwd,
            &[
                "export",
                synth.project_path.to_str().unwrap(),
                "--format",
                format,
                "-o",
                out.to_str().unwrap(),
                "--json",
            ],
        );
        assert!(r.success, "{format}: {}", r.stderr);
        let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
        assert_eq!(
            report["metadata"]["icc"],
            serde_json::json!("source"),
            "{format}"
        );
        let embedded = match format {
            "tiff" => read_source_metadata(&out).icc.unwrap(),
            "png" => {
                let file = std::fs::File::open(&out).unwrap();
                let decoder = png::Decoder::new(std::io::BufReader::new(file));
                let reader = decoder.read_info().unwrap();
                reader.info().icc_profile.clone().unwrap().into_owned()
            }
            "jpeg" => jpeg_icc_profile(&std::fs::read(&out).unwrap()).expect("APP2 ICC_PROFILE"),
            other => panic!("unexpected format {other}"),
        };
        assert_eq!(embedded, icc, "{format}: ICC must be byte-identical");
    }

    // `--icc off`: no profile in any container.
    let off_out = dir.join("off.tif");
    let r = run_cli_in(
        cwd,
        &[
            "export",
            synth.project_path.to_str().unwrap(),
            "--icc",
            "off",
            "-o",
            off_out.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(r.success, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(report["metadata"]["icc"], serde_json::json!("off"));
    assert!(read_source_metadata(&off_out).icc.is_none());

    // A profileless TIFF source: no profile, origin `none`.
    let without_profile = rich_frame_metadata("NoICCCam", None, None, None, None);
    let bare = build_metadata_synth("export_formats_icc_none", &[without_profile], None);
    let bare_out_dir = common::tmp_dir("export_formats_icc_none_out");
    let bare_out = bare_out_dir.join("out.tif");
    let r = run_cli_in(
        bare.project_path.parent().unwrap(),
        &[
            "export",
            bare.project_path.to_str().unwrap(),
            "-o",
            bare_out.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(r.success, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(report["metadata"]["icc"], serde_json::json!("none"));
    assert!(read_source_metadata(&bare_out).icc.is_none());

    // A RAW-sourced project: the built-in sRGB profile, EXIF carrying
    // rawler's Make/Model.
    let (raw_dir, raw_project) =
        build_raw_metadata_synth("export_formats_icc_raw", "RawMake", "RawModel");
    let raw_out = raw_dir.join("out.tif");
    let r = run_cli_in(
        raw_project.parent().unwrap(),
        &[
            "export",
            raw_project.to_str().unwrap(),
            "-o",
            raw_out.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(r.success, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(report["metadata"]["icc"], serde_json::json!("builtin-srgb"));
    let raw_read = read_back_tiff(&raw_out);
    let raw_icc = read_source_metadata(&raw_out).icc.unwrap();
    assert_eq!(raw_icc.len(), 456, "the built-in sRGB profile is 456 bytes");
    assert_eq!(raw_read.find_ifd0(271).unwrap(), b"RawMake\0");
    assert_eq!(raw_read.find_ifd0(272).unwrap(), b"RawModel\0");
}

// Every switch off writes nothing, byte for byte equal to the default
// export of a metadata-free twin project with the same pixels, across
// formats and options.
#[test]
fn export_metadata_switches_off_write_nothing() {
    let rich = rich_frame_metadata(
        "SwitchCam",
        None,
        Some(&test_icc(3)),
        Some(b"<x:xmpmeta/>"),
        Some(b"iptc-bytes"),
    );
    let rich_synth = build_metadata_synth("export_formats_switches_off_rich", &[rich], None);
    let twin_synth = build_metadata_synth(
        "export_formats_switches_off_twin",
        &[FrameMetadata::default()],
        None,
    );

    let dir = common::tmp_dir("export_formats_switches_off_out");
    let combos: &[(&str, &[&str])] = &[
        (
            "tiff16.tif",
            &[
                "--format",
                "tiff",
                "--tiff-bits",
                "16",
                "--tiff-compression",
                "none",
            ],
        ),
        (
            "tiff8-lzw.tif",
            &[
                "--format",
                "tiff",
                "--tiff-bits",
                "8",
                "--tiff-compression",
                "lzw",
            ],
        ),
        ("png16.png", &["--format", "png", "--png-bits", "16"]),
        ("png8.png", &["--format", "png", "--png-bits", "8"]),
        ("jpeg.jpg", &["--format", "jpeg"]),
    ];
    for (name, extra) in combos {
        let rich_out = dir.join(format!("rich-{name}"));
        let mut args = vec!["export", rich_synth.project_path.to_str().unwrap(), "-o"];
        let rich_out_str = rich_out.to_str().unwrap();
        args.push(rich_out_str);
        args.extend_from_slice(extra);
        args.extend_from_slice(&[
            "--exif", "off", "--icc", "off", "--xmp", "off", "--iptc", "off",
        ]);
        let r = run_cli_in(rich_synth.project_path.parent().unwrap(), &args);
        assert!(r.success, "{name}: {}", r.stderr);

        let twin_out = dir.join(format!("twin-{name}"));
        let mut targs = vec!["export", twin_synth.project_path.to_str().unwrap(), "-o"];
        let twin_out_str = twin_out.to_str().unwrap();
        targs.push(twin_out_str);
        targs.extend_from_slice(extra);
        let r = run_cli_in(twin_synth.project_path.parent().unwrap(), &targs);
        assert!(r.success, "{name} (twin): {}", r.stderr);

        assert_eq!(
            std::fs::read(&rich_out).unwrap(),
            std::fs::read(&twin_out).unwrap(),
            "{name}: every switch off must equal the metadata-free twin's default export"
        );
    }
}

// XMP and IPTC are opt-in, verbatim, per format, absent for a RAW
// source, and skipped (with a warning) when too large for a JPEG APP1
// segment.
#[test]
fn export_carries_xmp_and_iptc_when_asked() {
    let xmp = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF/></x:xmpmeta>".to_vec();
    let iptc = vec![0x1C, 0x02, 0x05, b'h', b'i'];

    let source = rich_frame_metadata("XmpCam", None, None, Some(&xmp), Some(&iptc));
    let synth = build_metadata_synth("export_formats_xmp_iptc", &[source], None);
    let cwd = synth.project_path.parent().unwrap();
    let dir = common::tmp_dir("export_formats_xmp_iptc_out");

    // TIFF: both verbatim in 700/33723.
    let tiff_out = dir.join("out.tif");
    let r = run_cli_in(
        cwd,
        &[
            "export",
            synth.project_path.to_str().unwrap(),
            "--xmp",
            "on",
            "--iptc",
            "on",
            "-o",
            tiff_out.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(r.success, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(report["metadata"]["xmp"], serde_json::json!("written"));
    assert_eq!(report["metadata"]["iptc"], serde_json::json!("written"));
    let tiff_meta = read_source_metadata(&tiff_out);
    assert_eq!(tiff_meta.xmp.as_deref(), Some(xmp.as_slice()));
    assert_eq!(tiff_meta.iptc.as_deref(), Some(iptc.as_slice()));

    // JPEG: XMP in a second APP1, IPTC in APP13.
    let jpeg_out = dir.join("out.jpg");
    let r = run_cli_in(
        cwd,
        &[
            "export",
            synth.project_path.to_str().unwrap(),
            "--format",
            "jpeg",
            "--xmp",
            "on",
            "--iptc",
            "on",
            "-o",
            jpeg_out.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(r.success, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(report["metadata"]["xmp"], serde_json::json!("written"));
    assert_eq!(report["metadata"]["iptc"], serde_json::json!("written"));
    let jpeg_bytes = std::fs::read(&jpeg_out).unwrap();
    assert_eq!(jpeg_xmp_packet(&jpeg_bytes).unwrap(), xmp);
    assert_eq!(jpeg_iptc_block(&jpeg_bytes).unwrap(), iptc);

    // PNG: XMP in an iTXt chunk; PNG has no IPTC chunk, reported unsupported.
    let png_out = dir.join("out.png");
    let r = run_cli_in(
        cwd,
        &[
            "export",
            synth.project_path.to_str().unwrap(),
            "--format",
            "png",
            "--xmp",
            "on",
            "--iptc",
            "on",
            "-o",
            png_out.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(r.success, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(report["metadata"]["xmp"], serde_json::json!("written"));
    assert_eq!(report["metadata"]["iptc"], serde_json::json!("unsupported"));
    let file = std::fs::File::open(&png_out).unwrap();
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let reader = decoder.read_info().unwrap();
    let itxt = reader
        .info()
        .utf8_text
        .iter()
        .find(|c| c.keyword == "XML:com.adobe.xmp")
        .expect("iTXt XMP chunk");
    assert_eq!(itxt.get_text().unwrap().as_bytes(), xmp.as_slice());

    // A RAW-sourced project carries neither: rawler surfaces no XMP packet.
    let (raw_dir, raw_project) =
        build_raw_metadata_synth("export_formats_xmp_iptc_raw", "RawMake", "RawModel");
    let raw_out = raw_dir.join("out.tif");
    let r = run_cli_in(
        raw_project.parent().unwrap(),
        &[
            "export",
            raw_project.to_str().unwrap(),
            "--xmp",
            "on",
            "--iptc",
            "on",
            "-o",
            raw_out.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(r.success, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(report["metadata"]["xmp"], serde_json::json!("absent"));
    assert_eq!(report["metadata"]["iptc"], serde_json::json!("absent"));

    // An XMP packet too large for one JPEG APP1 segment is skipped, with a
    // warning naming the size.
    let huge_xmp = vec![b'x'; 70_000];
    let huge_source = rich_frame_metadata("HugeXmpCam", None, None, Some(&huge_xmp), None);
    let huge_synth = build_metadata_synth("export_formats_xmp_huge", &[huge_source], None);
    let huge_out_dir = common::tmp_dir("export_formats_xmp_huge_out");
    let huge_out = huge_out_dir.join("out.jpg");
    let r = run_cli_in(
        huge_synth.project_path.parent().unwrap(),
        &[
            "export",
            huge_synth.project_path.to_str().unwrap(),
            "--format",
            "jpeg",
            "--xmp",
            "on",
            "-o",
            huge_out.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(r.success, "{}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(report["metadata"]["xmp"], serde_json::json!("skipped"));
    assert!(
        report["metadata"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("65502")),
        "{report}"
    );
    assert!(jpeg_xmp_packet(&std::fs::read(&huge_out).unwrap()).is_none());
}

// A junk EXIF pointer or a truncated ICC entry still exports the
// image, exits zero, and states the reason on stderr and in `--json`'s
// `metadata.warnings`; a source whose only EXIF entry is a junk pointer
// reports `exif: absent` and exactly one warning.
#[test]
fn export_survives_damaged_or_missing_source_metadata() {
    // A junk EXIF pointer and nothing else: the frame carries no whitelisted
    // IFD0 tag, no GPS, and its only 34665 entry resolves nowhere real.
    let junk = FrameMetadata {
        ifd0: vec![(34665, 4, longs_bo(ByteOrder::Little, &[0]))],
        ..Default::default()
    };
    let junk_synth = build_metadata_synth("export_formats_survives_junk_pointer", &[junk], None);
    let junk_out_dir = common::tmp_dir("export_formats_survives_junk_pointer_out");
    let junk_out = junk_out_dir.join("out.tif");
    let r = run_cli_in(
        junk_synth.project_path.parent().unwrap(),
        &[
            "export",
            junk_synth.project_path.to_str().unwrap(),
            "-o",
            junk_out.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(r.success, "{}", r.stderr);
    assert!(r.stderr.contains("EXIF sub-IFD"), "stderr: {}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(report["metadata"]["exif"], serde_json::json!("absent"));
    let warnings = report["metadata"]["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{report}");
    assert!(warnings[0].as_str().unwrap().contains("EXIF sub-IFD"));

    // A truncated ICC entry: the corruption must happen before the project
    // is sealed, since sealing hashes every source and a render refuses one
    // whose hash has since changed.
    let dir = common::tmp_dir("export_formats_survives_truncated_icc");
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let frame_path = dir.join("frame-00.tif");
    write_frame_with_metadata(
        &frame_path,
        MW,
        MH,
        &FrameMetadata {
            ifd0: vec![(34675, 7, test_icc(9))],
            ..Default::default()
        },
    )
    .unwrap();
    corrupt_ifd0_entry_count(&frame_path, 34675, 0xFFFF_FFF0);

    let composite_path = staging.join("composite.tif");
    write_tiff(
        &composite_path,
        MW,
        MH,
        3,
        &gradient_samples(MW, MH, 3),
        Some("staged composite"),
    );
    let zmap_path = staging.join("zmap.tif");
    let mut map = ZMapWriter::create(&zmap_path, ZMapInfo::new(MW, MH, 1).unwrap()).unwrap();
    map.write_tile(full(MW, MH), &vec![0u16; (MW * MH) as usize])
        .unwrap();
    map.finish().unwrap();
    let project_path = dir.join(format!("trunc.{}", project::PROJECT_EXTENSION));
    project::seal(SealRequest {
        project_path: &project_path,
        staged_composite: &composite_path,
        staged_zmap: &zmap_path,
        sources: &[frame_path],
        algorithm: "laplacian",
        params: &ParamValues::new(),
        align: None,
        registration: Registration::default(),
        composite_description: "staged composite".to_string(),
        label: None,
        metrics: Metrics::default(),
    })
    .expect("seal must succeed");

    let out = dir.join("out.tif");
    let r = run_cli_in(
        project_path.parent().unwrap(),
        &[
            "export",
            project_path.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(r.success, "{}", r.stderr);
    assert!(r.stderr.contains("ICC profile"), "stderr: {}", r.stderr);
    let report: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(report["metadata"]["icc"], serde_json::json!("none"));
    assert!(
        report["metadata"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("ICC profile")),
        "{report}"
    );
}
