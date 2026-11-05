// SPDX-License-Identifier: GPL-3.0-or-later

//! The sharp-area count over real frames: the same result at two tilings.

mod common;

use focusforge_core::io::open_frame_set;
use focusforge_core::params::ParamValues;
use focusforge_core::sharp_areas::count_sharp_areas;
use focusforge_core::tile::{HostBudget, NoControl};

#[test]
fn the_count_does_not_depend_on_the_tile_size() {
    let mut paths: Vec<_> = std::fs::read_dir(common::mantis_512_dir().join("frames"))
        .expect("the fixture frames directory exists")
        .map(|entry| entry.expect("a readable entry").path())
        .collect();
    paths.sort();
    let set = open_frame_set(
        &paths,
        HostBudget::host_derived(&focusforge_core::host::snapshot()),
    )
    .expect("the fixture frames open");
    let count = |tile_size| {
        count_sharp_areas(
            set.reader.as_ref(),
            set.reader.len(),
            set.reader.dimensions(),
            set.reader.frame(0).samples_per_pixel,
            &ParamValues::new(),
            tile_size,
            &NoControl,
        )
        .expect("the count runs")
    };
    let tiled = count(96);
    let whole = count(600);
    assert_eq!(tiled.cells, 64);
    assert_eq!(tiled.counts.len(), 8);
    assert!(tiled.counts.iter().any(|&n| n > 0), "{tiled:?}");
    assert_eq!(tiled, whole);
}
