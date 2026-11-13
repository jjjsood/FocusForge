// SPDX-License-Identifier: GPL-3.0-or-later

//! A pure estimate of how long a stacking run takes, from its plan.
//!
//! [`predict_run`] maps a [`RunPlanFeatures`] record to seconds per stage
//! and a total, before any stage has started. The seconds are for the
//! reference machine, a 4-core Intel Core i5-8265U laptop (turbo on,
//! default host budget; Intel UHD Graphics 620 for the GPU), on a cold run
//! of a `focusforge-cli 0.2.0` release build. They are a scale, not a
//! promise for another machine.
//!
//! # Form
//!
//! With `F` frames of `P` megapixels each, per backend:
//!
//! | stage    | seconds                                                  |
//! |----------|----------------------------------------------------------|
//! | setup    | `setup_fixed + setup_per_frame * F`                      |
//! | develop  | `develop * F * P` (RAW readers only)                     |
//! | align    | `align * (F - 1) * P` (only when alignment is computed)  |
//! | fuse     | `fuse * F * P`                                           |
//! | finalize | `finalize * P`                                           |
//!
//! `warp_write` is never predicted: a `stack` run does not enter it. The
//! worker count has no term (it followed the frame count in every
//! measured run), nor does the source bit depth or brightness
//! equalization. A resumed run gets no align term, but fuse is not
//! shrunk by the tiles it skips: the record carries no tile total, so a
//! resumed prediction is an upper bound for fuse.
//!
//! # Fit
//!
//! Each coefficient is a least-squares fit of measured stage times. TIFF
//! terms come from 61-, 32- and 8-frame 16-bit TIFF stacks (15.0 and
//! 0.26 Mpx frames); the RAW terms come from one 25-frame, 17.3 Mpx Canon
//! CR3 stack developed into memory. Held out, a 30-frame TIFF stack and a
//! different 25-frame CR3 stack (25 other frames of the same burst), each
//! on CPU and GPU with one cold and three warm runs, all 16 predictions
//! land within ±20 % of the measured wall time, the worst −13.0 % (cold
//! GPU, CR3). TIFF runs miss by −1.2 to +2.6 %, warm CR3 runs by −2.6 to
//! −8.2 %. The other classes reuse the TIFF setup terms and fit their stage
//! terms on the same machine. A 30-frame 15.0 Mpx stack, as 16-bit Deflate
//! TIFF (horizontal predictor, strips) and as quality-95 JPEG, fits the
//! `decoded` and JPEG classes; a 32-frame 15.0 Mpx stack held out predicts
//! them within +1.4 to +3.1 % and +1.8 to +4.1 %, every run within ±20 %.
//!
//! # Calibrated classes
//!
//! A row of `CALIBRATIONS` is one measured class: reader, RAW file
//! extension, develop store and blend algorithm. Measured: uncompressed
//! 16-bit strip TIFF read through `mmap` (`laplacian`, `depth-map`,
//! `weighted-average`), compressed TIFF (`decoded`) and JPEG (`laplacian`),
//! and Canon CR3 developed into memory or into a scratch file
//! (`laplacian`). Every other input still gets a prediction, from the first
//! row of its family, with [`RunPrediction::calibrated`] false.
//!
//! Each blend has one row per planned align backend, fitted on a 30-frame
//! 15.0 Mpx mantis subset and held out on a 32-frame beetle stack (16-bit
//! TIFF, reference laptop). Held-out error, `--cpu` / GPU-backed align:
//! `depth-map` −5.1 to −3.9 % / −3.7 to −0.3 %, `weighted-average`
//! −4.1 to −3.4 % / −3.0 to +0.5 %. CR3 into a scratch file: two disjoint
//! 25-frame 17.3 Mpx stacks, 6 GiB host budget, −8.0 to +1.2 %.
//!
//! `calibrated` reads only what the record and [`RawSource`] carry. It
//! cannot see which camera wrote a CR3 (two disjoint 25-frame stacks of
//! one Canon EOS R5 Mark II burst were measured, one fitted and one held
//! out; the CR3 develop rate predicts a RAF stack's develop 54 % low
//! and an RW2 stack's 17 % high), whether the frames are a real focus
//! stack (repeated frames run far cheaper), the scene (align time differs
//! 17 to 20 % between two halves of one burst), frame sizes and counts far
//! from the measured ones (RAW frame-count and size scaling is assumed),
//! compression, layout or bit depth within the `decoded` reader (only
//! 16-bit Deflate strips were measured), `--no-align` on decoded or JPEG
//! frames (decode then falls in fuse), decoded frames beyond the frame
//! cache, an align accelerator other than the measured integrated GPU, a
//! host where the offered accelerator finds no adapter, the host budget of
//! a scratch-file run (6 GiB), brightness equalization, or a cold GPU (6 to
//! 19 % more fuse time than a warm one).
//! Time outside the stages, up to 0.4 s warm, is not modelled.

use serde::Serialize;

use crate::io::{DevelopStore, ReaderKind};
use crate::pipeline::RunPlanFeatures;
use crate::timing::StageTimings;

