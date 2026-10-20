// SPDX-License-Identifier: GPL-3.0-or-later

//! Export format selection and per-format options: the value the CLI's
//! `export` command and the GUI's export modal both build, and hand to
//! [`export_checkpoint`].

use std::cell::RefCell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::mpsc;

use chrono::Local;
use jpeg_encoder::{Encoder, ImageBuffer, JpegColorType, SamplingFactor, rgb_to_ycbcr};
use serde::{Deserialize, Serialize};

use crate::io::{self, StripCompression, StripSpec, StripWriter};
use crate::params::{EnumVariant, ParamDesc, ParamKind, ParamValue, ParamValues, validate};

use super::Project;
use super::error::ProjectError;
use super::render::{CheckpointImage, PendingState, Renderer, write_tiff, write_via_partial};
use super::seal::HeldCheckpoint;

/// The largest side JPEG can encode: SOF0's width/height fields are 16 bits.
const JPEG_MAX_SIDE: u32 = 65535;

/// The image format an export writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Tiff,
    Png,
    Jpeg,
}

impl ExportFormat {
    /// The format `path`'s extension names, or `None` for an unrecognised
    /// or absent extension.
    pub fn from_extension(path: &Path) -> Option<ExportFormat> {
        if io::has_extension(path, io::TIFF_EXTENSIONS) {
            Some(ExportFormat::Tiff)
        } else if io::has_extension(path, io::PNG_EXTENSIONS) {
            Some(ExportFormat::Png)
        } else if io::has_extension(path, io::JPEG_EXTENSIONS) {
            Some(ExportFormat::Jpeg)
        } else {
            None
        }
    }

    pub fn id(&self) -> &'static str {
        match self {
            ExportFormat::Tiff => "tiff",
            ExportFormat::Png => "png",
            ExportFormat::Jpeg => "jpeg",
        }
    }
}

/// TIFF bit depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TiffBits {
    Eight,
    Sixteen,
}

/// TIFF strip compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TiffCompression {
    None,
    Lzw,
    Deflate,
}

/// PNG bit depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PngBits {
    Eight,
    Sixteen,
}

/// PNG deflate effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PngCompression {
    Fast,
    Default,
    Best,
}

/// JPEG chroma subsampling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JpegSubsampling {
    /// 4:4:4 -- every chroma sample at full resolution.
    S444,
    /// 4:2:0 -- chroma halved in both axes.
    S420,
}

/// A format choice plus that format's resolved options -- the value
/// [`export_checkpoint`] takes and the CLI's `export --format` plus its
/// per-format flags parse into.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "lowercase")]
pub enum ExportOptions {
    Tiff {
        bits: TiffBits,
        compression: TiffCompression,
    },
    Png {
        bits: PngBits,
        compression: PngCompression,
    },
    Jpeg {
        quality: i64,
        subsampling: JpegSubsampling,
    },
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions::Tiff {
            bits: TiffBits::Sixteen,
            compression: TiffCompression::None,
        }
    }
}

pub static TIFF_PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![
        ParamDesc {
            id: "tiff-bits".to_string(),
            label: "Bit depth".to_string(),
            help: "16-bit keeps every sample of the result image; 8-bit rounds \
                   each sample through the same conversion JPEG uses."
                .to_string(),
            kind: ParamKind::Enum {
                variants: vec![
                    EnumVariant {
                        id: "16".to_string(),
                        label: "16-bit".to_string(),
                    },
                    EnumVariant {
                        id: "8".to_string(),
                        label: "8-bit".to_string(),
                    },
                ],
            },
            default: ParamValue::Enum("16".to_string()),
            advanced: false,
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "tiff-compression".to_string(),
            label: "Compression".to_string(),
            help: "None keeps every strip uncompressed; LZW and Deflate are both \
                   lossless."
                .to_string(),
            kind: ParamKind::Enum {
                variants: vec![
                    EnumVariant {
                        id: "none".to_string(),
                        label: "None".to_string(),
                    },
                    EnumVariant {
                        id: "lzw".to_string(),
                        label: "LZW".to_string(),
                    },
                    EnumVariant {
                        id: "deflate".to_string(),
                        label: "Deflate".to_string(),
                    },
                ],
            },
            default: ParamValue::Enum("none".to_string()),
            advanced: false,
            auto: None,
            host_default: None,
        },
    ]
});

pub static PNG_PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![
        ParamDesc {
            id: "png-bits".to_string(),
            label: "Bit depth".to_string(),
            help: "16-bit keeps every sample of the result image; 8-bit rounds \
                   each sample through the same conversion JPEG uses."
                .to_string(),
            kind: ParamKind::Enum {
                variants: vec![
                    EnumVariant {
                        id: "16".to_string(),
                        label: "16-bit".to_string(),
                    },
                    EnumVariant {
                        id: "8".to_string(),
                        label: "8-bit".to_string(),
                    },
                ],
            },
            default: ParamValue::Enum("16".to_string()),
            advanced: false,
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "png-compression".to_string(),
            label: "Compression".to_string(),
            help: "PNG is always lossless; this only trades encode time for file size.".to_string(),
            kind: ParamKind::Enum {
                variants: vec![
                    EnumVariant {
                        id: "fast".to_string(),
                        label: "Fast".to_string(),
                    },
                    EnumVariant {
                        id: "default".to_string(),
                        label: "Default".to_string(),
                    },
                    EnumVariant {
                        id: "best".to_string(),
                        label: "Best".to_string(),
                    },
                ],
            },
            default: ParamValue::Enum("default".to_string()),
            advanced: false,
            auto: None,
            host_default: None,
        },
    ]
});

pub static JPEG_PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![
        ParamDesc {
            id: "jpeg-quality".to_string(),
            label: "Quality".to_string(),
            help: "1 is smallest and worst; 100 is largest and best. Always 8-bit.".to_string(),
            kind: ParamKind::Int { min: 1, max: 100 },
            default: ParamValue::Int(95),
            advanced: false,
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "jpeg-subsampling".to_string(),
            label: "Chroma subsampling".to_string(),
            help: "4:4:4 keeps full-resolution colour; 4:2:0 halves it in both axes for \
                   a smaller file."
                .to_string(),
            kind: ParamKind::Enum {
                variants: vec![
                    EnumVariant {
                        id: "444".to_string(),
                        label: "4:4:4".to_string(),
                    },
                    EnumVariant {
                        id: "420".to_string(),
                        label: "4:2:0".to_string(),
                    },
                ],
            },
            default: ParamValue::Enum("420".to_string()),
            advanced: false,
            auto: None,
            host_default: None,
        },
    ]
});

