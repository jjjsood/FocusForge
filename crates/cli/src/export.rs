// SPDX-License-Identifier: GPL-3.0-or-later

//! `focusforge-cli export`: a project in, an image file out.
//!
//! This is the **only** thing in the workspace that writes a TIFF from a
//! stack result. `stack` prepares projects for review; turning one into an
//! image is a separate, explicit step.
//!
//! Two properties this module exists to keep:
//!
//! - **The stored image is exported.** `-o` writes the checkpoint's own
//!   image, the one the viewer shows, encoded as `--format` and its
//!   per-format flags say. An unedited checkpoint reads no source frame; an
//!   edited one re-reads and verifies its fused ancestor's frames. The
//!   default (`--format tiff --tiff-bits 16 --tiff-compression none`, which
//!   an extension-less or `.tif`/`.tiff` `-o` also selects) writes a
//!   twelve-tag uncompressed IFD.
//! - **A warning, never a silence.** A project whose sources cannot be
//!   resolved relatively names every one of them on stderr and still writes
//!   the image. `core` returns them as data; printing them is this
//!   module's.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::Args;
use focusforge_core::algorithm::TileReader;
use focusforge_core::align::WarpingReader;
use focusforge_core::equalize::GainReader;
use focusforge_core::host;
use focusforge_core::io::{Rect, TiffSpec, TiffWriter, open_frame_set};
use focusforge_core::params::{ParamValue, ParamValues};
use focusforge_core::project::export::{
    ExportFormat, ExportOptions, IccStatus, METADATA_PARAMS, MetadataOptions, MetadataOutcome,
    MetadataStatus, params_for,
};
use focusforge_core::project::{self, FuseRecord, Project};
use focusforge_core::tile::HostBudget;
use focusforge_diagnostics::MIRRORED_OUTPUT_TARGET;
use serde::Serialize;

use crate::retouch::would_replace;

/// Rows per streamed band, `export_frame`'s own band-wise write.
const FRAME_BAND_ROWS: u32 = 256;

/// `--format` and every per-format flag `export` accepts. Flattened into
/// `Command::Export`; each field is `Option<String>` so an absent flag
/// falls back to its format's own `ParamDesc` default rather than clap's.
#[derive(Args, Default)]
pub struct FormatArgs {
    /// The image format to write: `tiff`, `png`, `jpeg` or `dng` (a linear
    /// DNG of a RAW project). Inferred from `-o`'s extension when omitted
    /// (`.tif`/`.tiff` -> tiff, `.png` -> png, `.jpg`/`.jpeg` -> jpeg,
    /// `.dng` -> dng, no extension -> tiff, the default); refused if it
    /// names a format other than a recognised extension's.
    #[arg(long)]
    format: Option<String>,
    /// TIFF bit depth: `16` (default) or `8`. TIFF only.
    #[arg(long = "tiff-bits", value_name = "BITS")]
    tiff_bits: Option<String>,
    /// TIFF strip compression: `none` (default), `lzw` or `deflate`. TIFF
    /// only.
    #[arg(long = "tiff-compression", value_name = "NAME")]
    tiff_compression: Option<String>,
    /// PNG bit depth: `16` (default) or `8`. PNG only.
    #[arg(long = "png-bits", value_name = "BITS")]
    png_bits: Option<String>,
    /// PNG compression effort: `fast`, `default` (default) or `best`. PNG
    /// only.
    #[arg(long = "png-compression", value_name = "NAME")]
    png_compression: Option<String>,
    /// JPEG quality, 1-100, default 95. JPEG only.
    #[arg(long = "jpeg-quality", value_name = "N")]
    jpeg_quality: Option<String>,
    /// JPEG chroma subsampling: `420` (default) or `444`. JPEG only.
    #[arg(long = "jpeg-subsampling", value_name = "SPEC")]
    jpeg_subsampling: Option<String>,
    /// Carry the alignment reference frame's EXIF: `on` (default) or `off`.
    #[arg(long)]
    exif: Option<String>,
    /// Embed an ICC profile: `on` (default) or `off`. Has no effect on a DNG,
    /// whose colour is defined by its camera matrices.
    #[arg(long)]
    icc: Option<String>,
    /// Carry the source frame's XMP packet verbatim: `on` or `off`
    /// (default).
    #[arg(long)]
    xmp: Option<String>,
    /// Carry the source frame's IPTC block verbatim: `on` or `off`
    /// (default). Never written to PNG.
    #[arg(long)]
    iptc: Option<String>,
}

