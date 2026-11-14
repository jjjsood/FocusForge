// SPDX-License-Identifier: GPL-3.0-or-later

//! `sharp-areas`: how many areas of the image are sharpest in each frame of a
//! directory, counted by [`focusforge_core::sharp_areas`].

use std::path::Path;
use std::process::ExitCode;

use focusforge_core::io::open_frame_set;
use focusforge_core::job::JobControl;
use focusforge_core::sharp_areas::{SHARP_AREA_PARAMS, count_sharp_areas_reporting_with_backend};
use focusforge_core::tile::HostBudget;
use focusforge_core::tile::plan::DEFAULT_TILE_SIZE;

use crate::set::{SetArgs, resolve_settings};

/// How many frames the count reads at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Priority {
    /// One frame at a time, leaving the other cores to other work.
    Low,
    /// As many frames at once as the host has threads.
    Normal,
}

pub(crate) fn run(dir: &Path, set: &SetArgs, json: bool, priority: Priority) -> ExitCode {
    let values = match resolve_settings(&SHARP_AREA_PARAMS, set) {
        Ok(values) => values,
        Err(message) => return crate::fail(message),
    };
    let paths = match crate::info_frame_paths(dir) {
        Ok(paths) => paths,
        Err(err) => return crate::fail(err),
    };
    let frames = match open_frame_set(
        &paths,
        HostBudget::host_derived(&focusforge_core::host::snapshot()),
    ) {
        Ok(frames) => frames,
        Err(err) => return crate::fail(err),
    };
    let reader = frames.reader.as_ref();
    let control = JobControl::new();
    if priority == Priority::Low {
        control.set_admission_cap(Some(1));
    }
    let result = match count_sharp_areas_reporting_with_backend(
        reader,
        reader.len(),
        reader.dimensions(),
        reader.frame(0).samples_per_pixel,
        &values,
        DEFAULT_TILE_SIZE,
        &*control,
        &mut |_| {},
        crate::sharpness_backend(),
    ) {
        Ok(result) => result,
        Err(err) => return crate::fail(err),
    };
    let rows = paths
        .iter()
        .zip(&result.counts)
        .enumerate()
        .map(|(index, (path, count))| {
            let file = path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            (index, file, *count)
        });
    let pairs = result
        .overlaps
        .iter()
        .enumerate()
        .map(|(index, overlap)| (format!("{}-{}", index + 1, index + 2), overlap.share()));
    if json {
        let document = serde_json::json!({
            "params": {
                "cell-size": values["cell-size"],
                "contrast": values["contrast"],
                "overlap-threshold": values["overlap-threshold"],
            },
            "cells": result.cells,
            "frames": rows
                .map(|(index, file, count)| serde_json::json!({
                    "index": index,
                    "file": file,
                    "count": count,
                }))
                .collect::<Vec<_>>(),
            "overlaps": pairs
                .map(|(pair, share)| serde_json::json!({ "pair": pair, "share": share }))
                .collect::<Vec<_>>(),
        });
        match serde_json::to_string_pretty(&document) {
            Ok(text) => println!("{text}"),
            Err(err) => return crate::fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        for (index, file, count) in rows {
            println!("{index}  {file}  {count}");
        }
        for (pair, share) in pairs {
            match share {
                Some(share) => println!("overlap  {pair}  {:.1}%", share * 100.0),
                None => println!("overlap  {pair}  no data"),
            }
        }
    }
    ExitCode::SUCCESS
}