/// The `ParamDesc` list a format's flags are validated and reported against.
pub fn params_for(format: ExportFormat) -> &'static [ParamDesc] {
    match format {
        ExportFormat::Tiff => &TIFF_PARAMS,
        ExportFormat::Png => &PNG_PARAMS,
        ExportFormat::Jpeg => &JPEG_PARAMS,
    }
}

/// The four metadata switches, shared across every export format.
pub static METADATA_PARAMS: LazyLock<Vec<ParamDesc>> = LazyLock::new(|| {
    vec![
        ParamDesc {
            id: "exif".to_string(),
            label: "Carry EXIF".to_string(),
            help: "Carries the alignment reference frame's Make, Model, exposure fields and \
                   capture time into the export, with the export's own size, tool and moment \
                   written in."
                .to_string(),
            kind: ParamKind::Bool,
            default: ParamValue::Bool(true),
            advanced: false,
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "icc".to_string(),
            label: "Embed ICC profile".to_string(),
            help: "Embeds the source frame's own colour profile, or a built-in sRGB profile \
                   for a RAW source."
                .to_string(),
            kind: ParamKind::Bool,
            default: ParamValue::Bool(true),
            advanced: false,
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "xmp".to_string(),
            label: "Carry XMP".to_string(),
            help: "Carries the source frame's XMP packet verbatim. Off by default: the \
                   reference frames' XMP is the source tool's own edit history, which does not \
                   describe the fused result."
                .to_string(),
            kind: ParamKind::Bool,
            default: ParamValue::Bool(false),
            advanced: false,
            auto: None,
            host_default: None,
        },
        ParamDesc {
            id: "iptc".to_string(),
            label: "Carry IPTC".to_string(),
            help: "Carries the source frame's IPTC block verbatim. Not written to PNG, which \
                   has no IPTC chunk."
                .to_string(),
            kind: ParamKind::Bool,
            default: ParamValue::Bool(false),
            advanced: false,
            auto: None,
            host_default: None,
        },
    ]
});

/// The four metadata switches [`export_checkpoint`] takes beside an
/// [`ExportOptions`]: `exif` and `icc` default on, `xmp` and `iptc` default
/// off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataOptions {
    pub exif: bool,
    pub icc: bool,
    pub xmp: bool,
    pub iptc: bool,
}

impl Default for MetadataOptions {
    fn default() -> Self {
        MetadataOptions {
            exif: true,
            icc: true,
            xmp: false,
            iptc: false,
        }
    }
}

impl MetadataOptions {
    /// Resolves [`METADATA_PARAMS`]' defaults against `values`, validating
    /// every entry the same way [`ExportOptions::from_values`] does.
    pub fn from_values(values: &ParamValues) -> Result<Self, ProjectError> {
        validate(&METADATA_PARAMS, values)
            .map_err(|source| ProjectError::ExportOptions { source })?;
        let resolved = |id: &str| -> bool {
            values
                .get(id)
                .and_then(ParamValue::as_bool)
                .unwrap_or_else(|| {
                    METADATA_PARAMS
                        .iter()
                        .find(|d| d.id == id)
                        .and_then(|d| d.default.as_bool())
                        .expect("id in schema with a bool default")
                })
        };
        Ok(MetadataOptions {
            exif: resolved("exif"),
            icc: resolved("icc"),
            xmp: resolved("xmp"),
            iptc: resolved("iptc"),
        })
    }

    /// The inverse of [`Self::from_values`].
    pub fn values(&self) -> ParamValues {
        let mut out = ParamValues::new();
        out.insert("exif".to_string(), ParamValue::Bool(self.exif));
        out.insert("icc".to_string(), ParamValue::Bool(self.icc));
        out.insert("xmp".to_string(), ParamValue::Bool(self.xmp));
        out.insert("iptc".to_string(), ParamValue::Bool(self.iptc));
        out
    }
}

/// One metadata block's fate in an export, decided in this priority: a
/// switched-off block is always `Off`; a combination its target format
/// cannot hold (IPTC into PNG) is `Unsupported`; a block the source never
/// had is `Absent`; otherwise `Written`, or `Skipped` when the resolved
/// bytes did not fit the format's container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MetadataStatus {
    #[default]
    Off,
    Unsupported,
    Absent,
    Written,
    Skipped,
}

/// Where the embedded ICC profile came from, mirroring [`MetadataStatus`]
/// for the one block with more than one non-`Written` source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IccStatus {
    #[default]
    Off,
    Source,
    BuiltinSrgb,
    None,
    Skipped,
}

/// [`export_checkpoint`]'s companion result: which source frame the EXIF
/// and ICC came from, and every metadata block's outcome.
#[derive(Debug, Clone)]
pub struct MetadataOutcome {
    pub frame: usize,
    pub frame_path: PathBuf,
    pub exif: MetadataStatus,
    pub icc: IccStatus,
    pub xmp: MetadataStatus,
    pub iptc: MetadataStatus,
    pub warnings: Vec<String>,
}

/// A format writer's own tally, before [`export_checkpoint`] adds the
/// source read's warnings and the resolved frame.
#[derive(Debug, Clone, Default)]
struct MetadataBlocks {
    exif: MetadataStatus,
    icc: IccStatus,
    xmp: MetadataStatus,
    iptc: MetadataStatus,
    warnings: Vec<String>,
}

fn block_status(on: bool, supported: bool, has_data: bool, skipped: bool) -> MetadataStatus {
    if !on {
        MetadataStatus::Off
    } else if !supported {
        MetadataStatus::Unsupported
    } else if !has_data {
        MetadataStatus::Absent
    } else if skipped {
        MetadataStatus::Skipped
    } else {
        MetadataStatus::Written
    }
}

fn icc_status(on: bool, origin: io::IccOrigin, skipped: bool) -> IccStatus {
    if !on {
        return IccStatus::Off;
    }
    match origin {
        io::IccOrigin::Off => IccStatus::Off,
        io::IccOrigin::None => IccStatus::None,
        io::IccOrigin::Source if skipped => IccStatus::Skipped,
        io::IccOrigin::Source => IccStatus::Source,
        io::IccOrigin::BuiltinSrgb if skipped => IccStatus::Skipped,
        io::IccOrigin::BuiltinSrgb => IccStatus::BuiltinSrgb,
    }
}

