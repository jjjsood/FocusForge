// SPDX-License-Identifier: GPL-3.0-or-later

//! `sharpness-map`: the focus score of every frame of a directory, and of a
//! composite, on one scale for the whole stack, written as `f32` TIFFs by
//! [`focusforge_core::sharpness_map`].

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use focusforge_core::io::{FrameReader, TiffSpec, TiffWriter, open_frame_set};
use focusforge_core::job::JobControl;
use focusforge_core::project::container::PartialFile;
use focusforge_core::sharp_areas::SharpAreasError;
use focusforge_core::sharpness_map::{SHARPNESS_PERCENTILE, sharpness_maps, stack_sharpness_scale};
use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;
use focusforge_core::tile::{HostBudget, TileControl};

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

/// A map write that failed, in the message the command prints.
struct MapFailed(String);

impl From<SharpAreasError> for MapFailed {
    fn from(err: SharpAreasError) -> Self {
        Self(err.to_string())
    }
}

/// Writes the maps of the first `frames` frames of `reader` beside
/// `final_paths` as partial files, in one pass over the tiles.
fn write_maps(
    reader: &dyn FrameReader,
    frames: usize,
    scale: f32,
    final_paths: &[PathBuf],
    control: &dyn TileControl,
) -> Result<Vec<PartialFile>, String> {
    let (width, height) = reader.dimensions();
    let spp = reader.frame(0).samples_per_pixel;
    let mut partials = Vec::with_capacity(frames);
    let mut writers = Vec::with_capacity(frames);
    for final_path in final_paths {
        let partial = PartialFile::new(final_path);
        writers.push(
            TiffWriter::create_f32(
                partial.path(),
                TiffSpec {
                    width,
                    height,
                    samples_per_pixel: 1,
                    description: None,
                },
            )
            .map_err(|err| err.to_string())?,
        );
        partials.push(partial);
    }
    sharpness_maps(
        reader,
        frames,
        (width, height),
        spp,
        scale,
        DEFAULT_TILE_SIZE,
        control,
        crate::sharpness_backend(),
        |frame, interior, map: &[f32]| {
            writers[frame]
                .write_tile_f32(interior, map)
                .map_err(|err| MapFailed(err.to_string()))
        },
    )
    .map_err(|MapFailed(message)| message)?;
    for writer in writers {
        writer.finish().map_err(|err| err.to_string())?;
    }
    Ok(partials)
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

    let frame_maps: Vec<Mapped> = paths
        .iter()
        .map(|path| {
            let stem = path
                .file_stem()
                .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
            Mapped {
                file: file_name(path),
                map: out.join(format!("{stem}-sharpness.tif")),
            }
        })
        .collect();
    let map_paths: Vec<PathBuf> = frame_maps.iter().map(|mapped| mapped.map.clone()).collect();
    let mut partials = write_maps(reader, reader.len(), scale, &map_paths, &*control)?;
    let composite_map = match &composite {
        None => None,
        Some((path, set)) => {
            let map = out.join("composite-sharpness.tif");
            partials.extend(write_maps(
                set.reader.as_ref(),
                1,
                scale,
                std::slice::from_ref(&map),
                &*control,
            )?);
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