/// Every `FormatArgs` flag, paired with the `ParamDesc` id it fills, in
/// declaration order.
const FORMAT_FLAG_IDS: [&str; 6] = [
    "tiff-bits",
    "tiff-compression",
    "png-bits",
    "png-compression",
    "jpeg-quality",
    "jpeg-subsampling",
];

/// Resolves the format to encode as: `--format` when given, refused if it
/// names a format other than a recognised extension's; otherwise the
/// extension `-o` names; otherwise TIFF -- the default an extension-less
/// path, or an unrecognised extension paired with an explicit `--format`,
/// keeps.
fn resolve_format(output: &Path, explicit: Option<&str>) -> Result<ExportFormat, String> {
    let explicit = explicit
        .map(|text| match text {
            "tiff" => Ok(ExportFormat::Tiff),
            "png" => Ok(ExportFormat::Png),
            "jpeg" => Ok(ExportFormat::Jpeg),
            "dng" => Ok(ExportFormat::Dng),
            other => Err(format!(
                "--format: unknown format {other:?}, expected tiff, png, jpeg or dng"
            )),
        })
        .transpose()?;
    let inferred = ExportFormat::from_extension(output);
    match (explicit, inferred) {
        (Some(fmt), Some(ext_fmt)) if fmt != ext_fmt => Err(format!(
            "--format {} conflicts with {}'s extension, which names {}",
            fmt.id(),
            output.display(),
            ext_fmt.id()
        )),
        (Some(fmt), _) => Ok(fmt),
        (None, Some(ext_fmt)) => Ok(ext_fmt),
        (None, None) if output.extension().is_some() => Err(format!(
            "--format: {} has an unrecognised extension; pass --format tiff, png, jpeg or dng",
            output.display()
        )),
        (None, None) => Ok(ExportFormat::Tiff),
    }
}

/// Every metadata switch, paired with the `METADATA_PARAMS` id it fills, in
/// declaration order.
const METADATA_FLAG_IDS: [&str; 4] = ["exif", "icc", "xmp", "iptc"];

/// Maps `on`/`off` onto the `true`/`false` text `ParamValue::parse` accepts
/// for a `Bool` descriptor, refusing any other word by the flag's own name.
fn on_off(id: &str, text: &str) -> Result<&'static str, String> {
    match text {
        "on" => Ok("true"),
        "off" => Ok("false"),
        other => Err(format!("--{id}: expected on or off, got {other:?}")),
    }
}

/// Resolves `args`' four metadata flags into a validated `MetadataOptions`,
/// the same `on|off`-through-`ParamValue::parse` shape
/// `resolve_export_options` uses for its own flags.
fn resolve_metadata_options(args: &FormatArgs) -> Result<MetadataOptions, String> {
    let given: [Option<&str>; 4] = [
        args.exif.as_deref(),
        args.icc.as_deref(),
        args.xmp.as_deref(),
        args.iptc.as_deref(),
    ];
    let mut values = ParamValues::new();
    for (id, text) in METADATA_FLAG_IDS.into_iter().zip(given) {
        let Some(text) = text else { continue };
        let mapped = on_off(id, text)?;
        let desc = METADATA_PARAMS
            .iter()
            .find(|d| d.id == id)
            .expect("id in METADATA_PARAMS");
        let value = ParamValue::parse(desc, mapped).map_err(|err| format!("--{id}: {err}"))?;
        values.insert(id.to_string(), value);
    }
    MetadataOptions::from_values(&values).map_err(|err| err.to_string())
}