/// Nothing about `plan` needs writing at all: every block is either
/// switched off or the source had nothing for it. The only case this
/// unlocks is the TIFF 16-bit/uncompressed default staying on the
/// metadata-free [`write_tiff`] path.
fn plan_is_empty(plan: &io::MetadataPlan) -> bool {
    !plan.has_exif && plan.icc_bytes.is_none() && !plan.has_xmp && !plan.has_iptc
}

impl ExportOptions {
    pub fn format(&self) -> ExportFormat {
        match self {
            ExportOptions::Tiff { .. } => ExportFormat::Tiff,
            ExportOptions::Png { .. } => ExportFormat::Png,
            ExportOptions::Jpeg { .. } => ExportFormat::Jpeg,
        }
    }

    /// Resolves `format`'s descriptor defaults against `values` (an
    /// explicit id wins over its default), validating every entry against
    /// `format`'s own schema -- an id belonging to another format, an
    /// out-of-range value or an unknown enum variant is refused by name.
    pub fn from_values(format: ExportFormat, values: &ParamValues) -> Result<Self, ProjectError> {
        let schema = params_for(format);
        validate(schema, values).map_err(|source| ProjectError::ExportOptions { source })?;
        let resolved = |id: &str| -> &ParamValue {
            values.get(id).unwrap_or_else(|| {
                &schema
                    .iter()
                    .find(|d| d.id == id)
                    .expect("id in schema")
                    .default
            })
        };
        Ok(match format {
            ExportFormat::Tiff => ExportOptions::Tiff {
                bits: match resolved("tiff-bits").as_enum() {
                    Some("8") => TiffBits::Eight,
                    _ => TiffBits::Sixteen,
                },
                compression: match resolved("tiff-compression").as_enum() {
                    Some("lzw") => TiffCompression::Lzw,
                    Some("deflate") => TiffCompression::Deflate,
                    _ => TiffCompression::None,
                },
            },
            ExportFormat::Png => ExportOptions::Png {
                bits: match resolved("png-bits").as_enum() {
                    Some("8") => PngBits::Eight,
                    _ => PngBits::Sixteen,
                },
                compression: match resolved("png-compression").as_enum() {
                    Some("fast") => PngCompression::Fast,
                    Some("best") => PngCompression::Best,
                    _ => PngCompression::Default,
                },
            },
            ExportFormat::Jpeg => ExportOptions::Jpeg {
                quality: resolved("jpeg-quality").as_i64().unwrap_or(95),
                subsampling: match resolved("jpeg-subsampling").as_enum() {
                    Some("420") => JpegSubsampling::S420,
                    _ => JpegSubsampling::S444,
                },
            },
        })
    }

    /// The inverse of [`Self::from_values`]: every descriptor id of this
    /// option's format, with its resolved value.
    pub fn values(&self) -> ParamValues {
        let mut out = ParamValues::new();
        match self {
            ExportOptions::Tiff { bits, compression } => {
                out.insert(
                    "tiff-bits".to_string(),
                    ParamValue::Enum(
                        match bits {
                            TiffBits::Eight => "8",
                            TiffBits::Sixteen => "16",
                        }
                        .to_string(),
                    ),
                );
                out.insert(
                    "tiff-compression".to_string(),
                    ParamValue::Enum(
                        match compression {
                            TiffCompression::None => "none",
                            TiffCompression::Lzw => "lzw",
                            TiffCompression::Deflate => "deflate",
                        }
                        .to_string(),
                    ),
                );
            }
            ExportOptions::Png { bits, compression } => {
                out.insert(
                    "png-bits".to_string(),
                    ParamValue::Enum(
                        match bits {
                            PngBits::Eight => "8",
                            PngBits::Sixteen => "16",
                        }
                        .to_string(),
                    ),
                );
                out.insert(
                    "png-compression".to_string(),
                    ParamValue::Enum(
                        match compression {
                            PngCompression::Fast => "fast",
                            PngCompression::Default => "default",
                            PngCompression::Best => "best",
                        }
                        .to_string(),
                    ),
                );
            }
            ExportOptions::Jpeg {
                quality,
                subsampling,
            } => {
                out.insert("jpeg-quality".to_string(), ParamValue::Int(*quality));
                out.insert(
                    "jpeg-subsampling".to_string(),
                    ParamValue::Enum(
                        match subsampling {
                            JpegSubsampling::S444 => "444",
                            JpegSubsampling::S420 => "420",
                        }
                        .to_string(),
                    ),
                );
            }
        }
        out
    }
}

/// The 16-to-8-bit conversion every 8-bit TIFF and every JPEG sample goes
/// through: plain rounding, no dither. `0 -> 0`, `65535 -> 255`, and every
/// result stays within 1 of `round(v * 255 / 65535)`.
pub fn to_8bit(v: u16) -> u8 {
    ((v as u32 * 255 + 32767) / 65535) as u8
}

/// Refuses a JPEG export wider or taller than [`JPEG_MAX_SIDE`], before any
/// pixel is rendered. Every other format has no such limit.
fn check_jpeg_size(
    options: &ExportOptions,
    width: u32,
    height: u32,
    path: &Path,
) -> Result<(), ProjectError> {
    if matches!(options, ExportOptions::Jpeg { .. })
        && (width > JPEG_MAX_SIDE || height > JPEG_MAX_SIDE)
    {
        return Err(ProjectError::ExportTooLarge {
            path: path.to_path_buf(),
            format: ExportFormat::Jpeg.id().to_string(),
            width,
            height,
            limit: JPEG_MAX_SIDE,
        });
    }
    Ok(())
}

/// Refuses a PNG or JPEG export whose image is neither greyscale (1 sample
/// per pixel) nor RGB (3), before any pixel is rendered. TIFF has no such
/// limit: [`StripWriter`] and [`write_tiff`] both handle arbitrary spp.
fn check_supported_samples(
    options: &ExportOptions,
    samples_per_pixel: u32,
    path: &Path,
) -> Result<(), ProjectError> {
    let format = options.format();
    if matches!(format, ExportFormat::Png | ExportFormat::Jpeg)
        && !matches!(samples_per_pixel, 1 | 3)
    {
        return Err(ProjectError::ExportUnsupportedSamples {
            path: path.to_path_buf(),
            format: format.id().to_string(),
            samples_per_pixel,
        });
    }
    Ok(())
}

