// SPDX-License-Identifier: GPL-3.0-or-later
use std::env;
use std::error::Error;
use std::path::Path;

use focusforge_core::align::{align_stack_with, crop_rect, AlignDriverOptions, FailurePolicy, PyramidalEcc, ReferencePolicy, StackAlignment, Transform, WarpingReader};
use focusforge_core::algorithm::{AlignAlgorithm, AlignContext, AlignOutcome, AlignStatus, TileReader};
use focusforge_core::equalize::{self, GainReader};
use focusforge_core::io::{TiffSpec, TiffStack, TiffWriter};
use focusforge_core::params::{ParamValue, ParamValues, StackContext, resolve, resolved_values};
use focusforge_core::tile::Rect;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Strategy { Anchor, Chain, Rolling }

impl Strategy {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "anchor" => Ok(Self::Anchor),
            "chain" => Ok(Self::Chain),
            "rolling" => Ok(Self::Rolling),
            _ => Err(format!("unknown strategy: {value}")),
        }
    }

    fn policy(self) -> Option<ReferencePolicy> {
        match self {
            Self::Anchor => Some(ReferencePolicy::Anchor),
            Self::Chain => Some(ReferencePolicy::Chain),
            Self::Rolling => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct FrameRecord {
    frame: usize,
    transform: Transform,
    diverged: bool,
    status: String,
    reason: Option<String>,
    iterations: u32,
    iterations_reason: Option<String>,
    best_rho: f64,
    read_error: Option<String>,
    solver_fraction: Option<f64>,
    validation_fraction: Option<f64>,
    solver_fraction_reason: Option<String>,
    validation_fraction_reason: Option<String>,
    interpolation_count: u32,
    fitting_template_coverage: Option<f64>,
    fitting_moving_coverage: Option<f64>,
    fitting_window: Option<(u32, u32, u32, u32)>,
    fitting_template_frame: Option<usize>,
    fitting_prewarp: Option<Transform>,
    fitting_reader: Option<String>,
}

struct StrategyOutput {
    reference: usize,
    records: Vec<FrameRecord>,
    crop: Option<Rect>,
    coverage: Vec<f64>,
    outputs: Vec<String>,
    masks: Vec<String>,
}

#[derive(Clone, Copy, Debug)]
struct RollingState {
    last_success: Transform,
    template_frame: usize,
}

impl RollingState {
    fn new(reference: usize) -> Self { Self { last_success: Transform::identity(), template_frame: reference } }
}

fn fitting_window(stack: StackContext) -> Rect {
    let raw_w = ((stack.width as f64) * 0.75).round().max(1.0) as u32;
    let raw_h = ((stack.height as f64) * 0.75).round().max(1.0) as u32;
    let (mut width, mut height) = (raw_w, raw_h);
    if u64::from(width) * u64::from(height) > (1 << 20) {
        let factor = ((1 << 20) as f64 / (u64::from(width) * u64::from(height)) as f64).sqrt();
        width = (width as f64 * factor).floor().max(1.0) as u32;
        height = (height as f64 * factor).floor().max(1.0) as u32;
    }
    width = width.min(stack.width);
    height = height.min(stack.height);
    let x = ((stack.width - width) / 2).max(1);
    let y = ((stack.height - height) / 2).max(1);
    Rect { x, y, width: width.min(stack.width.saturating_sub(1).saturating_sub(x)), height: height.min(stack.height.saturating_sub(1).saturating_sub(y)) }
}

fn coverage_fraction(reader: &dyn TileReader, frame: usize, rect: Rect, samples_per_pixel: u32) -> Result<f64, String> {
    let mut samples = vec![0; rect.width as usize * rect.height as usize * samples_per_pixel as usize];
    let covered = reader.read_tile_coverage_into(frame, rect, &mut samples).map_err(|error| error.to_string())?;
    Ok(covered.map(|mask| mask.into_iter().filter(|value| *value).count() as f64 / (rect.width as f64 * rect.height as f64)).unwrap_or(1.0))
}

fn run_anchor_or_chain(strategy: Strategy, stack: StackContext, reader: &dyn TileReader, cfg: &ParamValues) -> Result<StackAlignment, String> {
    let Some(policy) = strategy.policy() else {
        return Err("rolling uses its dedicated reader path".into());
    };
    if !matches!(cfg.get("reference-frame").and_then(ParamValue::as_enum), Some("first" | "middle")) {
        return Err("anchor and chain require first or middle reference".into());
    }
    if cfg.get("mode").and_then(ParamValue::as_enum) != Some("rotation-scale") {
        return Err("anchor and chain require rotation-scale mode".into());
    }
    let mut cfg = cfg.clone();
    cfg.insert("reference-policy".into(), ParamValue::Enum(match policy { ReferencePolicy::Anchor => "anchor", ReferencePolicy::Chain => "chain" }.into()));
    align_stack_with(&PyramidalEcc, stack, reader, &cfg, FailurePolicy::Strict, &AlignDriverOptions { align_backend: None, ..Default::default() })
        .map(|run| run.alignment)
        .map_err(|error| error.to_string())
}

fn rolling_solver_leg_with<F>(
    stack: StackContext,
    reader: &dyn TileReader,
    cfg: &ParamValues,
    reader_name: &str,
    reference: usize,
    direction: isize,
    injected_failure: Option<usize>,
    mut solver: F,
) -> Result<Vec<FrameRecord>, String>
where
    F: FnMut(&dyn TileReader, StackContext, usize, usize, &ParamValues) -> Result<AlignOutcome, String>,
{
    let mut state = RollingState::new(reference);
    let mut records = Vec::new();
    let mut next = reference as isize + direction;
    while (0..stack.frame_count as isize).contains(&next) {
        let frame = next as usize;
        let mut prewarp = vec![Transform::identity(); stack.frame_count];
        prewarp[state.template_frame] = state.last_success;
        prewarp[frame] = state.last_success;
        let warped = WarpingReader::new(
            reader,
            (stack.width, stack.height),
            stack.samples_per_pixel,
            prewarp,
        );
        let window = fitting_window(stack);
        let fitting = coverage_fraction(&warped, state.template_frame, window, stack.samples_per_pixel)
            .and_then(|template| coverage_fraction(&warped, frame, window, stack.samples_per_pixel).map(|moving| (template, moving)));
        let (template_coverage, moving_coverage) = match fitting {
            Ok(coverage) => coverage,
            Err(error) => {
                records.push(FrameRecord {
                    frame, transform: state.last_success, diverged: true, status: "read-error".into(), reason: Some(error.clone()),
                    iterations: 0, iterations_reason: None, best_rho: f64::NAN, read_error: Some(error), solver_fraction: None,
                    validation_fraction: None, solver_fraction_reason: Some("reader-failure".into()), validation_fraction_reason: Some("reader-failure".into()),
                    interpolation_count: if state.last_success == Transform::identity() { 0 } else { 2 }, fitting_template_coverage: None,
                    fitting_moving_coverage: None, fitting_window: Some((window.x, window.y, window.width, window.height)), fitting_template_frame: Some(state.template_frame), fitting_prewarp: Some(state.last_success), fitting_reader: Some(reader_name.into()),
                });
                next += direction;
                continue;
            }
        };
        let outcome = match solver(&warped, stack, state.template_frame, frame, cfg) {
            Ok(outcome) => outcome,
            Err(error) => {
                records.push(FrameRecord {
                    frame,
                    transform: state.last_success,
                    diverged: true,
                    status: "read-error".into(),
                    reason: Some(error.to_string()),
                    iterations: 0,
                    iterations_reason: None,
                    best_rho: f64::NAN,
                    read_error: Some(error.to_string()),
                    solver_fraction: None,
                    validation_fraction: None,
                    solver_fraction_reason: Some("reader-failure".into()),
                    validation_fraction_reason: Some("reader-failure".into()),
                    interpolation_count: if state.last_success == Transform::identity() { 0 } else { 2 },
                    fitting_template_coverage: None,
                    fitting_moving_coverage: None,
                    fitting_window: Some((window.x, window.y, window.width, window.height)),
                    fitting_template_frame: Some(state.template_frame),
                    fitting_prewarp: Some(state.last_success),
                    fitting_reader: Some(reader_name.into()),
                });
                next += direction;
                continue;
            }
        };
        let composed = state.last_success.compose(&outcome.transform);
        let (mut status, mut reason, mut diverged) = status_fields(&outcome.status);
        if injected_failure == Some(frame) {
            status = "injected-failure".into();
            reason = Some("registered-control".into());
            diverged = true;
        }
        records.push(FrameRecord {
            frame,
            transform: composed,
            diverged,
            status,
            reason,
            iterations: outcome.iterations,
            iterations_reason: None,
            best_rho: outcome.residual,
            read_error: None,
            solver_fraction: None,
            validation_fraction: None,
            solver_fraction_reason: Some("not-exposed-by-align-outcome".into()),
            validation_fraction_reason: Some("not-exposed-by-align-outcome".into()),
            interpolation_count: if state.last_success == Transform::identity() { 0 } else { 2 },
            fitting_template_coverage: Some(template_coverage),
            fitting_moving_coverage: Some(moving_coverage),
            fitting_window: Some((window.x, window.y, window.width, window.height)),
            fitting_template_frame: Some(state.template_frame),
            fitting_prewarp: Some(state.last_success),
            fitting_reader: Some(reader_name.into()),
        });
        if !diverged {
            state.last_success = composed;
            state.template_frame = frame;
        }
        next += direction;
    }
    Ok(records)
}

fn run_rolling(
    stack: StackContext,
    reader: &dyn TileReader,
    cfg: &ParamValues,
    reader_name: &str,
    reference: usize,
    injected_failure: Option<usize>,
) -> Result<Vec<FrameRecord>, String> {
    if reference >= stack.frame_count {
        return Err("reference is outside stack".into());
    }
    let solve = |warped: &dyn TileReader, stack: StackContext, template: usize, frame: usize, cfg: &ParamValues| {
        PyramidalEcc.align_frame(&AlignContext::new(stack, template, warped), frame, cfg).map_err(|error| error.to_string())
    };
    let mut records = rolling_solver_leg_with(stack, reader, cfg, reader_name, reference, -1, injected_failure, solve)?;
    let solve = |warped: &dyn TileReader, stack: StackContext, template: usize, frame: usize, cfg: &ParamValues| {
        PyramidalEcc.align_frame(&AlignContext::new(stack, template, warped), frame, cfg).map_err(|error| error.to_string())
    };
    records.extend(rolling_solver_leg_with(stack, reader, cfg, reader_name, reference, 1, injected_failure, solve)?);
    records.push(FrameRecord {
        frame: reference,
        transform: Transform::identity(),
        diverged: false,
        status: "converged".into(),
        reason: None,
        iterations: 0,
        iterations_reason: None,
        best_rho: 1.0,
        read_error: None,
        solver_fraction: None,
        validation_fraction: None,
        solver_fraction_reason: Some("reference-frame".into()),
        validation_fraction_reason: Some("reference-frame".into()),
        interpolation_count: 0,
        fitting_template_coverage: None,
        fitting_moving_coverage: None,
        fitting_window: None,
        fitting_template_frame: None,
        fitting_prewarp: None,
        fitting_reader: None,
    });
    records.sort_by_key(|record| record.frame);
    Ok(records)
}

fn records_from_alignment(alignment: StackAlignment) -> Vec<FrameRecord> {
    alignment
        .frames
        .into_iter()
        .map(|frame| FrameRecord {
            frame: frame.frame,
            transform: frame.transform,
            diverged: matches!(frame.status, AlignStatus::Diverged(_)),
            status: status_fields(&frame.status).0,
            reason: status_fields(&frame.status).1,
            iterations: frame.iterations,
            iterations_reason: None,
            best_rho: frame.residual,
            read_error: None,
            solver_fraction: None,
            validation_fraction: None,
            solver_fraction_reason: Some("not-exposed-by-align-outcome".into()),
            validation_fraction_reason: Some("not-exposed-by-align-outcome".into()),
            interpolation_count: 0,
            fitting_template_coverage: None,
            fitting_moving_coverage: None,
            fitting_window: None,
            fitting_template_frame: None,
            fitting_prewarp: None,
            fitting_reader: None,
        })
        .collect()
}

fn status_fields(status: &AlignStatus) -> (String, Option<String>, bool) {
    match status {
        AlignStatus::Converged => ("converged".into(), None, false),
        AlignStatus::HitCap => ("hit-cap".into(), None, false),
        AlignStatus::Stalled => ("stalled".into(), None, false),
        AlignStatus::Diverged(reason) => ("diverged".into(), Some(format!("{reason:?}")), true),
        _ => ("unknown".into(), Some("unrecognized-status".into()), true),
    }
}

fn product_crop(records: &[FrameRecord], frame: (u32, u32)) -> Result<Option<Rect>, String> {
    if records.iter().any(|record| !finite(record.transform)) {
        return Err("nonfinite transform has no crop".into());
    }
    crop_rect(
        &records.iter().map(|record| record.transform).collect::<Vec<_>>(),
        frame,
    )
    .map_err(|error| error.to_string())
}

fn product_resample_coverage(
    reader: &dyn TileReader,
    stack: StackContext,
    records: &[FrameRecord],
    crop: Rect,
) -> Result<Vec<f64>, String> {
    if records.len() != stack.frame_count {
        return Err("resample records do not cover every frame".into());
    }
    records
        .iter()
        .map(|record| {
            let warped = focusforge_core::align::warp::warp_tile(
                reader,
                record.frame,
                (stack.width, stack.height),
                stack.samples_per_pixel,
                crop,
                &record.transform,
            )
            .map_err(|error| error.to_string())?;
            Ok(warped.covered.iter().filter(|covered| **covered).count() as f64 / warped.covered.len() as f64)
        })
        .collect()
}

fn write_resampled(reader: &dyn TileReader, stack: StackContext, records: &[FrameRecord], crop: Rect, output: &str) -> Result<(Vec<String>, Vec<String>), String> {
    std::fs::create_dir_all(output).map_err(|error| error.to_string())?;
    let pairs = records.iter().map(|record| {
        let tile = focusforge_core::align::warp::warp_tile(reader, record.frame, (stack.width, stack.height), stack.samples_per_pixel, crop, &record.transform).map_err(|error| error.to_string())?;
        let path = Path::new(output).join(format!("frame-{:02}.tif", record.frame));
        let mut writer = TiffWriter::create(&path, TiffSpec { width: crop.width, height: crop.height, samples_per_pixel: stack.samples_per_pixel, description: None }).map_err(|error| error.to_string())?;
        writer.write_tile(Rect { x: 0, y: 0, width: crop.width, height: crop.height }, &tile.tile.samples).map_err(|error| error.to_string())?;
        writer.finish().map_err(|error| error.to_string())?;
        let mask_path = Path::new(output).join(format!("frame-{:02}-mask.tif", record.frame));
        let mask = tile.covered.iter().map(|covered| if *covered { 65535 } else { 0 }).collect::<Vec<_>>();
        let mut mask_writer = TiffWriter::create(&mask_path, TiffSpec { width: crop.width, height: crop.height, samples_per_pixel: 1, description: None }).map_err(|error| error.to_string())?;
        mask_writer.write_tile(Rect { x: 0, y: 0, width: crop.width, height: crop.height }, &mask).map_err(|error| error.to_string())?;
        mask_writer.finish().map_err(|error| error.to_string())?;
        Ok((path.canonicalize().map_err(|error| error.to_string())?.display().to_string(), mask_path.canonicalize().map_err(|error| error.to_string())?.display().to_string()))
    }).collect::<Result<Vec<_>, String>>()?;
    Ok(pairs.into_iter().unzip())
}

fn parse_transform_file(path: &str, frames: usize) -> Result<Vec<Transform>, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let mut transforms = vec![None; frames];
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let values = line.split(',').collect::<Vec<_>>();
        if values.len() != 10 { return Err("transform row needs frame plus nine values".into()); }
        let index = values[0].parse::<usize>().map_err(|_| "transform frame index is invalid")?;
        if index >= frames || transforms[index].is_some() { return Err("transform frame index differs".into()); }
        let mut matrix = [[0.0; 3]; 3];
        for row in 0..3 { for column in 0..3 {
            matrix[row][column] = values[1 + row * 3 + column].parse::<f64>().map_err(|_| "transform value is invalid")?;
            if !matrix[row][column].is_finite() { return Err("transform value is nonfinite".into()); }
        }}
        transforms[index] = Some(Transform { m: matrix });
    }
    transforms.into_iter().collect::<Option<Vec<_>>>().ok_or_else(|| "transform matrix is incomplete".into())
}

