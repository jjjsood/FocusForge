// SPDX-License-Identifier: GPL-3.0-or-later

//! `sharpness-map`: the focus score of every frame of a directory, and of a
//! composite, on one scale for the whole stack, written as `f32` TIFFs by
//! [`focusforge_core::sharpness_map`].

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use focusforge_core::io::{FrameReader, TiffSpec, TiffWriter, open_frame_set};
use focusforge_core::job::JobControl;
use focusforge_core::project::container::PartialFile;
use focusforge_core::sharpness_map::{
    SHARPNESS_PERCENTILE, sharpness_map_tile, sharpness_plan, stack_sharpness_scale,
};
use focusforge_core::tile::HostBudget;
use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;

use crate::sharp_areas::Priority;

/// A source image and the map written for it.
struct Mapped {
    file: String,
    map: PathBuf,
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Writes the map of `frame` of `reader` beside `final_path` as a partial file.
fn write_map(
    reader: &dyn FrameReader,
    frame: usize,
    scale: f32,
    final_path: &Path,
) -> Result<PartialFile, String> {
    let (width, height) = reader.dimensions();
    let spp = reader.frame(frame).samples_per_pixel;
    let plan = sharpness_plan((width, height), DEFAULT_TILE_SIZE).map_err(|err| err.to_string())?;
    let partial = PartialFile::new(final_path);
    let mut writer = TiffWriter::create_f32(
        partial.path(),
        TiffSpec {
            width,
            height,
            samples_per_pixel: 1,
            description: None,
        },
    )
    .map_err(|err| err.to_string())?;
    for tile in plan.tiles() {
        let map =
            sharpness_map_tile(reader, frame, tile, spp, scale).map_err(|err| err.to_string())?;
        writer
            .write_tile_f32(tile.interior(), &map)
            .map_err(|err| err.to_string())?;
    }
    writer.finish().map_err(|err| err.to_string())?;
    Ok(partial)
}

/// The scale and the maps of one run; nothing is on disk unless this is `Ok`.
fn build(
    dir: &Path,
    out: &Path,
    composite: Option<&Path>,
    priority: Priority,
) -> Result<(f32, Vec<Mapped>, Option<Mapped>), String> {
    let paths = crate::info_frame_paths(dir).map_err(|err| err.to_string())?;
    let budget = || HostBudget::host_derived(&focusforge_core::host::snapshot());
    let frames = open_frame_set(&paths, budget()).map_err(|err| err.to_string())?;
    let reader = frames.reader.as_ref();
    let extent = reader.dimensions();
    let spp = reader.frame(0).samples_per_pixel;

    let composite = match composite {
        None => None,
        Some(path) => {
            let set =
                open_frame_set(&[path.to_path_buf()], budget()).map_err(|err| err.to_string())?;
            let (size, found) = (
                set.reader.dimensions(),
                set.reader.frame(0).samples_per_pixel,
            );
            if size != extent || found != spp {
                return Err(format!(
                    "composite {} is {}x{} with {found} samples per pixel, the frames are {}x{} with {spp}",
                    path.display(),
                    size.0,
                    size.1,
                    extent.0,
                    extent.1,
                ));
            }
            Some((path, set))
        }
    };
    std::fs::create_dir_all(out)
        .map_err(|err| format!("cannot create output directory {}: {err}", out.display()))?;

    let control = JobControl::new();
    if priority == Priority::Low {
        control.set_admission_cap(Some(1));
    }
    let scale = stack_sharpness_scale(
        reader,
        reader.len(),
        extent,
        spp,
        DEFAULT_TILE_SIZE,
        &*control,
    )
    .map_err(|err| err.to_string())?;

    let mut partials = Vec::new();
    let mut frame_maps = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let stem = path
            .file_stem()
            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
        let map = out.join(format!("{stem}-sharpness.tif"));
        partials.push(write_map(reader, index, scale, &map)?);
        frame_maps.push(Mapped {
            file: file_name(path),
            map,
        });
    }
    let composite_map = match &composite {
        None => None,
        Some((path, set)) => {
            let map = out.join("composite-sharpness.tif");
            partials.push(write_map(set.reader.as_ref(), 0, scale, &map)?);
            Some(Mapped {
                file: file_name(path),
                map,
            })
        }
    };

    let mut committed = Vec::new();
    for (partial, map) in partials
        .into_iter()
        .zip(frame_maps.iter().chain(&composite_map))
    {
        if let Err(err) = partial.commit() {
            for done in committed {
                let _ = std::fs::remove_file(done);
            }
            return Err(format!("cannot write {}: {err}", map.map.display()));
        }
        committed.push(&map.map);
    }
    Ok((scale, frame_maps, composite_map))
}

pub(crate) fn run(
    dir: &Path,
    out: &Path,
    composite: Option<&Path>,
    json: bool,
    priority: Priority,
) -> ExitCode {
    let (scale, frames, composite) = match build(dir, out, composite, priority) {
        Ok(built) => built,
        Err(message) => return crate::fail(message),
    };
    if json {
        let document = serde_json::json!({
            "percentile": SHARPNESS_PERCENTILE,
            "scale": f64::from(scale),
            "frames": frames
                .iter()
                .enumerate()
                .map(|(index, mapped)| serde_json::json!({
                    "index": index,
                    "file": mapped.file,
                    "map": mapped.map,
                }))
                .collect::<Vec<_>>(),
            "composite": composite.as_ref().map(|mapped| serde_json::json!({
                "file": mapped.file,
                "map": mapped.map,
            })),
        });
        match serde_json::to_string_pretty(&document) {
            Ok(text) => println!("{text}"),
            Err(err) => return crate::fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        println!("scale  {scale}");
        for (index, mapped) in frames.iter().enumerate() {
            println!("{index}  {}  {}", mapped.file, mapped.map.display());
        }
        if let Some(mapped) = &composite {
            println!("composite  {}  {}", mapped.file, mapped.map.display());
        }
    }
    ExitCode::SUCCESS
}