/// Validates `options` against its own schema (so a value built directly,
/// not through [`ExportOptions::from_values`], is refused the same way),
/// then renders `id` through the one render ([`CheckpointImage::render`]) and
/// writes it as `options` say: a `.partial` sibling of `path`, renamed onto it
/// only once every band is written, removed on any error. `metadata_options`
/// resolves the reference frame's EXIF, ICC, XMP and IPTC into the file; a
/// damaged or missing block is named in the returned [`MetadataOutcome`],
/// never fatal. An unedited checkpoint reads no source frame for pixels.
pub fn export_checkpoint(
    project: &mut Project,
    id: &str,
    path: &Path,
    options: &ExportOptions,
    metadata_options: &MetadataOptions,
) -> Result<MetadataOutcome, ProjectError> {
    let image = CheckpointImage::prepare(project, id)?;
    let record = project.fuse_state(id)?;
    let frame = record.align.as_ref().map_or(0, |a| a.reference);
    let frame_path = project.source_path(&record.sources[frame]);
    encode(
        &image,
        &mut |sink| image.render(Some(&mut *project), image.full(), sink),
        frame,
        &frame_path,
        path,
        options,
        metadata_options,
    )
}

/// [`export_checkpoint`] for a held result, not yet in any project: streams
/// its composite TIFF, so it needs no project; the reference frame is read
/// only for its metadata.
pub fn export_held(
    held: &HeldCheckpoint,
    path: &Path,
    options: &ExportOptions,
    metadata_options: &MetadataOptions,
) -> Result<MetadataOutcome, ProjectError> {
    let image = CheckpointImage::prepare_held(held)?;
    let frame = held.align.as_ref().map_or(0, |a| a.reference);
    encode(
        &image,
        &mut |sink| image.render(None, image.full(), sink),
        frame,
        &held.sources[frame],
        path,
        options,
        metadata_options,
    )
}

/// [`export_checkpoint`] for a [`PendingState`], strokes not yet saved: the
/// same render, validation, metadata resolution and `.partial` discipline
/// through the one shared `encode` path, with no project read or written.
/// A missing source is [`ProjectError::ExportSourcesUnavailable`].
pub fn export_pending(
    state: &PendingState,
    path: &Path,
    options: &ExportOptions,
    metadata_options: &MetadataOptions,
) -> Result<MetadataOutcome, ProjectError> {
    let image = CheckpointImage::prepare_pending(state)?;
    let frame = state.reference;
    let frame_path = state.sources[frame].clone();
    encode(
        &image,
        &mut |sink| image.render(None, image.full(), sink),
        frame,
        &frame_path,
        path,
        options,
        metadata_options,
    )
}

/// Validates `options`, writes `image` through `render` as `options` say (a
/// `.partial` sibling of `path`, renamed onto it only once complete), and
/// resolves `frame_path`'s metadata into the same file -- a damaged or
/// missing block is named in the returned [`MetadataOutcome`], never fatal.
/// Shared by [`export_checkpoint`], [`export_held`] and [`export_pending`].
fn encode(
    image: &CheckpointImage,
    render: &mut Renderer<'_>,
    frame: usize,
    frame_path: &Path,
    path: &Path,
    options: &ExportOptions,
    metadata_options: &MetadataOptions,
) -> Result<MetadataOutcome, ProjectError> {
    validate(params_for(options.format()), &options.values())
        .map_err(|source| ProjectError::ExportOptions { source })?;
    check_jpeg_size(options, image.width, image.height, path)?;
    check_supported_samples(options, image.samples_per_pixel, path)?;

    let source = io::read_source_metadata(frame_path);
    let now = Local::now().fixed_offset();
    let plan = io::resolve_plan(
        &source,
        image.width,
        image.height,
        now,
        metadata_options.exif,
        metadata_options.icc,
        metadata_options.xmp,
        metadata_options.iptc,
    );

    let blocks = match options {
        ExportOptions::Tiff {
            bits: TiffBits::Sixteen,
            compression: TiffCompression::None,
        } if plan_is_empty(&plan) => {
            write_tiff(image, render, path)?;
            MetadataBlocks {
                exif: block_status(metadata_options.exif, true, false, false),
                icc: icc_status(metadata_options.icc, plan.icc, false),
                xmp: block_status(metadata_options.xmp, true, false, false),
                iptc: block_status(metadata_options.iptc, true, false, false),
                warnings: Vec::new(),
            }
        }
        ExportOptions::Tiff { bits, compression } => write_strip_tiff(
            image,
            render,
            path,
            *bits,
            *compression,
            &plan,
            metadata_options,
        )?,
        ExportOptions::Png { bits, compression } => write_png(
            image,
            render,
            path,
            *bits,
            *compression,
            &plan,
            metadata_options,
        )?,
        ExportOptions::Jpeg {
            quality,
            subsampling,
        } => write_jpeg(
            image,
            render,
            path,
            *quality,
            *subsampling,
            &plan,
            metadata_options,
        )?,
    };

    let mut warnings = source.warnings;
    warnings.extend(blocks.warnings);
    Ok(MetadataOutcome {
        frame,
        frame_path: frame_path.to_path_buf(),
        exif: blocks.exif,
        icc: blocks.icc,
        xmp: blocks.xmp,
        iptc: blocks.iptc,
        warnings,
    })
}

