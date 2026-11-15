// SPDX-License-Identifier: GPL-3.0-or-later

//! `hot-pixels`: the pixels of a directory's frames that differ from their
//! neighbours at the same sensor position in more than half the frames,
//! found by [`focusforge_core::hot_pixels`].

use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::AtomicUsize;

use focusforge_core::hot_pixels::detect;
use focusforge_core::io::open_frame_set;
use focusforge_core::params::ParamDesc;
use focusforge_core::project::PREPARE_PARAMS;
use focusforge_core::tile::HostBudget;

use crate::set::{SetArgs, resolve_settings};

const THRESHOLD: &str = "hot-pixel-threshold";

/// The parameters this command reads: the threshold row of the prepare schema.
pub(crate) fn params() -> Vec<ParamDesc> {
    PREPARE_PARAMS
        .iter()
        .filter(|desc| desc.id == THRESHOLD)
        .cloned()
        .collect()
}

pub(crate) fn run(dir: &Path, set: &SetArgs, json: bool) -> ExitCode {
    let values = match resolve_settings(&params(), set) {
        Ok(values) => values,
        Err(message) => return crate::fail(message),
    };
    let threshold = values[THRESHOLD]
        .as_f64()
        .expect("the threshold parameter is a number");
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
    let workers = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let mask = match detect(
        reader,
        reader.len(),
        reader.dimensions(),
        reader.frame(0).samples_per_pixel,
        threshold,
        workers,
        &AtomicUsize::new(0),
        &|| false,
    ) {
        Ok(mask) => mask,
        Err(err) => return crate::fail(err),
    };
    if json {
        let document = serde_json::json!({
            "params": { THRESHOLD: values[THRESHOLD] },
            "width": mask.width(),
            "height": mask.height(),
            "count": mask.count(),
            "pixels": mask
                .positions()
                .into_iter()
                .map(|(x, y)| serde_json::json!({ "x": x, "y": y }))
                .collect::<Vec<_>>(),
        });
        match serde_json::to_string_pretty(&document) {
            Ok(text) => println!("{text}"),
            Err(err) => return crate::fail(format!("failed to serialize JSON: {err}")),
        }
    } else {
        println!("hot pixels: {}", mask.count());
    }
    ExitCode::SUCCESS
}