fn metadata_status_str(status: MetadataStatus) -> &'static str {
    match status {
        MetadataStatus::Off => "off",
        MetadataStatus::Unsupported => "unsupported",
        MetadataStatus::Absent => "absent",
        MetadataStatus::Written => "written",
        MetadataStatus::Skipped => "skipped",
    }
}

fn icc_status_str(status: IccStatus) -> &'static str {
    match status {
        IccStatus::Off => "off",
        IccStatus::Source => "source",
        IccStatus::BuiltinSrgb => "builtin-srgb",
        IccStatus::None => "none",
        IccStatus::Skipped => "skipped",
        IccStatus::Unsupported => "unsupported",
    }
}

/// [`MetadataOutcome`] rendered for the report: statuses as their fixed
/// strings, the frame path as text.
#[derive(Serialize)]
pub struct MetadataReport {
    pub frame: usize,
    pub frame_path: String,
    pub exif: String,
    pub icc: String,
    pub xmp: String,
    pub iptc: String,
    pub warnings: Vec<String>,
}

impl From<&MetadataOutcome> for MetadataReport {
    fn from(outcome: &MetadataOutcome) -> Self {
        MetadataReport {
            frame: outcome.frame,
            frame_path: outcome.frame_path.display().to_string(),
            exif: metadata_status_str(outcome.exif).to_string(),
            icc: icc_status_str(outcome.icc).to_string(),
            xmp: metadata_status_str(outcome.xmp).to_string(),
            iptc: metadata_status_str(outcome.iptc).to_string(),
            warnings: outcome.warnings.clone(),
        }
    }
}

/// Resolves `args` into a validated `ExportOptions` for the format `output`
/// selects: a flag belonging to another format is refused by name, and
/// every given flag is parsed and range-checked through its own
/// `ParamDesc` before `ExportOptions::from_values` fills the rest from that
/// format's defaults.
fn resolve_export_options(output: &Path, args: &FormatArgs) -> Result<ExportOptions, String> {
    let format = resolve_format(output, args.format.as_deref())?;
    let schema = params_for(format);
    let given: [Option<&str>; 6] = [
        args.tiff_bits.as_deref(),
        args.tiff_compression.as_deref(),
        args.png_bits.as_deref(),
        args.png_compression.as_deref(),
        args.jpeg_quality.as_deref(),
        args.jpeg_subsampling.as_deref(),
    ];
    let mut values = ParamValues::new();
    for (id, text) in FORMAT_FLAG_IDS.into_iter().zip(given) {
        let Some(text) = text else { continue };
        let Some(desc) = schema.iter().find(|d| d.id == id) else {
            return Err(format!(
                "--{id} is not an option of --format {}",
                format.id()
            ));
        };
        let value = ParamValue::parse(desc, text).map_err(|err| {
            let rendered = err.to_string();
            let rendered = rendered
                .strip_prefix(&format!("{id}: "))
                .unwrap_or(&rendered);
            format!("--{id}: {rendered}")
        })?;
        values.insert(id.to_string(), value);
    }
    ExportOptions::from_values(format, &values).map_err(|err| err.to_string())
}

/// The `export --json` report.
#[derive(Serialize)]
pub struct ExportReport {
    pub project: String,
    /// Final paths only -- never the `.partial` names a run writes to first.
    pub wrote: Vec<String>,
    /// Every source the project could not record relative to its own
    /// directory. Present and empty is a positive statement, not an
    /// omission.
    pub unresolved_sources: Vec<String>,
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    pub frame_count: u32,
    pub algorithm: String,
    /// The checkpoint this export rendered: `--checkpoint`'s resolved id,
    /// or the head's when it was omitted.
    pub checkpoint: String,
    /// The format `-o`'s image was encoded as. `null` when only `--zmap`
    /// or `--frame-output` was written.
    pub format: Option<String>,
    /// Every descriptor id of `format`'s schema with its resolved value,
    /// defaults included. `null` alongside `format`.
    pub format_options: Option<ParamValues>,
    /// The source frame index written "as fused", if `--frame` was given.
    pub frame: Option<usize>,
    /// EXIF, ICC, XMP and IPTC carry-over for the `-o` image, `null`
    /// without one.
    pub metadata: Option<MetadataReport>,
    pub wall_seconds: f64,
}