/// 8- or 16-bit, compressed or uncompressed TIFF, through [`StripWriter`].
#[allow(clippy::too_many_arguments)] // one flag/plan apiece; a struct wrapper would only rename this list
fn write_strip_tiff(
    image: &CheckpointImage,
    render: &mut Renderer<'_>,
    path: &Path,
    bits: TiffBits,
    compression: TiffCompression,
    plan: &io::MetadataPlan,
    metadata_options: &MetadataOptions,
) -> Result<MetadataBlocks, ProjectError> {
    let bits_per_sample = match bits {
        TiffBits::Sixteen => 16,
        TiffBits::Eight => 8,
    };
    let compression = match compression {
        TiffCompression::None => StripCompression::None,
        TiffCompression::Lzw => StripCompression::Lzw,
        TiffCompression::Deflate => StripCompression::Deflate,
    };
    let mut ifd0 = plan.tags.ifd0.clone();
    if let Some(bytes) = &plan.icc_bytes {
        ifd0.push(io::icc_tag_entry(bytes));
    }
    if let Some(bytes) = &plan.xmp_bytes {
        ifd0.push(io::xmp_tag_entry(bytes));
    }
    if let Some(bytes) = &plan.iptc_bytes {
        ifd0.push(io::iptc_tag_entry(bytes));
    }
    let metadata = io::MetadataTags {
        ifd0,
        exif: plan.tags.exif.clone(),
        gps: plan.tags.gps.clone(),
    };

    write_via_partial(path, |partial| {
        let mut writer = StripWriter::create(
            partial,
            StripSpec {
                width: image.width,
                height: image.height,
                samples_per_pixel: image.samples_per_pixel,
                bits_per_sample,
                compression,
                description: Some(image.description.clone()),
                metadata,
            },
        )
        .map_err(|err| ProjectError::container(partial, err))?;
        let mut row_bytes: Vec<u8> = Vec::new();
        render(&mut |band, _rect| {
            row_bytes.clear();
            match bits {
                TiffBits::Sixteen => row_bytes.extend(band.iter().flat_map(|v| v.to_le_bytes())),
                TiffBits::Eight => row_bytes.extend(band.iter().map(|&v| to_8bit(v))),
            }
            writer
                .write_rows(&row_bytes)
                .map_err(|err| ProjectError::container(path, err))
        })?;
        writer
            .finish()
            .map_err(|err| ProjectError::container(path, err))
            .map(|_| ())
    })?;

    Ok(MetadataBlocks {
        exif: block_status(metadata_options.exif, true, plan.has_exif, false),
        icc: icc_status(metadata_options.icc, plan.icc, false),
        xmp: block_status(metadata_options.xmp, true, plan.has_xmp, false),
        iptc: block_status(metadata_options.iptc, true, plan.has_iptc, false),
        warnings: Vec::new(),
    })
}

/// 8- or 16-bit PNG, lossless, through the `png` crate's `StreamWriter` fed
/// band by band. 16-bit samples are written big-endian, as the PNG spec
/// requires. EXIF goes to the `eXIf` chunk, ICC to `iCCP`, both through
/// `png::Info`; XMP goes to an `iTXt` chunk, which needs valid UTF-8 text --
/// a non-UTF-8 packet is skipped. PNG has no IPTC chunk, so that switch is
/// always `Unsupported` here regardless of the source.
#[allow(clippy::too_many_arguments)] // one flag/plan apiece; a struct wrapper would only rename this list
fn write_png(
    image: &CheckpointImage,
    render: &mut Renderer<'_>,
    path: &Path,
    bits: PngBits,
    compression: PngCompression,
    plan: &io::MetadataPlan,
    metadata_options: &MetadataOptions,
) -> Result<MetadataBlocks, ProjectError> {
    let color = if image.samples_per_pixel >= 3 {
        png::ColorType::Rgb
    } else {
        png::ColorType::Grayscale
    };
    let depth = match bits {
        PngBits::Sixteen => png::BitDepth::Sixteen,
        PngBits::Eight => png::BitDepth::Eight,
    };
    let level = match compression {
        PngCompression::Fast => png::Compression::Fast,
        PngCompression::Default => png::Compression::Balanced,
        PngCompression::Best => png::Compression::High,
    };
    let exif_bytes = plan.has_exif.then(|| io::exif_blob(&plan.tags));
    let mut warnings = Vec::new();
    let mut xmp_skipped = false;
    let xmp_text =
        plan.xmp_bytes
            .as_ref()
            .and_then(|bytes| match String::from_utf8(bytes.clone()) {
                Ok(text) => Some(text),
                Err(_) => {
                    xmp_skipped = true;
                    warnings.push(
                        "XMP packet is not valid UTF-8, which PNG's iTXt chunk requires; skipped"
                            .to_string(),
                    );
                    None
                }
            });

    write_via_partial(path, |partial| {
        let file = std::fs::File::create(partial).map_err(|err| ProjectError::io(partial, err))?;
        let mut info = png::Info::with_size(image.width, image.height);
        info.color_type = color;
        info.bit_depth = depth;
        if let Some(bytes) = &plan.icc_bytes {
            info.icc_profile = Some(bytes.clone().into());
        }
        if let Some(bytes) = &exif_bytes {
            info.exif_metadata = Some(bytes.clone().into());
        }
        let mut encoder = png::Encoder::with_info(std::io::BufWriter::new(file), info)
            .map_err(|err| ProjectError::container(partial, err))?;
        encoder.set_compression(level);
        if let Some(text) = &xmp_text {
            encoder
                .add_itxt_chunk("XML:com.adobe.xmp".to_string(), text.clone())
                .map_err(|err| ProjectError::container(partial, err))?;
        }
        let mut writer = encoder
            .write_header()
            .map_err(|err| ProjectError::container(partial, err))?;
        let mut row_bytes: Vec<u8> = Vec::new();
        {
            let mut stream = writer
                .stream_writer()
                .map_err(|err| ProjectError::container(partial, err))?;
            render(&mut |band, _rect| {
                row_bytes.clear();
                match bits {
                    PngBits::Sixteen => row_bytes.extend(band.iter().flat_map(|v| v.to_be_bytes())),
                    PngBits::Eight => row_bytes.extend(band.iter().map(|&v| to_8bit(v))),
                }
                stream
                    .write_all(&row_bytes)
                    .map_err(|err| ProjectError::container(path, err))
            })?;
            stream
                .finish()
                .map_err(|err| ProjectError::container(path, err))?;
        }
        writer
            .finish()
            .map_err(|err| ProjectError::container(path, err))
    })?;

    Ok(MetadataBlocks {
        exif: block_status(metadata_options.exif, true, plan.has_exif, false),
        icc: icc_status(metadata_options.icc, plan.icc, false),
        xmp: block_status(metadata_options.xmp, true, plan.has_xmp, xmp_skipped),
        iptc: block_status(metadata_options.iptc, false, plan.has_iptc, false),
        warnings,
    })
}

/// Which of a JPEG export's four metadata blocks the encoder refused for
/// exceeding its container's byte cap -- discovered only once the encoder
/// thread actually calls `add_exif_metadata`/`add_icc_profile`/
/// `add_app_segment`, so this is built there and read back after `join`.
#[derive(Debug, Clone, Copy, Default)]
struct JpegMetadataSkips {
    exif: bool,
    icc: bool,
    xmp: bool,
    iptc: bool,
}