/// Seconds per unit of work for each modelled stage, on one backend.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StageCost {
    /// Seconds of setup independent of the frame count.
    pub(crate) setup_fixed: f64,
    /// Seconds of setup per frame.
    pub(crate) setup_per_frame: f64,
    /// Seconds per frame-megapixel of develop; read for RAW readers only.
    pub(crate) develop: f64,
    /// Seconds per megapixel per neighbouring frame pair of alignment.
    pub(crate) align: f64,
    /// Seconds per frame-megapixel of fuse.
    pub(crate) fuse: f64,
    /// Seconds per megapixel of one source frame of finalize.
    pub(crate) finalize: f64,
}

/// One measured class and its costs per backend.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Calibration {
    pub(crate) reader: ReaderKind,
    /// Lower-case RAW file extension, `None` for a non-RAW reader.
    pub(crate) raw_extension: Option<&'static str>,
    /// Develop store of a RAW class, `None` for a non-RAW reader.
    pub(crate) raw_store: Option<DevelopStore>,
    /// Blend algorithm id.
    pub(crate) algorithm: &'static str,
    /// `Some` keys the row to one planned align backend (`true` for a GPU
    /// accelerator), `None` matches either.
    pub(crate) align_gpu: Option<bool>,
    pub(crate) cpu: StageCost,
    pub(crate) gpu: StageCost,
}