/// Runs the subcommand.
///
/// At least one of `-o`/`--zmap`/`--frame` is required **by name**, checked
/// here so the error names the missing flag instead of clap's generic exit
/// 2. `--frame-output` without `--frame` is refused the same way; `--frame`
/// without `--frame-output` is already a clap usage error (`requires`).
/// An output that names the project file itself is refused before anything
/// is written.
/// `checkpoint` is an id or unique id prefix, resolved against the
/// project's own history; `None` renders the head -- `--frame-output` reads
/// the same resolved checkpoint's own fuse state, not the project's head.
#[allow(clippy::too_many_arguments)] // one flag apiece; grouping them would only rename this list
pub fn run(
    project_path: &Path,
    output: Option<&Path>,
    zmap: Option<&Path>,
    checkpoint: Option<&str>,
    format: &FormatArgs,
    frame: Option<usize>,
    frame_output: Option<&Path>,
    json: bool,
) -> ExitCode {
    let start = Instant::now();
    if output.is_none() && zmap.is_none() && frame_output.is_none() {
        return fail(
            "export needs somewhere to write: -o <file> for the image, \
             --zmap <file> for the Z-Map, --frame <index> --frame-output <file> for one \
             source frame, or any combination",
        );
    }
    if frame_output.is_some() && frame.is_none() {
        return fail("--frame-output requires --frame <index>");
    }
    for (flag, path) in [
        ("-o", output),
        ("--zmap", zmap),
        ("--frame-output", frame_output),
    ] {
        if let Some(path) = path
            && would_replace(path, project_path)
        {
            return fail(format!(
                "{flag} {} is also the project file: refusing to overwrite it",
                path.display()
            ));
        }
    }

    let export_options = match output {
        Some(path) => match resolve_export_options(path, format) {
            Ok(options) => Some(options),
            Err(message) => return fail(message),
        },
        None => None,
    };
    let metadata_options = match output {
        Some(_) => match resolve_metadata_options(format) {
            Ok(options) => Some(options),
            Err(message) => return fail(message),
        },
        None => None,
    };

    let mut project = match Project::open(project_path) {
        Ok(project) => project,
        Err(err) => return fail(err),
    };

    // Reported before anything is written: a user who is about to discover
    // their sources moved should learn it at the top of the output, not
    // after a minute of writing.
    let unresolved: Vec<String> = project
        .unresolved_sources()
        .map(|s| s.path.clone())
        .collect();
    if !unresolved.is_empty() {
        let warning = format!(
            "warning: {} source frame(s) unresolved -- not reachable from this project, either \
             because the path was recorded as given or because it is no longer there. An \
             unedited image export reads no source frame; an edited one re-reads its fused \
             ancestor's frames and fails below if one is missing or changed:",
            unresolved.len()
        );
        eprintln!("{warning}");
        for path in &unresolved {
            eprintln!("  {path}");
        }
        tracing::warn!(target: MIRRORED_OUTPUT_TARGET, "{warning}");
    }

    let resolved = match checkpoint {
        Some(id_or_prefix) => {
            crate::history::resolve_checkpoint(project.history(), project_path, id_or_prefix)
        }
        None => Ok(project.checkpoint()),
    };
    let checkpoint = match resolved {
        Ok(checkpoint) => checkpoint.clone(),
        Err(err) => return fail(err),
    };
    let record = match project.fuse_state(&checkpoint.id) {
        Ok(record) => record,
        Err(err) => return fail(err),
    };

    let source_count = project.manifest().sources.len();
    if let Some(index) = frame
        && index >= source_count
    {
        return fail(format!(
            "frame {index} is out of range: this project has frames 0-{}",
            source_count.saturating_sub(1)
        ));
    }

    let mut wrote = Vec::new();
    let mut metadata_outcome: Option<MetadataOutcome> = None;
    if let Some(path) = output {
        let options = export_options
            .as_ref()
            .expect("export_options is Some whenever output is Some");
        let metadata = metadata_options
            .as_ref()
            .expect("metadata_options is Some whenever output is Some");
        match project::export_checkpoint(&mut project, &checkpoint.id, path, options, metadata) {
            Ok(outcome) => {
                for warning in &outcome.warnings {
                    let warning = format!("warning: {warning}");
                    eprintln!("{warning}");
                    tracing::warn!(target: MIRRORED_OUTPUT_TARGET, "{warning}");
                }
                metadata_outcome = Some(outcome);
            }
            Err(err) => return fail(err),
        }
        wrote.push(path.display().to_string());
    }
    if let Some(path) = zmap {
        if let Err(err) = project::materialize_zmap(&mut project, &checkpoint, path) {
            return fail(err);
        }
        wrote.push(path.display().to_string());
    }
    if let (Some(index), Some(path)) = (frame, frame_output) {
        if let Err(err) = export_frame(&project, &record, index, path) {
            return fail(err);
        }
        wrote.push(path.display().to_string());
    }

    let report = ExportReport {
        project: project_path.display().to_string(),
        wrote,
        unresolved_sources: unresolved,
        width: record.composite.width,
        height: record.composite.height,
        samples_per_pixel: record.composite.samples_per_pixel,
        frame_count: record.composite.frame_count,
        algorithm: checkpoint.algorithm,
        checkpoint: checkpoint.id,
        format: export_options.as_ref().map(|o| o.format().id().to_string()),
        format_options: export_options.as_ref().map(|o| o.values()),
        frame,
        metadata: metadata_outcome.as_ref().map(MetadataReport::from),
        wall_seconds: start.elapsed().as_secs_f64(),
    };
    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(err) => return fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        println!("{}", render(&report));
    }
    ExitCode::SUCCESS
}