/// 8-bit JPEG through `jpeg_encoder`. The calling thread converts and hands
/// each band over a rendezvous channel while a scoped worker thread encodes
/// row by row, so neither side holds the whole image resident and at most two
/// 8-bit bands are alive at once, whichever side is faster. EXIF goes
/// through `add_exif_metadata` (APP1 `Exif`), ICC through `add_icc_profile`
/// (APP2, chunked by the encoder itself), XMP through `add_app_segment`
/// (a second APP1, `http://ns.adobe.com/xap/1.0/` prefixed) and IPTC
/// through `add_app_segment` (APP13, a Photoshop `8BIM` resource) -- each
/// skipped, not fatal, when the encoder refuses it for size.
#[allow(clippy::too_many_arguments)] // one flag/plan apiece; a struct wrapper would only rename this list
fn write_jpeg(
    image: &CheckpointImage,
    render: &mut Renderer<'_>,
    path: &Path,
    quality: i64,
    subsampling: JpegSubsampling,
    plan: &io::MetadataPlan,
    metadata_options: &MetadataOptions,
) -> Result<MetadataBlocks, ProjectError> {
    let width = image.width;
    let height = image.height;
    let samples_per_pixel = image.samples_per_pixel;
    let quality = quality as u8;
    let sampling_factor = match subsampling {
        JpegSubsampling::S444 => SamplingFactor::F_1_1,
        JpegSubsampling::S420 => SamplingFactor::F_2_2,
    };
    let exif_bytes = plan.has_exif.then(|| io::exif_blob(&plan.tags));
    let icc_bytes = plan.icc_bytes.clone();
    let xmp_bytes = plan.xmp_bytes.clone();
    let iptc_bytes = plan.iptc_bytes.clone();
    let mut skips = JpegMetadataSkips::default();

    write_via_partial(path, |partial| {
        let file = std::fs::File::create(partial).map_err(|err| ProjectError::io(partial, err))?;
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(0);
        let source = JpegRowSource::new(rx, width, height, samples_per_pixel);
        let (render_result, encode_result, thread_skips) = std::thread::scope(|scope| {
            let handle = scope.spawn(move || {
                let mut encoder = Encoder::new(std::io::BufWriter::new(file), quality);
                encoder.set_sampling_factor(sampling_factor);
                let mut skips = JpegMetadataSkips::default();
                if let Some(bytes) = &exif_bytes {
                    skips.exif = encoder.add_exif_metadata(bytes).is_err();
                }
                if let Some(bytes) = &icc_bytes {
                    skips.icc = encoder.add_icc_profile(bytes).is_err();
                }
                if let Some(bytes) = &xmp_bytes {
                    let mut segment = io::XMP_APP1_HEADER.to_vec();
                    segment.extend_from_slice(bytes);
                    skips.xmp = encoder.add_app_segment(1, segment).is_err();
                }
                if let Some(bytes) = &iptc_bytes {
                    skips.iptc = encoder
                        .add_app_segment(13, io::photoshop_iptc_segment(bytes))
                        .is_err();
                }
                (encoder.encode_image(source), skips)
            });
            let render_result = render(&mut |band, _rect| {
                let bytes: Vec<u8> = band.iter().map(|&v| to_8bit(v)).collect();
                tx.send(bytes).map_err(|_| {
                    ProjectError::container(
                        path,
                        "the JPEG encoder thread closed its channel early",
                    )
                })
            });
            drop(tx);
            let (encode_result, thread_skips) = match handle.join() {
                Ok((result, skips)) => (
                    result.map_err(|err| ProjectError::container(partial, err)),
                    skips,
                ),
                Err(_) => (
                    Err(ProjectError::container(
                        partial,
                        "the JPEG encoder thread panicked",
                    )),
                    JpegMetadataSkips::default(),
                ),
            };
            (render_result, encode_result, thread_skips)
        });
        skips = thread_skips;
        render_result.and(encode_result)
    })?;

    let mut warnings = Vec::new();
    if skips.exif {
        warnings.push("EXIF blob exceeds JPEG's APP1 segment limit; skipped".to_string());
    }
    if skips.icc {
        warnings.push("ICC profile exceeds JPEG's embedding limit; skipped".to_string());
    }
    if skips.xmp {
        warnings
            .push("XMP packet exceeds one JPEG APP1 segment (65502 bytes); skipped".to_string());
    }
    if skips.iptc {
        warnings.push("IPTC block exceeds JPEG's APP13 segment limit; skipped".to_string());
    }

    Ok(MetadataBlocks {
        exif: block_status(metadata_options.exif, true, plan.has_exif, skips.exif),
        icc: icc_status(metadata_options.icc, plan.icc, skips.icc),
        xmp: block_status(metadata_options.xmp, true, plan.has_xmp, skips.xmp),
        iptc: block_status(metadata_options.iptc, true, plan.has_iptc, skips.iptc),
        warnings,
    })
}

/// Bridges the render thread's 8-bit bands to `jpeg_encoder`'s row-at-a-time
/// pull: every row up to `height` is requested once, in non-decreasing
/// order, except the last row, which is re-served to pad an incomplete MCU.
/// `fill_buffers` takes `&self`, so the receiver and the current band sit
/// behind a `RefCell`.
struct JpegRowSource {
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    state: RefCell<JpegRowState>,
}

struct JpegRowState {
    rx: mpsc::Receiver<Vec<u8>>,
    closed: bool,
    band: Vec<u8>,
    band_start_row: u32,
    rows_in_band: u32,
}

impl JpegRowSource {
    fn new(rx: mpsc::Receiver<Vec<u8>>, width: u32, height: u32, samples_per_pixel: u32) -> Self {
        JpegRowSource {
            width,
            height,
            samples_per_pixel,
            state: RefCell::new(JpegRowState {
                rx,
                closed: false,
                band: Vec::new(),
                band_start_row: 0,
                rows_in_band: 0,
            }),
        }
    }

    fn row_bytes(&self) -> usize {
        (self.width * self.samples_per_pixel) as usize
    }
}