/// The measured classes. A record matches a row exactly (reader, blend
/// algorithm, the planned align backend where the row sets one and, for RAW,
/// extension and develop store) and is then `calibrated`; otherwise the first
/// row of its family (RAW or not) is the fallback.
///
/// - Uncompressed 16-bit TIFF, `mmap`, `laplacian`: least squares over 61-,
///   32- and 8-frame stacks of 15.0 and 0.26 Mpx frames on the reference
///   machine, CPU and Intel UHD Graphics 620.
/// - Canon CR3 into memory, `laplacian`: one 25-frame 17.3 Mpx stack on the
///   same machine, CPU and GPU (a second, disjoint 25-frame stack of the
///   same burst was held out). Setup is the TIFF fit's.
/// - `depth-map` and `weighted-average`, `mmap`: one row per planned align
///   backend, each a 30-frame 15.0 Mpx stack on the same machine, held out
///   on a 32-frame stack of the same size.
///
/// Setup is the `mmap` TIFF fit's in every row. The other rows' stacks and
/// held-out errors are in the module docs. A blend row repeats its `cpu`
/// costs in `gpu`, which no plan with that blend reads.
pub(crate) const CALIBRATIONS: &[Calibration] = &[
    Calibration {
        reader: ReaderKind::Mmap,
        raw_extension: None,
        raw_store: None,
        algorithm: "laplacian",
        align_gpu: None,
        cpu: StageCost {
            setup_fixed: 5.700_419_114_984_098e-5,
            setup_per_frame: 0.000_325_912_683_680_398_4,
            develop: 0.0,
            align: 0.028_569_888_671_864_073,
            fuse: 0.208_772_574_999_624_76,
            finalize: 0.075_571_671_849_717_89,
        },
        gpu: StageCost {
            setup_fixed: 0.000_106_572_996_944_319_9,
            setup_per_frame: 0.000_320_863_014_825_713,
            develop: 0.0,
            align: 0.016_830_933_786_359_185,
            fuse: 0.103_978_535_006_414_1,
            finalize: 0.082_810_151_824_623_05,
        },
    },
    Calibration {
        reader: ReaderKind::Raw,
        raw_extension: Some("cr3"),
        raw_store: Some(DevelopStore::Memory),
        algorithm: "laplacian",
        align_gpu: None,
        cpu: StageCost {
            setup_fixed: 5.700_419_114_984_098e-5,
            setup_per_frame: 0.000_325_912_683_680_398_4,
            develop: 0.021_791_716_757_937_656,
            align: 0.166_588_890_182_213_23,
            fuse: 0.204_399_389_000_640_6,
            finalize: 0.166_311_269_519_082_07,
        },
        gpu: StageCost {
            setup_fixed: 0.000_106_572_996_944_319_9,
            setup_per_frame: 0.000_320_863_014_825_713,
            develop: 0.021_919_317_046_514_37,
            align: 0.045_501_246_753_700_32,
            fuse: 0.114_348_145_913_178_07,
            finalize: 0.175_458_290_774_584_3,
        },
    },
    Calibration {
        reader: ReaderKind::Decoded,
        raw_extension: None,
        raw_store: None,
        algorithm: "laplacian",
        align_gpu: None,
        cpu: StageCost {
            setup_fixed: 5.700_419_114_984_098e-5,
            setup_per_frame: 0.000_325_912_683_680_398_4,
            develop: 0.0,
            align: 0.039_322_546_807_054_326,
            fuse: 0.203_596_291_748_992_77,
            finalize: 0.073_252_178_835_414_63,
        },
        gpu: StageCost {
            setup_fixed: 0.000_106_572_996_944_319_9,
            setup_per_frame: 0.000_320_863_014_825_713,
            develop: 0.0,
            align: 0.026_784_707_328_002_026,
            fuse: 0.103_915_241_358_714_06,
            finalize: 0.079_923_559_445_151_74,
        },
    },
    Calibration {
        reader: ReaderKind::Jpeg,
        raw_extension: None,
        raw_store: None,
        algorithm: "laplacian",
        align_gpu: None,
        cpu: StageCost {
            setup_fixed: 5.700_419_114_984_098e-5,
            setup_per_frame: 0.000_325_912_683_680_398_4,
            develop: 0.0,
            align: 0.034_773_364_314_026_854,
            fuse: 0.200_792_003_411_743_03,
            finalize: 0.065_759_544_850_150_9,
        },
        gpu: StageCost {
            setup_fixed: 0.000_106_572_996_944_319_9,
            setup_per_frame: 0.000_320_863_014_825_713,
            develop: 0.0,
            align: 0.020_167_923_638_218_132,
            fuse: 0.104_480_860_154_241_32,
            finalize: 0.075_106_655_970_273_5,
        },
    },
    Calibration {
        reader: ReaderKind::Mmap,
        raw_extension: None,
        raw_store: None,
        algorithm: "depth-map",
        align_gpu: Some(false),
        cpu: StageCost {
            setup_fixed: 5.700_419_114_984_098e-5,
            setup_per_frame: 0.000_325_912_683_680_398_4,
            develop: 0.0,
            align: 0.028_970_981_528_786_313,
            fuse: 0.063_053_574_996_851_91,
            finalize: 0.074_010_086_961_996_7,
        },
        gpu: StageCost {
            setup_fixed: 5.700_419_114_984_098e-5,
            setup_per_frame: 0.000_325_912_683_680_398_4,
            develop: 0.0,
            align: 0.028_970_981_528_786_313,
            fuse: 0.063_053_574_996_851_91,
            finalize: 0.074_010_086_961_996_7,
        },
    },
    Calibration {
        reader: ReaderKind::Mmap,
        raw_extension: None,
        raw_store: None,
        algorithm: "depth-map",
        align_gpu: Some(true),
        cpu: StageCost {
            setup_fixed: 5.700_419_114_984_098e-5,
            setup_per_frame: 0.000_325_912_683_680_398_4,
            develop: 0.0,
            align: 0.018_109_428_578_254_646,
            fuse: 0.059_907_682_187_174_96,
            finalize: 0.073_609_970_652_727_22,
        },
        gpu: StageCost {
            setup_fixed: 5.700_419_114_984_098e-5,
            setup_per_frame: 0.000_325_912_683_680_398_4,
            develop: 0.0,
            align: 0.018_109_428_578_254_646,
            fuse: 0.059_907_682_187_174_96,
            finalize: 0.073_609_970_652_727_22,
        },
    },
    Calibration {
        reader: ReaderKind::Mmap,
        raw_extension: None,
        raw_store: None,
        algorithm: "weighted-average",
        align_gpu: Some(false),
        cpu: StageCost {
            setup_fixed: 5.700_419_114_984_098e-5,
            setup_per_frame: 0.000_325_912_683_680_398_4,
            develop: 0.0,
            align: 0.028_792_422_211_439_517,
            fuse: 0.070_604_818_618_745_08,
            finalize: 0.074_370_095_865_356_7,
        },
        gpu: StageCost {
            setup_fixed: 5.700_419_114_984_098e-5,
            setup_per_frame: 0.000_325_912_683_680_398_4,
            develop: 0.0,
            align: 0.028_792_422_211_439_517,
            fuse: 0.070_604_818_618_745_08,
            finalize: 0.074_370_095_865_356_7,
        },
    },
    Calibration {
        reader: ReaderKind::Mmap,
        raw_extension: None,
        raw_store: None,
        algorithm: "weighted-average",
        align_gpu: Some(true),
        cpu: StageCost {
            setup_fixed: 5.700_419_114_984_098e-5,
            setup_per_frame: 0.000_325_912_683_680_398_4,
            develop: 0.0,
            align: 0.018_216_272_369_680_28,
            fuse: 0.067_472_151_672_907_71,
            finalize: 0.075_011_924_409_679_7,
        },
        gpu: StageCost {
            setup_fixed: 5.700_419_114_984_098e-5,
            setup_per_frame: 0.000_325_912_683_680_398_4,
            develop: 0.0,
            align: 0.018_216_272_369_680_28,
            fuse: 0.067_472_151_672_907_71,
            finalize: 0.075_011_924_409_679_7,
        },
    },
    Calibration {
        reader: ReaderKind::Raw,
        raw_extension: Some("cr3"),
        raw_store: Some(DevelopStore::ScratchFile),
        algorithm: "laplacian",
        align_gpu: None,
        cpu: StageCost {
            setup_fixed: 5.700_419_114_984_098e-5,
            setup_per_frame: 0.000_325_912_683_680_398_4,
            develop: 0.021_558_968_534_685_755,
            align: 0.166_238_222_430_539_9,
            fuse: 0.204_963_900_580_328_68,
            finalize: 0.167_254_499_378_161_3,
        },
        gpu: StageCost {
            setup_fixed: 0.000_106_572_996_944_319_9,
            setup_per_frame: 0.000_320_863_014_825_713,
            develop: 0.021_623_588_451_739_943,
            align: 0.046_545_544_461_478_565,
            fuse: 0.121_900_874_836_370_44,
            finalize: 0.171_252_801_069_687_63,
        },
    },
];

/// The RAW format and develop store of a run, which [`RunPlanFeatures`]
/// does not carry.
#[derive(Debug, Clone, Copy)]
pub struct RawSource<'a> {
    /// File extension of the first frame, with or without case.
    pub extension: &'a str,
    /// Where the developed frames live.
    pub store: DevelopStore,
}

/// Predicted seconds for a cold run on the reference machine.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RunPrediction {
    /// Per stage; `None` for a stage the plan does not enter.
    pub stages: StageTimings,
    /// Sum of the present stages.
    pub total_seconds: f64,
    /// Whether the input matches a measured class (see the module docs).
    pub calibrated: bool,
}