/// Human rendering: one fact per line, unit-testable without capturing
/// stdout -- the shape `stack::render` uses.
fn render(report: &ExportReport) -> String {
    let mut lines = vec![
        report.project.clone(),
        format!(
            "{}x{} {}ch, {} frames",
            report.width, report.height, report.samples_per_pixel, report.frame_count
        ),
        format!("algorithm: {}", report.algorithm),
        format!("checkpoint: {}", report.checkpoint),
    ];
    if let Some(format) = &report.format {
        lines.push(format!("format: {format}"));
    }
    if let Some(index) = report.frame {
        lines.push(format!("frame: {index}"));
    }
    if let Some(metadata) = &report.metadata {
        lines.push(format!(
            "metadata frame: {} ({})",
            metadata.frame, metadata.frame_path
        ));
        lines.push(format!(
            "metadata: exif={} icc={} xmp={} iptc={}",
            metadata.exif, metadata.icc, metadata.xmp, metadata.iptc
        ));
        for warning in &metadata.warnings {
            lines.push(format!("metadata warning: {warning}"));
        }
    }
    if !report.unresolved_sources.is_empty() {
        lines.push(format!(
            "unresolved sources: {}",
            report.unresolved_sources.len()
        ));
    }
    for path in &report.wrote {
        lines.push(format!("wrote: {path}"));
    }
    lines.push(format!("wall: {:.3}s", report.wall_seconds));
    lines.join("\n")
}

fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("error: {message}");
    tracing::error!(target: MIRRORED_OUTPUT_TARGET, "error: {message}");
    ExitCode::FAILURE
}