/// Pushes one 8-bit row into `jpeg_encoder`'s per-component buffers: RGB
/// converts through [`rgb_to_ycbcr`], everything else (luma) copies as is.
fn push_row(row: &[u8], samples_per_pixel: u32, buffers: &mut [Vec<u8>; 4]) {
    if samples_per_pixel >= 3 {
        for chunk in row.chunks_exact(3) {
            let (y, cb, cr) = rgb_to_ycbcr(chunk[0], chunk[1], chunk[2]);
            buffers[0].push(y);
            buffers[1].push(cb);
            buffers[2].push(cr);
        }
    } else {
        buffers[0].extend_from_slice(row);
    }
}

impl ImageBuffer for JpegRowSource {
    fn get_jpeg_color_type(&self) -> JpegColorType {
        if self.samples_per_pixel >= 3 {
            JpegColorType::Ycbcr
        } else {
            JpegColorType::Luma
        }
    }

    fn width(&self) -> u16 {
        self.width as u16
    }

    fn height(&self) -> u16 {
        self.height as u16
    }

    fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        let y = y as u32;
        let row_bytes = self.row_bytes();
        let mut state = self.state.borrow_mut();
        while !state.closed && y >= state.band_start_row + state.rows_in_band {
            // Free the consumed band first: the sender may already be
            // converting the band after the one this `recv` returns.
            state.band = Vec::new();
            match state.rx.recv() {
                Ok(band) => {
                    state.band_start_row += state.rows_in_band;
                    state.rows_in_band = (band.len() / row_bytes) as u32;
                    state.band = band;
                }
                Err(_) => state.closed = true,
            }
        }
        if state.closed && y >= state.band_start_row + state.rows_in_band {
            let zero_row = vec![0u8; row_bytes];
            push_row(&zero_row, self.samples_per_pixel, buffers);
        } else {
            let offset = ((y - state.band_start_row) as usize) * row_bytes;
            push_row(
                &state.band[offset..offset + row_bytes],
                self.samples_per_pixel,
                buffers,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_8bit_endpoints() {
        assert_eq!(to_8bit(0), 0);
        assert_eq!(to_8bit(65535), 255);
    }

    #[test]
    fn to_8bit_stays_within_one_of_the_exact_rounding_and_is_monotonic() {
        let mut prev = 0u8;
        for v in 0..=u16::MAX {
            let got = to_8bit(v);
            let exact = (v as f64) * 255.0 / 65535.0;
            let diff = (got as f64 - exact).abs();
            assert!(diff <= 1.0, "v={v} got={got} exact={exact}");
            assert!(got >= prev, "v={v} got={got} prev={prev} not monotonic");
            prev = got;
        }
    }

    fn all_defaults(schema: &[ParamDesc]) -> ParamValues {
        schema
            .iter()
            .map(|d| (d.id.clone(), d.default.clone()))
            .collect()
    }

    #[test]
    fn from_values_values_round_trips_for_every_format() {
        for format in [ExportFormat::Tiff, ExportFormat::Png, ExportFormat::Jpeg] {
            let defaults = all_defaults(params_for(format));
            let options = ExportOptions::from_values(format, &defaults).unwrap();
            assert_eq!(options.format(), format);
            assert_eq!(options.values(), defaults);
            // Round-tripping through `values()` again must be the identity.
            let again = ExportOptions::from_values(format, &options.values()).unwrap();
            assert_eq!(again, options);
        }
    }

    #[test]
    fn from_values_defaults_match_descriptor_defaults() {
        assert_eq!(
            ExportOptions::default(),
            ExportOptions::Tiff {
                bits: TiffBits::Sixteen,
                compression: TiffCompression::None,
            }
        );
        let jpeg = ExportOptions::from_values(ExportFormat::Jpeg, &ParamValues::new()).unwrap();
        assert_eq!(
            jpeg,
            ExportOptions::Jpeg {
                quality: 95,
                subsampling: JpegSubsampling::S420,
            }
        );
    }

    #[test]
    fn from_values_refuses_an_id_of_another_format() {
        let mut values = ParamValues::new();
        values.insert("jpeg-quality".to_string(), ParamValue::Int(80));
        let err = ExportOptions::from_values(ExportFormat::Tiff, &values).unwrap_err();
        assert!(matches!(err, ProjectError::ExportOptions { .. }));
        assert!(err.to_string().contains("jpeg-quality"));
    }

    #[test]
    fn from_values_refuses_out_of_range_jpeg_quality() {
        for bad in [0i64, 101] {
            let mut values = ParamValues::new();
            values.insert("jpeg-quality".to_string(), ParamValue::Int(bad));
            let err = ExportOptions::from_values(ExportFormat::Jpeg, &values).unwrap_err();
            assert!(err.to_string().contains("jpeg-quality"), "{err}");
        }
    }

    #[test]
    fn from_values_refuses_an_unknown_tiff_bits_variant() {
        let mut values = ParamValues::new();
        values.insert("tiff-bits".to_string(), ParamValue::Enum("12".to_string()));
        let err = ExportOptions::from_values(ExportFormat::Tiff, &values).unwrap_err();
        assert!(err.to_string().contains("tiff-bits"), "{err}");
    }

    #[test]
    fn jpeg_size_check_refuses_above_the_limit_with_no_pixel_rendered() {
        let jpeg = ExportOptions::Jpeg {
            quality: 95,
            subsampling: JpegSubsampling::S444,
        };
        let err = check_jpeg_size(&jpeg, JPEG_MAX_SIDE + 1, 100, Path::new("out.jpg")).unwrap_err();
        assert!(matches!(err, ProjectError::ExportTooLarge { .. }));
        assert!(err.to_string().contains(&(JPEG_MAX_SIDE + 1).to_string()));
        assert!(check_jpeg_size(&jpeg, JPEG_MAX_SIDE, JPEG_MAX_SIDE, Path::new("out.jpg")).is_ok());

        // Only JPEG has this limit: the same oversized shape is fine for TIFF.
        let tiff = ExportOptions::default();
        assert!(check_jpeg_size(&tiff, JPEG_MAX_SIDE + 1, 100, Path::new("out.tif")).is_ok());
    }

    #[test]
    fn supported_samples_check_refuses_anything_but_one_or_three_for_png_and_jpeg() {
        let png = ExportOptions::Png {
            bits: PngBits::Sixteen,
            compression: PngCompression::Default,
        };
        let jpeg = ExportOptions::Jpeg {
            quality: 95,
            subsampling: JpegSubsampling::S444,
        };
        for spp in [0u32, 2, 4] {
            for options in [&png, &jpeg] {
                let err = check_supported_samples(options, spp, Path::new("out")).unwrap_err();
                assert!(
                    matches!(err, ProjectError::ExportUnsupportedSamples { .. }),
                    "{options:?} spp={spp}: {err}"
                );
                assert!(err.to_string().contains(&spp.to_string()), "{err}");
            }
        }
        for spp in [1u32, 3] {
            assert!(check_supported_samples(&png, spp, Path::new("out")).is_ok());
            assert!(check_supported_samples(&jpeg, spp, Path::new("out")).is_ok());
        }

        // TIFF has no such limit.
        let tiff = ExportOptions::default();
        assert!(check_supported_samples(&tiff, 2, Path::new("out.tif")).is_ok());
    }

    #[test]
    fn jpeg_row_source_serves_luma_rows_from_bands_in_order() {
        let (tx, rx) = mpsc::channel();
        tx.send(vec![10, 20, 30, 40]).unwrap(); // rows 0-1, width 2
        tx.send(vec![50, 60]).unwrap(); // row 2
        drop(tx);
        let source = JpegRowSource::new(rx, 2, 3, 1);

        let mut buffers: [Vec<u8>; 4] = Default::default();
        source.fill_buffers(0, &mut buffers);
        assert_eq!(buffers[0], vec![10, 20]);

        let mut buffers: [Vec<u8>; 4] = Default::default();
        source.fill_buffers(1, &mut buffers);
        assert_eq!(buffers[0], vec![30, 40]);

        let mut buffers: [Vec<u8>; 4] = Default::default();
        source.fill_buffers(2, &mut buffers);
        assert_eq!(buffers[0], vec![50, 60]);

        // The last row is re-requested to pad an incomplete MCU; it must
        // come back without a further channel read.
        let mut buffers: [Vec<u8>; 4] = Default::default();
        source.fill_buffers(2, &mut buffers);
        assert_eq!(buffers[0], vec![50, 60]);
    }

    #[test]
    fn jpeg_row_source_converts_rgb_rows_to_ycbcr() {
        let (tx, rx) = mpsc::channel();
        tx.send(vec![255, 255, 255]).unwrap();
        drop(tx);
        let source = JpegRowSource::new(rx, 1, 1, 3);

        let mut buffers: [Vec<u8>; 4] = Default::default();
        source.fill_buffers(0, &mut buffers);
        let (y, cb, cr) = rgb_to_ycbcr(255, 255, 255);
        assert_eq!(buffers[0], vec![y]);
        assert_eq!(buffers[1], vec![cb]);
        assert_eq!(buffers[2], vec![cr]);
    }

    #[test]
    fn jpeg_row_source_fills_zero_rows_once_the_channel_closes_early() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        drop(tx); // no bands ever arrive
        let source = JpegRowSource::new(rx, 2, 2, 1);

        let mut buffers: [Vec<u8>; 4] = Default::default();
        source.fill_buffers(0, &mut buffers);
        assert_eq!(buffers[0], vec![0, 0]);

        let mut buffers: [Vec<u8>; 4] = Default::default();
        source.fill_buffers(1, &mut buffers);
        assert_eq!(buffers[0], vec![0, 0]);
    }

    #[test]
    fn jpeg_row_source_drives_a_real_encode_to_valid_jpeg_bytes() {
        let (tx, rx) = mpsc::channel();
        for _ in 0..4 {
            tx.send(vec![128u8; 4 * 3]).unwrap(); // one row, width 4, RGB
        }
        drop(tx);
        let source = JpegRowSource::new(rx, 4, 4, 3);

        let mut out: Vec<u8> = Vec::new();
        let mut encoder = Encoder::new(&mut out, 90);
        encoder.set_sampling_factor(SamplingFactor::F_1_1);
        encoder.encode_image(source).unwrap();

        assert_eq!(&out[0..2], &[0xFF, 0xD8], "missing SOI marker");
        assert_eq!(&out[out.len() - 2..], &[0xFF, 0xD9], "missing EOI marker");
    }

    #[test]
    fn extension_inference_matches_each_recognised_extension() {
        for (name, expected) in [
            ("out.tif", Some(ExportFormat::Tiff)),
            ("out.TIFF", Some(ExportFormat::Tiff)),
            ("out.png", Some(ExportFormat::Png)),
            ("out.jpg", Some(ExportFormat::Jpeg)),
            ("out.JPEG", Some(ExportFormat::Jpeg)),
            ("out.webp", None),
            ("out", None),
        ] {
            assert_eq!(
                ExportFormat::from_extension(Path::new(name)),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn metadata_options_from_values_values_round_trips_and_defaults_match_the_switches() {
        let defaults = all_defaults(&METADATA_PARAMS);
        let options = MetadataOptions::from_values(&defaults).unwrap();
        assert_eq!(options, MetadataOptions::default());
        assert_eq!(options.values(), defaults);
        assert!(options.exif && options.icc && !options.xmp && !options.iptc);
    }

    #[test]
    fn block_status_precedence_is_off_then_unsupported_then_absent_then_written_or_skipped() {
        assert_eq!(block_status(false, true, true, false), MetadataStatus::Off);
        assert_eq!(
            block_status(true, false, true, false),
            MetadataStatus::Unsupported
        );
        assert_eq!(
            block_status(true, true, false, false),
            MetadataStatus::Absent
        );
        assert_eq!(
            block_status(true, true, true, false),
            MetadataStatus::Written
        );
        assert_eq!(
            block_status(true, true, true, true),
            MetadataStatus::Skipped
        );
    }

    #[test]
    fn icc_status_reports_off_then_origin_or_skipped() {
        assert_eq!(
            icc_status(false, io::IccOrigin::Source, false),
            IccStatus::Off
        );
        assert_eq!(
            icc_status(true, io::IccOrigin::None, false),
            IccStatus::None
        );
        assert_eq!(
            icc_status(true, io::IccOrigin::Source, false),
            IccStatus::Source
        );
        assert_eq!(
            icc_status(true, io::IccOrigin::BuiltinSrgb, false),
            IccStatus::BuiltinSrgb
        );
        assert_eq!(
            icc_status(true, io::IccOrigin::Source, true),
            IccStatus::Skipped
        );
    }
}