/// The row for `features`, and whether it is an exact match. A RAW record
/// without `raw` never matches; `raw` is ignored for other readers.
fn calibration<'t>(
    table: &'t [Calibration],
    features: &RunPlanFeatures,
    raw: Option<RawSource<'_>>,
) -> (&'t Calibration, bool) {
    let is_raw = features.reader == ReaderKind::Raw;
    let exact = table.iter().find(|row| {
        row.reader == features.reader
            && row.algorithm == features.algorithm
            && row.align_gpu.is_none_or(|gpu| gpu == features.align_gpu)
            && (!is_raw
                || raw.is_some_and(|raw| {
                    row.raw_extension
                        .is_some_and(|ext| ext.eq_ignore_ascii_case(raw.extension))
                        && row.raw_store == Some(raw.store)
                }))
    });
    match exact {
        Some(row) => (row, true),
        None => (
            table
                .iter()
                .find(|row| (row.reader == ReaderKind::Raw) == is_raw)
                .expect("the table holds a fallback row for each family"),
            false,
        ),
    }
}

/// Predicts the stages of a cold run. `align` is whether the run computes
/// an alignment; it is spent only when the record shows no cache hit and
/// nothing resumed. `None` when the stack has no frames or no pixels.
pub fn predict_run(
    features: &RunPlanFeatures,
    align: bool,
    raw: Option<RawSource<'_>>,
) -> Option<RunPrediction> {
    if features.frame_count == 0 || features.frame_pixels == 0 {
        return None;
    }
    let (row, calibrated) = calibration(CALIBRATIONS, features, raw);
    let cost = if features.gpu { row.gpu } else { row.cpu };
    let frames = features.frame_count as f64;
    let megapixels = features.frame_pixels as f64 / 1e6;
    let stages = StageTimings {
        setup: Some(cost.setup_fixed + cost.setup_per_frame * frames),
        develop: (features.reader == ReaderKind::Raw).then_some(cost.develop * frames * megapixels),
        align: (align && !features.align_cache_hit && features.resumed_units == 0)
            .then(|| cost.align * features.frame_count.saturating_sub(1) as f64 * megapixels),
        fuse: Some(cost.fuse * frames * megapixels),
        warp_write: None,
        finalize: Some(cost.finalize * megapixels),
    };
    let total_seconds = [
        stages.setup,
        stages.develop,
        stages.align,
        stages.fuse,
        stages.finalize,
    ]
    .into_iter()
    .flatten()
    .sum();
    Some(RunPrediction {
        stages,
        total_seconds,
        calibrated,
    })
}

/// Seconds per read megapixel per frame and per edge tile per frame of one
/// fuse tile, on one backend.
struct TileCost {
    per_read_mpx_per_frame: f64,
    per_edge_tile_per_frame: f64,
}

/// Least squares through the origin over 1688 fuse tiles of the TIFF stacks
/// and the 25-frame CR3 stack on the reference machine's CPU; the fit's rms
/// relative error per tile is 0.40.
const CPU_TILE_COST: TileCost = TileCost {
    per_read_mpx_per_frame: 0.524_561_692_657_810_3,
    per_edge_tile_per_frame: 0.011_965_701_220_045_864,
};

/// The same fit on the reference machine's Intel UHD Graphics 620 (rms
/// relative error per tile 0.25).
const GPU_TILE_COST: TileCost = TileCost {
    per_read_mpx_per_frame: 0.248_637_029_459_205_1,
    per_edge_tile_per_frame: 0.025_883_198_735_390_182,
};

