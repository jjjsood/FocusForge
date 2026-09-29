// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli export`'s per-format options: TIFF bit depth and
//! compression, PNG bit depth and compression, JPEG quality and chroma
//! subsampling, extension inference and every refusal. A single-frame
//! gradient project, sized off a multiple of 16 and spanning more than one
//! export band, gives compression and 8-bit rounding something to do.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use focusforge_core::align::Registration;
use focusforge_core::io::{Rect, SegmentOrigin, TiffSegmentReader, TiffSpec, TiffWriter};
use focusforge_core::params::ParamValues;
use focusforge_core::project::export::to_8bit;
use focusforge_core::project::{self, Metrics, Op, Point, SealRequest, ZMapStorage};
use focusforge_core::zmap::{ZMapInfo, ZMapWriter};

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
        ZMapStorage::Bounded,
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

    // An unwritable destination.
    let synth = build_synth("export_formats_refused_unwritable", 3);
    let readonly_dir = dir.join("readonly");
    std::fs::create_dir_all(&readonly_dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&readonly_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    }
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&readonly_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert!(!out.exists());
    assert!(!partial_of(&out).exists());

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