fn resample_only(input: &str, transform_file: &str, output: &str) -> Result<String, String> {
    let reader = TiffStack::open(input).map_err(|error| error.to_string())?;
    let (width, height) = reader.dimensions();
    let stack = StackContext::new(reader.len(), width, height, reader.header(0).samples_per_pixel);
    let transforms = parse_transform_file(transform_file, stack.frame_count)?;
    let crop = crop_rect(&transforms, (width, height)).map_err(|error| error.to_string())?.ok_or("resample has no common crop")?;
    std::fs::create_dir_all(output).map_err(|error| error.to_string())?;
    let mut outputs = Vec::new();
    let mut masks = Vec::new();
    let mut coverage = Vec::new();
    for (frame, transform) in transforms.iter().enumerate() {
        let warped = focusforge_core::align::warp::warp_tile(&reader, frame, (width, height), stack.samples_per_pixel, crop, transform).map_err(|error| error.to_string())?;
        let image_path = Path::new(output).join(format!("frame-{:02}.tif", frame));
        let mut image = TiffWriter::create(&image_path, TiffSpec { width: crop.width, height: crop.height, samples_per_pixel: stack.samples_per_pixel, description: None }).map_err(|error| error.to_string())?;
        image.write_tile(Rect { x: 0, y: 0, width: crop.width, height: crop.height }, &warped.tile.samples).map_err(|error| error.to_string())?;
        image.finish().map_err(|error| error.to_string())?;
        let mask_path = Path::new(output).join(format!("frame-{:02}-mask.tif", frame));
        let samples = warped.covered.iter().map(|covered| if *covered { 65535 } else { 0 }).collect::<Vec<_>>();
        let mut mask = TiffWriter::create(&mask_path, TiffSpec { width: crop.width, height: crop.height, samples_per_pixel: 1, description: None }).map_err(|error| error.to_string())?;
        mask.write_tile(Rect { x: 0, y: 0, width: crop.width, height: crop.height }, &samples).map_err(|error| error.to_string())?;
        mask.finish().map_err(|error| error.to_string())?;
        coverage.push(warped.covered.iter().filter(|value| **value).count() as f64 / warped.covered.len() as f64);
        outputs.push(json_string(&image_path.canonicalize().map_err(|error| error.to_string())?.display().to_string()));
        masks.push(json_string(&mask_path.canonicalize().map_err(|error| error.to_string())?.display().to_string()));
    }
    Ok(format!("{{\"crop\":[{},{},{},{}],\"outputs\":[{}],\"masks\":[{}],\"reader_coverage\":[{}]}}", crop.x, crop.y, crop.width, crop.height, outputs.join(","), masks.join(","), coverage.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")))
}

fn run_directory(input: &str, strategy: Strategy, reference: &str, order: &str, injected_failure: Option<(usize, String)>, real: bool, output: Option<&str>) -> Result<StrategyOutput, String> {
    let reader = TiffStack::open(input).map_err(|error| error.to_string())?;
    if reader.is_empty() {
        return Err("input stack is empty".into());
    }
    let (width, height) = reader.dimensions();
    let stack = StackContext::new(reader.len(), width, height, reader.header(0).samples_per_pixel);
    let mut cfg = resolved_values(&resolve(PyramidalEcc.params(), &ParamValues::new(), &stack).map_err(|error| error.to_string())?);
    cfg.insert("mode".into(), ParamValue::Enum("rotation-scale".into()));
    cfg.insert("reference-frame".into(), ParamValue::Enum(reference.into()));
    let reference_index = if reference == "first" { 0 } else { (stack.frame_count - 1) / 2 };
    if let Some((frame, leg)) = injected_failure.as_ref() {
        if *frame >= stack.frame_count || (leg == "left" && *frame >= reference_index) || (leg == "right" && *frame <= reference_index) {
            return Err("injected frame does not belong to its declared rolling leg".into());
        }
    }
    if !matches!(order, "none" | "gain-before" | "truth-oracle") {
        return Err("order must be none, gain-before, or truth-oracle".into());
    }
    let gains = if order == "gain-before" {
        equalize::estimate(&reader, stack.frame_count, (stack.width, stack.height), stack.samples_per_pixel, reference_index, 1.5)
            .map_err(|error| error.to_string())?
            .into_iter().map(|estimate| (estimate.applied * 65536.0).round() as u32).collect()
    } else { Vec::new() };
    let gained = GainReader::new(&reader, &gains);
    let active_reader: &dyn TileReader = if order == "gain-before" { &gained } else { &reader };
    let records = match strategy {
        Strategy::Anchor | Strategy::Chain => {
            cfg.insert("reference-policy".into(), ParamValue::Enum(match strategy { Strategy::Anchor => "anchor", Strategy::Chain => "chain", Strategy::Rolling => unreachable!() }.into()));
            records_from_alignment(run_anchor_or_chain(strategy, stack, active_reader, &cfg)?)
        }
        Strategy::Rolling => run_rolling(stack, active_reader, &cfg, if order == "gain-before" { "gain-reader" } else { "raw-reader" }, reference_index, injected_failure.map(|(frame, _)| frame))?,
    };
    if real && records.iter().any(|record| record.diverged || record.read_error.is_some() || !finite(record.transform)) {
        return Err("real preparation refuses failed or unreadable alignment".into());
    }
    let crop = product_crop(&records, (stack.width, stack.height))?;
    // GainReader belongs only before fitting.  Stored product pixels and
    // coverage always come from the same original u16 raw reader used by
    // none/oracle metric scoring.
    let coverage = match crop {
        Some(crop) => product_resample_coverage(&reader, stack, &records, crop)?,
        None => Vec::new(),
    };
    let (outputs, masks) = match (crop, output) {
        (Some(crop), Some(directory)) => write_resampled(&reader, stack, &records, crop, directory)?,
        _ => (Vec::new(), Vec::new()),
    };
    Ok(StrategyOutput { reference: reference_index, records, crop, coverage, outputs, masks })
}

fn json_records(strategy: Strategy, output: &StrategyOutput) -> String {
    let strategy = match strategy { Strategy::Anchor => "anchor", Strategy::Chain => "chain", Strategy::Rolling => "rolling" };
    let rows = output.records.iter().map(|record| {
        let rho = if record.best_rho.is_finite() { record.best_rho.to_string() } else { "null".into() };
        let error = record.read_error.as_ref().map(|error| json_string(error)).unwrap_or_else(|| "null".into());
        let reason = record.reason.as_ref().map(|value| json_string(value)).unwrap_or_else(|| "null".into());
        let iterations_reason = record.iterations_reason.as_ref().map(|value| json_string(value)).unwrap_or_else(|| "null".into());
        let solver_fraction = record.solver_fraction.map(|value| value.to_string()).unwrap_or_else(|| "null".into());
        let validation_fraction = record.validation_fraction.map(|value| value.to_string()).unwrap_or_else(|| "null".into());
        let solver_reason = record.solver_fraction_reason.as_ref().map(|value| json_string(value)).unwrap_or_else(|| "null".into());
        let validation_reason = record.validation_fraction_reason.as_ref().map(|value| json_string(value)).unwrap_or_else(|| "null".into());
        let fitting_template = record.fitting_template_coverage.map(|value| value.to_string()).unwrap_or_else(|| "null".into());
        let fitting_moving = record.fitting_moving_coverage.map(|value| value.to_string()).unwrap_or_else(|| "null".into());
        let fitting_window = record.fitting_window.map(|(x, y, width, height)| format!("[{}, {}, {}, {}]", x, y, width, height)).unwrap_or_else(|| "null".into());
        let fitting_template_frame = record.fitting_template_frame.map(|value| value.to_string()).unwrap_or_else(|| "null".into());
        let fitting_prewarp = record.fitting_prewarp.map(|matrix| format!("[[{}, {}, {}],[{}, {}, {}],[{}, {}, {}]]", matrix.m[0][0], matrix.m[0][1], matrix.m[0][2], matrix.m[1][0], matrix.m[1][1], matrix.m[1][2], matrix.m[2][0], matrix.m[2][1], matrix.m[2][2])).unwrap_or_else(|| "null".into());
        let fitting_reader = record.fitting_reader.as_ref().map(|value| json_string(value)).unwrap_or_else(|| "null".into());
        format!(
            "{{\"frame\":{},\"status\":{},\"reason\":{},\"iterations\":{},\"iterations_reason\":{},\"read_error\":{},\"best_rho\":{},\"solver_fraction\":{},\"validation_fraction\":{},\"solver_fraction_reason\":{},\"validation_fraction_reason\":{},\"interpolation_count\":{},\"fitting_template_coverage\":{},\"fitting_moving_coverage\":{},\"fitting_window\":{},\"fitting_template_frame\":{},\"fitting_prewarp\":{},\"fitting_reader\":{},\"transform\":[[{},{},{}],[{},{},{}],[{},{},{}]]}}",
            record.frame,
            json_string(&record.status),
            reason,
            record.iterations,
            iterations_reason,
            error,
            rho,
            solver_fraction,
            validation_fraction,
            solver_reason,
            validation_reason,
            record.interpolation_count,
            fitting_template,
            fitting_moving,
            fitting_window,
            fitting_template_frame,
            fitting_prewarp,
            fitting_reader,
            record.transform.m[0][0], record.transform.m[0][1], record.transform.m[0][2],
            record.transform.m[1][0], record.transform.m[1][1], record.transform.m[1][2],
            record.transform.m[2][0], record.transform.m[2][1], record.transform.m[2][2],
        )
    }).collect::<Vec<_>>().join(",");
    let crop = output.crop.map(|rect| format!("[{}, {}, {}, {}]", rect.x, rect.y, rect.width, rect.height)).unwrap_or_else(|| "null".into());
    let coverage = output.coverage.iter().map(ToString::to_string).collect::<Vec<_>>().join(",");
    let outputs = output.outputs.iter().map(|path| json_string(path)).collect::<Vec<_>>().join(",");
    let masks = output.masks.iter().map(|path| json_string(path)).collect::<Vec<_>>().join(",");
    format!("{{\"strategy\":\"{}\",\"reference\":{},\"crop\":{},\"reader_coverage\":[{}],\"outputs\":[{}],\"masks\":[{}],\"frames\":[{}]}}", strategy, output.reference, crop, coverage, outputs, masks, rows)
}

fn json_string(value: &str) -> String {
    let mut output = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""), '\\' => output.push_str("\\\\"), '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"), '\t' => output.push_str("\\t"),
            ch if ch.is_control() => output.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => output.push(ch),
        }
    }
    output.push('"');
    output
}