/// Writes source frame `index` "as fused" to `path`: warped by its recorded
/// transform and offset by the applied crop, at the resolved checkpoint's
/// own composite width and height -- never the project's head unless that
/// is what `--checkpoint` resolved to. `index` must already be checked in
/// range.
fn export_frame(
    project: &Project,
    record: &FuseRecord,
    index: usize,
    path: &Path,
) -> Result<(), String> {
    let composite = record.composite.clone();
    let registration = record.registration.clone();
    let paths: Vec<PathBuf> = project
        .manifest()
        .sources
        .iter()
        .map(|source| project.source_path(source))
        .collect();

    let budget = HostBudget::host_derived(&host::snapshot());
    let frame_set = open_frame_set(&paths, budget).map_err(|err| err.to_string())?;
    let frame_dims = frame_set.reader.dimensions();
    let transforms = registration.transforms(frame_set.reader.len());
    let gained = GainReader::new(frame_set.reader.as_ref(), registration.gains());
    let reader = WarpingReader::new(&gained, frame_dims, composite.samples_per_pixel, transforms);
    let origin = registration.origin();

    let partial = project::container::PartialFile::new(path);
    let mut writer = TiffWriter::create(
        partial.path(),
        TiffSpec {
            width: composite.width,
            height: composite.height,
            samples_per_pixel: composite.samples_per_pixel,
            description: Some(composite.description.clone()),
        },
    )
    .map_err(|err| err.to_string())?;

    let spp = composite.samples_per_pixel;
    let mut buf = vec![0u16; composite.width as usize * FRAME_BAND_ROWS as usize * spp as usize];
    let mut y = 0u32;
    while y < composite.height {
        let rows = FRAME_BAND_ROWS.min(composite.height - y);
        let rect = Rect {
            x: 0,
            y,
            width: composite.width,
            height: rows,
        };
        let source_rect = Rect {
            x: rect.x + origin.0,
            y: rect.y + origin.1,
            width: rect.width,
            height: rect.height,
        };
        let len = (rect.width * rows * spp) as usize;
        reader
            .read_tile_into(index, source_rect, &mut buf[..len])
            .map_err(|err| err.to_string())?;
        writer
            .write_tile(rect, &buf[..len])
            .map_err(|err| err.to_string())?;
        y += rows;
    }
    writer.finish().map_err(|err| err.to_string())?;
    partial
        .commit()
        .map_err(|err| format!("{}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use focusforge_core::project::export::JpegSubsampling;

    #[test]
    fn resolve_format_defaults_to_tiff_with_no_extension_and_no_flag() {
        assert_eq!(
            resolve_format(Path::new("out"), None),
            Ok(ExportFormat::Tiff)
        );
    }

    #[test]
    fn resolve_format_infers_from_a_recognised_extension() {
        assert_eq!(
            resolve_format(Path::new("out.png"), None),
            Ok(ExportFormat::Png)
        );
    }

    #[test]
    fn resolve_format_refuses_an_unrecognised_extension_with_no_flag() {
        let err = resolve_format(Path::new("out.webp"), None).unwrap_err();
        assert!(err.contains("--format"), "{err}");
    }

    #[test]
    fn resolve_format_refuses_a_flag_that_contradicts_the_extension() {
        let err = resolve_format(Path::new("out.jpg"), Some("tiff")).unwrap_err();
        assert!(err.contains("--format"), "{err}");
    }

    #[test]
    fn resolve_format_honours_an_explicit_flag_over_an_unrecognised_extension() {
        assert_eq!(
            resolve_format(Path::new("out.exr"), Some("tiff")),
            Ok(ExportFormat::Tiff)
        );
    }

    #[test]
    fn resolve_format_infers_dng() {
        assert_eq!(
            resolve_format(Path::new("out.dng"), None),
            Ok(ExportFormat::Dng)
        );
        assert_eq!(
            resolve_format(Path::new("out"), Some("dng")),
            Ok(ExportFormat::Dng)
        );
    }

    #[test]
    fn resolve_format_refuses_tiff_for_a_dng_output() {
        let err = resolve_format(Path::new("out.dng"), Some("tiff")).unwrap_err();
        assert!(err.contains("--format") && err.contains("dng"), "{err}");
    }

    #[test]
    fn resolve_format_refuses_an_unknown_flag_value() {
        let err = resolve_format(Path::new("out.tif"), Some("webp")).unwrap_err();
        assert!(err.contains("--format"), "{err}");
    }

    #[test]
    fn resolve_export_options_defaults_to_tiff_16_none() {
        let options = resolve_export_options(Path::new("out.tif"), &FormatArgs::default())
            .expect("default flags must resolve");
        assert_eq!(options, ExportOptions::default());
    }

    #[test]
    fn resolve_export_options_refuses_a_flag_of_another_format() {
        let args = FormatArgs {
            jpeg_quality: Some("80".to_string()),
            ..Default::default()
        };
        let err = resolve_export_options(Path::new("out.tif"), &args).unwrap_err();
        assert!(err.contains("--jpeg-quality"), "{err}");
    }

    #[test]
    fn resolve_export_options_refuses_an_out_of_range_value_naming_the_flag() {
        let args = FormatArgs {
            format: Some("jpeg".to_string()),
            jpeg_quality: Some("0".to_string()),
            ..Default::default()
        };
        let err = resolve_export_options(Path::new("out.jpg"), &args).unwrap_err();
        assert!(err.starts_with("--jpeg-quality:"), "{err}");
    }

    #[test]
    fn resolve_export_options_builds_a_full_jpeg_choice() {
        let args = FormatArgs {
            jpeg_quality: Some("80".to_string()),
            jpeg_subsampling: Some("420".to_string()),
            ..Default::default()
        };
        let options = resolve_export_options(Path::new("out.jpg"), &args)
            .expect("valid jpeg flags must resolve");
        assert_eq!(
            options,
            ExportOptions::Jpeg {
                quality: 80,
                subsampling: JpegSubsampling::S420,
            }
        );
    }

    #[test]
    fn export_flags_parse_to_the_modal_literals() {
        use focusforge_core::project::export::{
            PngBits, PngCompression, TiffBits, TiffCompression,
        };
        let s = |v: &str| Some(v.to_string());
        let jpeg = |quality: &str, subsampling: Option<String>| FormatArgs {
            format: s("jpeg"),
            jpeg_quality: s(quality),
            jpeg_subsampling: subsampling,
            ..Default::default()
        };
        let cases = [
            (
                FormatArgs {
                    format: s("tiff"),
                    tiff_bits: s("8"),
                    tiff_compression: s("lzw"),
                    ..Default::default()
                },
                ExportOptions::Tiff {
                    bits: TiffBits::Eight,
                    compression: TiffCompression::Lzw,
                },
            ),
            (
                FormatArgs {
                    format: s("tiff"),
                    tiff_bits: s("16"),
                    tiff_compression: s("deflate"),
                    ..Default::default()
                },
                ExportOptions::Tiff {
                    bits: TiffBits::Sixteen,
                    compression: TiffCompression::Deflate,
                },
            ),
            (
                FormatArgs {
                    format: s("png"),
                    png_bits: s("8"),
                    png_compression: s("fast"),
                    ..Default::default()
                },
                ExportOptions::Png {
                    bits: PngBits::Eight,
                    compression: PngCompression::Fast,
                },
            ),
            (
                FormatArgs {
                    format: s("png"),
                    png_bits: s("16"),
                    png_compression: s("best"),
                    ..Default::default()
                },
                ExportOptions::Png {
                    bits: PngBits::Sixteen,
                    compression: PngCompression::Best,
                },
            ),
            (
                jpeg("1", s("444")),
                ExportOptions::Jpeg {
                    quality: 1,
                    subsampling: JpegSubsampling::S444,
                },
            ),
            (
                jpeg("85", s("420")),
                ExportOptions::Jpeg {
                    quality: 85,
                    subsampling: JpegSubsampling::S420,
                },
            ),
            (
                jpeg("100", None),
                ExportOptions::Jpeg {
                    quality: 100,
                    subsampling: JpegSubsampling::S420,
                },
            ),
        ];
        for (args, literal) in cases {
            assert_eq!(
                resolve_export_options(Path::new("out"), &args).expect("valid flags resolve"),
                literal
            );
        }

        let off = FormatArgs {
            exif: s("off"),
            icc: s("off"),
            xmp: s("off"),
            iptc: s("off"),
            ..Default::default()
        };
        assert_eq!(
            resolve_metadata_options(&off).expect("valid flags resolve"),
            MetadataOptions {
                exif: false,
                icc: false,
                xmp: false,
                iptc: false,
            }
        );
        assert_eq!(
            resolve_metadata_options(&FormatArgs::default()).expect("no flags resolve"),
            MetadataOptions::default()
        );
    }

    #[test]
    fn render_names_every_fact_one_per_line() {
        let text = render(&ExportReport {
            project: "demo.ffp".to_string(),
            wrote: vec!["out.tif".to_string(), "out-zmap.tif".to_string()],
            unresolved_sources: vec!["/elsewhere/00.tif".to_string()],
            width: 512,
            height: 512,
            samples_per_pixel: 3,
            frame_count: 8,
            algorithm: "example-stack".to_string(),
            checkpoint: "abc123".to_string(),
            format: Some("tiff".to_string()),
            format_options: Some(ParamValues::new()),
            frame: None,
            metadata: None,
            wall_seconds: 0.25,
        });
        for needle in [
            "demo.ffp",
            "512x512",
            "3ch",
            "8 frames",
            "algorithm: example-stack",
            "checkpoint: abc123",
            "format: tiff",
            "unresolved sources: 1",
            "wrote: out.tif",
            "wrote: out-zmap.tif",
            "wall: 0.250s",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in: {text}");
        }
    }

    #[test]
    fn render_prints_the_metadata_lines_when_metadata_is_some() {
        let text = render(&ExportReport {
            project: "demo.ffp".to_string(),
            wrote: vec!["out.tif".to_string()],
            unresolved_sources: Vec::new(),
            width: 64,
            height: 64,
            samples_per_pixel: 3,
            frame_count: 2,
            algorithm: "example-stack".to_string(),
            checkpoint: "abc123".to_string(),
            format: Some("tiff".to_string()),
            format_options: Some(ParamValues::new()),
            frame: None,
            metadata: Some(MetadataReport {
                frame: 1,
                frame_path: "frame-01.tif".to_string(),
                exif: "written".to_string(),
                icc: "source".to_string(),
                xmp: "off".to_string(),
                iptc: "off".to_string(),
                warnings: vec!["ICC profile exceeds JPEG's embedding limit; skipped".to_string()],
            }),
            wall_seconds: 0.1,
        });
        for needle in [
            "metadata frame: 1 (frame-01.tif)",
            "metadata: exif=written icc=source xmp=off iptc=off",
            "metadata warning: ICC profile exceeds JPEG's embedding limit; skipped",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in: {text}");
        }
    }

    #[test]
    fn resolve_metadata_options_defaults_to_exif_and_icc_on() {
        let options =
            resolve_metadata_options(&FormatArgs::default()).expect("default flags must resolve");
        assert_eq!(options, MetadataOptions::default());
    }

    #[test]
    fn resolve_metadata_options_maps_on_off_through_every_flag() {
        let args = FormatArgs {
            exif: Some("off".to_string()),
            icc: Some("off".to_string()),
            xmp: Some("on".to_string()),
            iptc: Some("on".to_string()),
            ..Default::default()
        };
        let options = resolve_metadata_options(&args).expect("on/off flags must resolve");
        assert_eq!(
            options,
            MetadataOptions {
                exif: false,
                icc: false,
                xmp: true,
                iptc: true,
            }
        );
    }

    #[test]
    fn resolve_metadata_options_refuses_any_other_word_by_flag_name() {
        let args = FormatArgs {
            xmp: Some("maybe".to_string()),
            ..Default::default()
        };
        let err = resolve_metadata_options(&args).unwrap_err();
        assert!(err.starts_with("--xmp:"), "{err}");
    }
}
