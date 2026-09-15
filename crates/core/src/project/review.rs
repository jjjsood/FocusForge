// SPDX-License-Identifier: GPL-3.0-or-later

//! The two measures `focusforge-cli review` reports over one already-written
//! project: the composite sharpness ratio (divided from the manifest's
//! stored lapvars -- the composite blob is **never** reopened) and the
//! Z-Map spread (streamed, since no run stores that number).
//!
//! Nothing here prints: the module-level rule at `crate::project`'s own
//! doc comment.

use serde::{Deserialize, Serialize};

use crate::tile::Rect;
use crate::zmap::metrics::SpreadScan;

use super::{Plane, Project, ProjectError};

/// One result's review metrics: the sharpness ratio plus the two numbers it
/// was divided from, and the Z-Map spread.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReviewMetrics {
    /// `composite_lapvar / max_input_lapvar`, `None` when either input is
    /// `None` or the divisor is not `> 0.0`.
    pub sharpness_ratio: Option<f64>,
    pub composite_lapvar: Option<f64>,
    pub max_input_lapvar: Option<f64>,
    pub lapvar_window: Option<Rect>,
    /// How many distinct source frames the Z-Map actually uses.
    /// [`crate::zmap::NO_SOURCE`] is never counted.
    pub distinct_sources: u32,
    /// The all-8-identical coherence fraction,
    /// [`crate::zmap::metrics::all_eight_identical_fraction`]'s definition.
    pub coherence: f64,
}

/// Divides the manifest's stored sharpness numbers and streams the project's
/// Z-Map through a [`SpreadScan`]. The composite blob is never opened.
pub fn measure(project: &mut Project) -> Result<ReviewMetrics, ProjectError> {
    let metrics = project.manifest().metrics;
    let sharpness_ratio = match (metrics.composite_lapvar, metrics.max_input_lapvar) {
        (Some(composite), Some(max_input)) if max_input > 0.0 => Some(composite / max_input),
        _ => None,
    };

    let zmap = project.plane(Plane::ZMap);
    let (width, height) = (zmap.width, zmap.height);
    let mut scan = SpreadScan::new(width, height);
    project.read_plane_into(Plane::ZMap, &mut |band, rect| scan.push_band(band, rect))?;
    let spread = scan.finish();

    Ok(ReviewMetrics {
        sharpness_ratio,
        composite_lapvar: metrics.composite_lapvar,
        max_input_lapvar: metrics.max_input_lapvar,
        lapvar_window: metrics.lapvar_window,
        distinct_sources: spread.distinct_sources,
        coherence: spread.coherence,
    })
}