fn rebase(transforms: &[Transform], reference: usize) -> Result<Vec<Transform>, String> {
    let reference_transform = transforms.get(reference).ok_or("reference is outside transform list")?.inverse().map_err(|error| error.to_string())?;
    Ok(transforms.iter().map(|transform| transform.compose(&reference_transform)).collect())
}

fn finite(transform: Transform) -> bool {
    transform.m.iter().flatten().all(|value| value.is_finite())
}

fn self_check() -> Result<(), String> {
    if PyramidalEcc.id() != "pyramidal-ecc" {
        return Err("public complete solver contract is unavailable".into());
    }
    let a = Transform::translation(2.0, 0.0);
    let b = Transform::similarity(1.0, 0.2, 0.0, 0.0);
    if a.compose(&b) == b.compose(&a) { return Err("composition check is degenerate".into()); }
    if !finite(a.compose(&b)) { return Err("finite composition record is invalid".into()); }
    if Strategy::parse("rolling")?.policy().is_some() || Strategy::parse("anchor")?.policy() != Some(ReferencePolicy::Anchor) { return Err("strategy policy binding failed".into()); }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    if arguments.as_slice() == ["--self-check"] {
        self_check().map_err(std::io::Error::other)?;
        println!("alignment reference records: ok");
        return Ok(());
    }
    if arguments.first().is_some_and(|value| value == "--resample") {
        let value = |name: &str| arguments.iter().position(|item| item == name).and_then(|index| arguments.get(index + 1)).map(String::as_str);
        let output = resample_only(value("--input").ok_or("missing input")?, value("--transforms").ok_or("missing transforms")?, value("--output").ok_or("missing output")?).map_err(std::io::Error::other)?;
        println!("{output}");
        return Ok(());
    }
    if arguments.first().is_some_and(|value| value == "--input") {
        let value = |name: &str| arguments.iter().position(|item| item == name).and_then(|index| arguments.get(index + 1)).map(String::as_str);
        let input = value("--input").ok_or("missing input")?;
        let strategy = Strategy::parse(value("--strategy").ok_or("missing strategy")?).map_err(std::io::Error::other)?;
        let reference = value("--reference").ok_or("missing reference")?;
        if !matches!(reference, "first" | "middle") {
            return Err("reference must be first or middle".into());
        }
        let order = value("--order").unwrap_or("none");
        let injected = value("--inject-failure").map(|item| item.parse::<usize>()).transpose().map_err(std::io::Error::other)?;
        if injected.is_some() && strategy != Strategy::Rolling {
            return Err("injected failure requires rolling strategy".into());
        }
        let leg = value("--leg");
        if let Some(leg) = leg {
            if !matches!(leg, "left" | "right") {
                return Err("leg must be left or right".into());
            }
        }
        let injected = match (injected, leg) {
            (Some(frame), Some(leg)) => Some((frame, leg.to_owned())),
            (Some(_), None) => return Err("injected failure requires an explicit leg".into()),
            (None, Some(_)) => return Err("leg requires injected failure".into()),
            (None, None) => None,
        };
        let output = run_directory(input, strategy, reference, order, injected, arguments.iter().any(|value| value == "--real"), value("--output")).map_err(std::io::Error::other)?;
        println!("{}", json_records(strategy, &output));
        return Ok(());
    }
    Err("usage: alignment-reference-strategies --self-check | --resample --input <dir> --transforms <csv> --output <dir> | --input <dir> --strategy <anchor|chain|rolling> --reference <first|middle> [--order <none|gain-before|truth-oracle>] [--inject-failure <frame> --leg <left|right>] [--real]".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockReader;

    impl TileReader for MockReader {
        fn read_tile_into(&self, frame: usize, rect: Rect, out: &mut [u16]) -> Result<(), focusforge_core::algorithm::AlgorithmError> {
            if out.len() != rect.width as usize * rect.height as usize { panic!("mock tile length"); }
            out.fill(frame as u16 + 7);
            Ok(())
        }
    }

    struct FailingReader;

    impl TileReader for FailingReader {
        fn read_tile_into(&self, frame: usize, rect: Rect, out: &mut [u16]) -> Result<(), focusforge_core::algorithm::AlgorithmError> {
            if frame == 1 {
                return Err(focusforge_core::algorithm::AlgorithmError::FrameOutOfRange { frame, frame_count: 1 });
            }
            if out.len() != rect.width as usize * rect.height as usize { panic!("failing mock tile length"); }
            out.fill(frame as u16 + 7);
            Ok(())
        }
    }

    #[test]
    fn known_composition_and_rebase_are_pinned() {
        let scale_rotate = Transform::similarity(2.0, 0.0, 0.0, 0.0);
        let translated = Transform::translation(3.0, -5.0);
        let composed = translated.compose(&scale_rotate);
        assert_eq!(composed.map_point(2.0, 3.0), Some((7.0, 1.0)));
        assert_eq!(scale_rotate.compose(&translated).map_point(2.0, 3.0), Some((10.0, -4.0)));
        assert_eq!(composed.inverse().unwrap().map_point(7.0, 1.0), Some((2.0, 3.0)));
        let transforms = [Transform::translation(4.0, 0.0), Transform::translation(7.0, 0.0), Transform::translation(10.0, 0.0), Transform::translation(13.0, 0.0)];
        assert_eq!(rebase(&transforms, 0).unwrap()[0], Transform::identity());
        assert_eq!(rebase(&transforms, 1).unwrap()[1], Transform::identity());
    }

    #[test]
    fn json_diagnostics_escape_controls() {
        assert_eq!(json_string("\"\\\n\r\t\u{0001}"), "\"\\\"\\\\\\n\\r\\t\\u0001\"");
    }

    #[test]
    fn production_rolling_callback_keeps_last_success_after_failure() {
        let stack = StackContext::new(4, 4, 4, 1);
        let first = Transform::translation(2.0, 1.0);
        let failed = Transform::similarity(1.0, 1.0, 3.0, -2.0);
        let last = Transform::translation(-1.0, 4.0);
        let rows = rolling_solver_leg_with(stack, &MockReader, &ParamValues::new(), "unit-reader", 0, 1, None, |reader, _, template, frame, _| {
            let mut samples = vec![0; 16];
            reader.read_tile_coverage_into(template, Rect { x: 0, y: 0, width: 4, height: 4 }, &mut samples).unwrap();
            assert_eq!(samples.len(), 16);
            if frame == 1 {
                let mut pixels = vec![0; 1];
                reader.read_tile_into(frame, Rect { x: 1, y: 1, width: 1, height: 1 }, &mut pixels).unwrap();
                assert_eq!(pixels, vec![8]);
            }
            let status = if frame == 2 { AlignStatus::Diverged(focusforge_core::align::DivergenceReason::LemmaOneRegion) } else { AlignStatus::Converged };
            let transform = if frame == 1 { first } else if frame == 2 { failed } else { last };
            Ok(AlignOutcome { transform, status, iterations: 3, residual: 0.5 })
        }).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[1].diverged);
        assert_eq!(rows[1].transform, first.compose(&failed));
        assert_eq!(rows[2].transform, first.compose(&last));
        assert_eq!(rows[2].status, "converged");
        assert_eq!(rows[0].fitting_template_frame, Some(0));
        assert_eq!(rows[1].fitting_template_frame, Some(1));
        assert_eq!(rows[2].fitting_template_frame, Some(1));
        assert_eq!(rows[2].fitting_window, Some((1, 1, 2, 2)));
        assert_eq!(rows[2].fitting_template_coverage, Some(0.5));
        assert_eq!(rows[2].fitting_moving_coverage, Some(0.5));
    }

    #[test]
    fn production_rolling_both_legs_reset_and_read_errors_never_advance() {
        let stack = StackContext::new(5, 4, 4, 1);
        let left = rolling_solver_leg_with(stack, &MockReader, &ParamValues::new(), "unit-reader", 2, -1, None, |reader, _, template, frame, _| {
            let mut samples = vec![0; 16];
            reader.read_tile_coverage_into(frame, Rect { x: 0, y: 0, width: 4, height: 4 }, &mut samples).unwrap();
            assert_eq!(samples[0], frame as u16 + 7);
            let mut pixels = vec![0; 1];
            reader.read_tile_into(frame, Rect { x: 1, y: 1, width: 1, height: 1 }, &mut pixels).unwrap();
            assert_eq!(pixels, vec![frame as u16 + 7]);
            assert_eq!(template, 2);
            if frame == 1 { return Err("mock read error".into()); }
            Ok(AlignOutcome { transform: Transform::translation(-2.0, 0.0), status: AlignStatus::Converged, iterations: 1, residual: 0.25 })
        }).unwrap();
        assert_eq!(left.iter().map(|row| row.frame).collect::<Vec<_>>(), vec![1, 0]);
        assert_eq!(left[0].status, "read-error");
        assert_eq!(left[0].transform, Transform::identity());
        assert_eq!(left[1].transform, Transform::translation(-2.0, 0.0));
        let right = rolling_solver_leg_with(stack, &MockReader, &ParamValues::new(), "unit-reader", 2, 1, None, |reader, _, template, frame, _| {
            let mut samples = vec![0; 16];
            reader.read_tile_coverage_into(template, Rect { x: 0, y: 0, width: 4, height: 4 }, &mut samples).unwrap();
            if frame == 3 {
                assert_eq!(template, 2);
                assert_eq!(samples[0], 9);
                let mut pixels = vec![0; 1];
                reader.read_tile_into(template, Rect { x: 1, y: 1, width: 1, height: 1 }, &mut pixels).unwrap();
                assert_eq!(pixels, vec![9]);
            } else {
                assert_eq!(frame, 4);
                assert_eq!(template, 3);
            }
            Ok(AlignOutcome { transform: Transform::translation(2.0, 0.0), status: AlignStatus::Converged, iterations: 1, residual: 0.25 })
        }).unwrap();
        assert_eq!(right.iter().map(|row| row.frame).collect::<Vec<_>>(), vec![3, 4]);
        assert_eq!(right[0].transform, Transform::translation(2.0, 0.0));
        assert_eq!(right[1].transform, Transform::translation(4.0, 0.0));
        assert_eq!(right[0].fitting_template_frame, Some(2));
        assert_eq!(right[1].fitting_template_frame, Some(3));
    }

    #[test]
    fn production_rolling_reader_failure_skips_solver_and_retains_last_success() {
        let stack = StackContext::new(3, 4, 4, 1);
        let mut solver_calls = 0;
        let rows = rolling_solver_leg_with(stack, &FailingReader, &ParamValues::new(), "unit-reader", 0, 1, None, |_, _, template, frame, _| {
            solver_calls += 1;
            assert_eq!((template, frame), (0, 2));
            Ok(AlignOutcome { transform: Transform::translation(1.0, 0.0), status: AlignStatus::Converged, iterations: 1, residual: 0.25 })
        }).unwrap();
        assert_eq!(solver_calls, 1);
        assert_eq!(rows[0].status, "read-error");
        assert_eq!(rows[0].fitting_template_frame, Some(0));
        assert_eq!(rows[1].transform, Transform::translation(1.0, 0.0));
        assert_eq!(rows[1].fitting_template_frame, Some(0));
    }
}