/// Relative weight of one fuse tile: `frame_count * (p * read_megapixels +
/// q * edge)` with the fitted `p`, `q` of the backend. The inputs are those
/// a fuse tile's trace records. It weights fuse progress; it is not part of
/// [`predict_run`].
pub fn fuse_tile_cost(gpu: bool, frame_count: usize, read_pixels: u64, touches_edge: bool) -> f64 {
    let cost = if gpu { GPU_TILE_COST } else { CPU_TILE_COST };
    let edge = if touches_edge {
        cost.per_edge_tile_per_frame
    } else {
        0.0
    };
    frame_count as f64 * (cost.per_read_mpx_per_frame * read_pixels as f64 / 1e6 + edge)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CR3: Option<RawSource<'static>> = Some(RawSource {
        extension: "CR3",
        store: DevelopStore::Memory,
    });
    const READERS: [ReaderKind; 4] = [
        ReaderKind::Mmap,
        ReaderKind::Decoded,
        ReaderKind::Raw,
        ReaderKind::Jpeg,
    ];

    fn record(reader: ReaderKind, gpu: bool, frames: usize, pixels: u64) -> RunPlanFeatures {
        RunPlanFeatures {
            frame_count: frames,
            frame_pixels: pixels,
            source_bits_per_sample: 16,
            reader,
            gpu,
            align_gpu: false,
            workers: 4,
            algorithm: "laplacian".to_string(),
            align_cache_hit: false,
            resumed_units: 0,
        }
    }

    fn stage_list(s: &StageTimings) -> [Option<f64>; 6] {
        [
            s.setup,
            s.develop,
            s.align,
            s.fuse,
            s.warp_write,
            s.finalize,
        ]
    }

    fn rel_close(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol * a.abs().max(b.abs())
    }

    #[test]
    fn run_prediction_stages_present_and_absent() {
        for reader in READERS {
            for gpu in [false, true] {
                for frames in [1usize, 2, 25, 200] {
                    for pixels in [1u64, 4096, 17_258_496, 100_000_000] {
                        // (align requested, cache hit, resumed units, align expected)
                        let modes = [
                            (false, false, 0, false),
                            (true, false, 0, true),
                            (true, true, 0, false),
                            (true, false, 3, false),
                        ];
                        for (align, hit, resumed, align_expected) in modes {
                            let mut rec = record(reader, gpu, frames, pixels);
                            rec.align_cache_hit = hit;
                            rec.resumed_units = resumed;
                            let at = format!(
                                "{reader:?} gpu={gpu} F={frames} px={pixels} {align}/{hit}/{resumed}"
                            );
                            let p = predict_run(&rec, align, CR3).expect(&at);
                            let s = p.stages;
                            assert!(
                                s.setup.is_some() && s.fuse.is_some() && s.finalize.is_some(),
                                "{at}"
                            );
                            assert_eq!(s.develop.is_some(), reader == ReaderKind::Raw, "{at}");
                            assert_eq!(s.align.is_some(), align_expected, "{at}");
                            if align_expected && frames == 1 {
                                assert_eq!(s.align, Some(0.0), "{at}");
                            }
                            assert_eq!(s.warp_write, None, "{at}");
                            let mut sum = 0.0;
                            for v in stage_list(&s).into_iter().flatten() {
                                assert!(v.is_finite() && v >= 0.0, "{at}: {v}");
                                sum += v;
                            }
                            assert!(rel_close(p.total_seconds, sum, 1e-9), "{at}");

                            let mut other = rec.clone();
                            other.workers = 64;
                            let q = predict_run(&other, align, CR3).expect(&at);
                            assert_eq!(q.stages, p.stages, "{at}: workers move seconds");

                            let family = if reader == ReaderKind::Raw {
                                ReaderKind::Raw
                            } else {
                                ReaderKind::Mmap
                            };
                            let mut fallback = record(family, gpu, frames, pixels);
                            fallback.align_cache_hit = hit;
                            fallback.resumed_units = resumed;
                            let mut unknown = rec.clone();
                            unknown.algorithm = "no-such-algorithm".to_string();
                            let u = predict_run(&unknown, align, CR3).expect(&at);
                            let f = predict_run(&fallback, align, CR3).expect(&at);
                            assert!(!u.calibrated, "{at}");
                            assert_eq!(u.stages, f.stages, "{at}: unknown id misses the fallback");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn run_prediction_monotone() {
        for cal in CALIBRATIONS {
            for c in [cal.cpu, cal.gpu] {
                for v in [
                    c.setup_fixed,
                    c.setup_per_frame,
                    c.develop,
                    c.align,
                    c.fuse,
                    c.finalize,
                ] {
                    assert!(v >= 0.0 && v.is_finite());
                }
            }
        }
        let at_least = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) => a >= b,
            (None, None) => true,
            _ => false,
        };
        for row in CALIBRATIONS {
            let raw = row
                .raw_extension
                .zip(row.raw_store)
                .map(|(extension, store)| RawSource { extension, store });
            let key = |gpu, frames, pixels| {
                let mut rec = record(row.reader, gpu, frames, pixels);
                rec.algorithm = row.algorithm.to_string();
                rec
            };
            for gpu in [false, true] {
                let at = format!("{:?} {} gpu={gpu}", row.reader, row.algorithm);
                let base = predict_run(&key(gpu, 10, 1_000_000), true, raw).unwrap();
                assert!(base.calibrated, "{at}");
                assert!(base.total_seconds > 0.0);
                for bigger in [key(gpu, 20, 1_000_000), key(gpu, 10, 2_000_000)] {
                    let p = predict_run(&bigger, true, raw).unwrap();
                    assert!(p.total_seconds >= base.total_seconds);
                    for (a, b) in stage_list(&p.stages)
                        .into_iter()
                        .zip(stage_list(&base.stages))
                    {
                        assert!(at_least(a, b), "{at}: {a:?} < {b:?}");
                    }
                }
                let mut hit = key(gpu, 10, 1_000_000);
                hit.align_cache_hit = true;
                let mut resumed = key(gpu, 10, 1_000_000);
                resumed.resumed_units = 5;
                for rec in [hit, resumed] {
                    let p = predict_run(&rec, true, raw).unwrap();
                    assert!(p.total_seconds <= base.total_seconds);
                    assert!(p.stages.align.unwrap_or(0.0) <= base.stages.align.unwrap());
                }
            }
        }
    }

    #[test]
    fn run_prediction_matches_fit() {
        // frames, pixels, reader, gpu, [setup, develop, align, fuse, finalize], total
        type Row = (usize, u64, ReaderKind, bool, [Option<f64>; 5], f64);
        let rows: [Row; 6] = [
            (
                30,
                14_993_216,
                ReaderKind::Mmap,
                false,
                [
                    Some(0.0098),
                    None,
                    Some(12.4223),
                    Some(93.9052),
                    Some(1.1331),
                ],
                107.470,
            ),
            (
                30,
                14_993_216,
                ReaderKind::Mmap,
                true,
                [
                    Some(0.0097),
                    None,
                    Some(7.3181),
                    Some(46.7692),
                    Some(1.2416),
                ],
                55.339,
            ),
            (
                25,
                17_258_496,
                ReaderKind::Raw,
                false,
                [
                    Some(0.0082),
                    Some(9.4023),
                    Some(69.0018),
                    Some(88.1907),
                    Some(2.8703),
                ],
                169.473,
            ),
            (
                25,
                17_258_496,
                ReaderKind::Raw,
                true,
                [
                    Some(0.0081),
                    Some(9.4574),
                    Some(18.8468),
                    Some(49.3369),
                    Some(3.0281),
                ],
                80.677,
            ),
            (
                61,
                14_993_216,
                ReaderKind::Mmap,
                false,
                [
                    Some(0.0199),
                    None,
                    Some(25.7013),
                    Some(190.9405),
                    Some(1.1331),
                ],
                217.795,
            ),
            (
                61,
                14_993_216,
                ReaderKind::Mmap,
                true,
                [
                    Some(0.0197),
                    None,
                    Some(15.1410),
                    Some(95.0973),
                    Some(1.2416),
                ],
                111.500,
            ),
        ];
        for (frames, pixels, reader, gpu, want, total) in rows {
            let at = format!("{reader:?} gpu={gpu} F={frames}");
            let p = predict_run(&record(reader, gpu, frames, pixels), true, CR3).expect(&at);
            assert!(p.calibrated, "{at}");
            let s = p.stages;
            let got = [s.setup, s.develop, s.align, s.fuse, s.finalize];
            for (i, (g, w)) in got.into_iter().zip(want).enumerate() {
                match (g, w) {
                    (Some(g), Some(w)) if i == 0 => assert!((g - w).abs() < 1e-4, "{at} setup {g}"),
                    (Some(g), Some(w)) => {
                        assert!(rel_close(g, w, 0.01), "{at} stage {i}: {g} vs {w}");
                    }
                    (None, None) => {}
                    other => panic!("{at} stage {i}: {other:?}"),
                }
            }
            assert!(
                rel_close(p.total_seconds, total, 0.01),
                "{at} total {}",
                p.total_seconds
            );
        }
    }

    #[test]
    fn run_prediction_matches_class_fits() {
        let scratch = Some(RawSource {
            extension: "cr3",
            store: DevelopStore::ScratchFile,
        });
        // class, frames, pixels, reader, algorithm, raw, gpu, align_gpu,
        // [setup, develop, align, fuse, finalize], total
        type Row = (
            &'static str,
            usize,
            u64,
            ReaderKind,
            &'static str,
            Option<RawSource<'static>>,
            bool,
            bool,
            [Option<f64>; 5],
            f64,
        );
        let tiff = 14_997_654;
        let raw = 17_258_496;
        let rows: [Row; 10] = [
            (
                "decoded cpu",
                32,
                tiff,
                ReaderKind::Decoded,
                "laplacian",
                None,
                false,
                false,
                [
                    Some(0.0105),
                    None,
                    Some(18.2821),
                    Some(97.7109),
                    Some(1.0986),
                ],
                117.102,
            ),
            (
                "decoded gpu",
                32,
                tiff,
                ReaderKind::Decoded,
                "laplacian",
                None,
                true,
                false,
                [
                    Some(0.0104),
                    None,
                    Some(12.4529),
                    Some(49.8715),
                    Some(1.1987),
                ],
                63.533,
            ),
            (
                "jpeg cpu",
                32,
                tiff,
                ReaderKind::Jpeg,
                "laplacian",
                None,
                false,
                false,
                [
                    Some(0.0105),
                    None,
                    Some(16.1671),
                    Some(96.3651),
                    Some(0.9862),
                ],
                113.529,
            ),
            (
                "jpeg gpu",
                32,
                tiff,
                ReaderKind::Jpeg,
                "laplacian",
                None,
                true,
                false,
                [
                    Some(0.0104),
                    None,
                    Some(9.3766),
                    Some(50.1430),
                    Some(1.1264),
                ],
                60.656,
            ),
            (
                "depth-map cpu align",
                32,
                tiff,
                ReaderKind::Mmap,
                "depth-map",
                None,
                false,
                false,
                [
                    Some(0.0105),
                    None,
                    Some(13.4694),
                    Some(30.2610),
                    Some(1.1100),
                ],
                44.851,
            ),
            (
                "depth-map gpu align",
                32,
                tiff,
                ReaderKind::Mmap,
                "depth-map",
                None,
                false,
                true,
                [
                    Some(0.0105),
                    None,
                    Some(8.4196),
                    Some(28.7512),
                    Some(1.1040),
                ],
                38.285,
            ),
            (
                "weighted-average cpu align",
                32,
                tiff,
                ReaderKind::Mmap,
                "weighted-average",
                None,
                false,
                false,
                [
                    Some(0.0105),
                    None,
                    Some(13.3864),
                    Some(33.8850),
                    Some(1.1154),
                ],
                48.397,
            ),
            (
                "weighted-average gpu align",
                32,
                tiff,
                ReaderKind::Mmap,
                "weighted-average",
                None,
                false,
                true,
                [
                    Some(0.0105),
                    None,
                    Some(8.4692),
                    Some(32.3816),
                    Some(1.1250),
                ],
                41.986,
            ),
            (
                "cr3 scratch cpu",
                25,
                raw,
                ReaderKind::Raw,
                "laplacian",
                scratch,
                false,
                false,
                [
                    Some(0.0082),
                    Some(9.3019),
                    Some(68.8565),
                    Some(88.4342),
                    Some(2.8866),
                ],
                169.487,
            ),
            (
                "cr3 scratch gpu",
                25,
                raw,
                ReaderKind::Raw,
                "laplacian",
                scratch,
                true,
                false,
                [
                    Some(0.0081),
                    Some(9.3298),
                    Some(19.2793),
                    Some(52.5956),
                    Some(2.9556),
                ],
                84.168,
            ),
        ];
        for (class, frames, pixels, reader, algorithm, source, gpu, align_gpu, want, total) in rows
        {
            let mut rec = record(reader, gpu, frames, pixels);
            rec.algorithm = algorithm.to_string();
            rec.align_gpu = align_gpu;
            let p = predict_run(&rec, true, source).expect(class);
            assert!(p.calibrated, "{class}");
            let s = p.stages;
            let got = [s.setup, s.develop, s.align, s.fuse, s.finalize];
            for (i, (g, w)) in got.into_iter().zip(want).enumerate() {
                match (g, w) {
                    (Some(g), Some(w)) if i == 0 => {
                        assert!((g - w).abs() < 1e-4, "{class} setup {g}");
                    }
                    (Some(g), Some(w)) => {
                        assert!(rel_close(g, w, 0.01), "{class} stage {i}: {g} vs {w}");
                    }
                    (None, None) => {}
                    other => panic!("{class} stage {i}: {other:?}"),
                }
            }
            assert!(
                rel_close(p.total_seconds, total, 0.01),
                "{class} total {}",
                p.total_seconds
            );
        }
    }

    #[test]
    fn run_prediction_degenerate() {
        for (frames, pixels) in [(0usize, 1_000_000u64), (5, 0), (0, 0)] {
            for reader in READERS {
                assert_eq!(
                    predict_run(&record(reader, false, frames, pixels), true, CR3),
                    None
                );
            }
        }
        for reader in READERS {
            for align_gpu in [false, true] {
                let mut rec = record(reader, true, 200, u64::MAX);
                rec.align_gpu = align_gpu;
                let p = predict_run(&rec, true, CR3).unwrap();
                assert!(p.total_seconds.is_finite() && p.total_seconds >= 0.0);
            }
        }
    }

    #[test]
    fn fuse_tile_cost() {
        // (gpu, frames, read px, edge, seconds)
        let rows = [
            (false, 30usize, 600_000u64, true, 9.801_081_504),
            (false, 30, 600_000, false, 9.442_110_468),
            (false, 8, 40_000, true, 0.263_585_351),
            (true, 30, 600_000, true, 5.251_962_492),
            (true, 30, 600_000, false, 4.475_466_530),
            (true, 8, 40_000, true, 0.286_629_439),
        ];
        for (gpu, f, read, edge, want) in rows {
            let got = super::fuse_tile_cost(gpu, f, read, edge);
            assert!(
                rel_close(got, want, 1e-6),
                "gpu={gpu} F={f} read={read} edge={edge}: {got} vs {want}"
            );
        }
        for (gpu, q) in [
            (false, 0.011_965_701_220_045_864),
            (true, 0.025_883_198_735_390_182),
        ] {
            let interior = super::fuse_tile_cost(gpu, 30, 600_000, false);
            let step = super::fuse_tile_cost(gpu, 30, 600_000, true) - interior;
            assert!(
                rel_close(step, 30.0 * q, 1e-9),
                "gpu={gpu} edge step {step}"
            );
            assert!(super::fuse_tile_cost(gpu, 30, 700_000, false) > interior);
            assert!(super::fuse_tile_cost(gpu, 31, 600_000, false) > interior);
        }
    }

    #[test]
    fn run_prediction_calibrated_classes() {
        let cr3 = |ext: &'static str, store| {
            Some(RawSource {
                extension: ext,
                store,
            })
        };
        let mem = DevelopStore::Memory;
        let with = |reader, algorithm: &str| {
            let mut r = record(reader, false, 25, 17_258_496);
            r.algorithm = algorithm.to_string();
            r
        };
        let flag = |rec: &RunPlanFeatures, raw| predict_run(rec, true, raw).unwrap().calibrated;
        let on_align = |mut rec: RunPlanFeatures, align_gpu| {
            rec.align_gpu = align_gpu;
            rec
        };

        let scratch = DevelopStore::ScratchFile;
        for (reader, algorithm, raw) in [
            (ReaderKind::Mmap, "laplacian", None),
            (ReaderKind::Decoded, "laplacian", None),
            (ReaderKind::Jpeg, "laplacian", None),
            (ReaderKind::Mmap, "depth-map", None),
            (ReaderKind::Mmap, "weighted-average", None),
            (ReaderKind::Raw, "laplacian", cr3("cr3", mem)),
            (ReaderKind::Raw, "laplacian", cr3("CR3", mem)),
            (ReaderKind::Raw, "laplacian", cr3("cr3", scratch)),
            (ReaderKind::Raw, "laplacian", cr3("CR3", scratch)),
            // `raw` is ignored for a non-RAW reader.
            (ReaderKind::Mmap, "laplacian", cr3("dng", scratch)),
        ] {
            for align_gpu in [false, true] {
                assert!(
                    flag(&on_align(with(reader, algorithm), align_gpu), raw),
                    "{reader:?} {algorithm} align_gpu {align_gpu} {:?}",
                    raw.map(|r| (r.extension, r.store))
                );
            }
        }

        let uncalibrated = [
            (with(ReaderKind::Decoded, "depth-map"), None),
            (with(ReaderKind::Jpeg, "depth-map"), None),
            (with(ReaderKind::Mmap, "no-such-algorithm"), None),
            (with(ReaderKind::Mmap, "dummy-stack"), None),
            (with(ReaderKind::Raw, "laplacian"), cr3("dng", mem)),
            (with(ReaderKind::Raw, "laplacian"), cr3("dng", scratch)),
            (with(ReaderKind::Raw, "laplacian"), cr3("raf", mem)),
            (with(ReaderKind::Raw, "laplacian"), cr3("rw2", mem)),
            (with(ReaderKind::Raw, "laplacian"), None),
            (with(ReaderKind::Raw, "depth-map"), cr3("cr3", mem)),
            (with(ReaderKind::Raw, "depth-map"), cr3("cr3", scratch)),
        ];
        for ((rec, raw), align_gpu) in uncalibrated
            .iter()
            .flat_map(|row| [(row, false), (row, true)])
        {
            let at = format!(
                "{:?} {} {:?} align_gpu {align_gpu}",
                rec.reader,
                rec.algorithm,
                raw.map(|r| r.extension)
            );
            let p = predict_run(&on_align(rec.clone(), align_gpu), true, *raw).unwrap();
            assert!(!p.calibrated, "{at}");
            let family = if rec.reader == ReaderKind::Raw {
                ReaderKind::Raw
            } else {
                ReaderKind::Mmap
            };
            let base = predict_run(&with(family, "laplacian"), true, cr3("cr3", mem)).unwrap();
            assert_eq!(p.stages, base.stages, "{at}");
        }

        // The two rows measured first stay first, which pins each family's fallback.
        let key = |row: &Calibration| (row.reader, row.raw_extension, row.raw_store, row.algorithm);
        assert_eq!(
            key(&CALIBRATIONS[0]),
            (ReaderKind::Mmap, None, None, "laplacian")
        );
        assert_eq!(
            key(&CALIBRATIONS[1]),
            (ReaderKind::Raw, Some("cr3"), Some(mem), "laplacian")
        );

        // Only the blend rows are keyed by align backend, and the two keys
        // of each blend are both present.
        for row in CALIBRATIONS {
            assert_eq!(
                row.align_gpu.is_some(),
                row.algorithm != "laplacian",
                "{}",
                row.algorithm
            );
        }
        for algorithm in ["depth-map", "weighted-average"] {
            for key in [false, true] {
                let rows = CALIBRATIONS
                    .iter()
                    .filter(|r| r.algorithm == algorithm && r.align_gpu == Some(key));
                assert_eq!(rows.count(), 1, "{algorithm} {key}");
            }
        }

        // A row without an align key predicts the same for either backend.
        for row in CALIBRATIONS.iter().filter(|r| r.align_gpu.is_none()) {
            let mut on = with(row.reader, row.algorithm);
            on.align_gpu = true;
            let mut off = on.clone();
            off.align_gpu = false;
            let raw = row
                .raw_extension
                .zip(row.raw_store)
                .map(|(extension, store)| RawSource { extension, store });
            assert_eq!(
                predict_run(&on, true, raw).unwrap().stages,
                predict_run(&off, true, raw).unwrap().stages,
                "{}",
                row.algorithm
            );
        }

        // A CPU-only blend is never planned on the GPU, so both costs agree.
        for row in CALIBRATIONS.iter().filter(|r| r.algorithm != "laplacian") {
            assert_eq!(row.gpu, row.cpu, "{}", row.algorithm);
            let mut on = with(row.reader, row.algorithm);
            on.align_gpu = row
                .align_gpu
                .expect("a blend row is keyed by align backend");
            on.gpu = true;
            let mut off = on.clone();
            off.gpu = false;
            assert_eq!(
                predict_run(&on, true, None).unwrap().stages,
                predict_run(&off, true, None).unwrap().stages
            );
        }

        // An appended row flips only its own class.
        let mut table = CALIBRATIONS.to_vec();
        let mut row = table[0];
        row.reader = ReaderKind::Decoded;
        row.algorithm = "depth-map";
        row.cpu.fuse = 9.0;
        table.push(row);
        let decoded_depth_map = with(ReaderKind::Decoded, "depth-map");
        let (hit, calibrated) = calibration(&table, &decoded_depth_map, None);
        assert!(calibrated && hit.cpu.fuse == 9.0);
        assert!(!calibration(CALIBRATIONS, &decoded_depth_map, None).1);
        for (rec, raw) in uncalibrated
            .iter()
            .filter(|(rec, _)| rec.algorithm != "depth-map" || rec.reader != ReaderKind::Decoded)
        {
            assert!(!calibration(&table, rec, *raw).1);
        }
        for (rec, raw) in [
            (with(ReaderKind::Mmap, "laplacian"), None),
            (with(ReaderKind::Raw, "laplacian"), cr3("cr3", mem)),
        ] {
            let (a, fa) = calibration(&table, &rec, raw);
            let (b, fb) = calibration(CALIBRATIONS, &rec, raw);
            assert!(fa && fb && a == b);
        }
    }
}
