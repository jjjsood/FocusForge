// SPDX-License-Identifier: GPL-3.0-or-later

//! [`FusionConsumer`]: the [`TileConsumer`] that drives *any* registered
//! [`StackAlgorithm`] through the tile scheduler and writes both of its
//! outputs -- the composite and the Z-Map. Algorithm-agnostic on purpose:
//! the golden tests, the CLI and every fusion algorithm all need exactly
//! this, and a copy living in a test file would be rewritten twice.

use std::fs::File;
use std::path::Path;
use std::sync::Mutex;

use crate::algorithm::{AlgorithmError, StackAlgorithm};
use crate::io::{TiffSpec, TiffWriter};
use crate::params::ParamValues;
use crate::tile::{Rect, TileConsumer, TileJob};
use crate::zmap::{ZMapInfo, ZMapWriter};

/// Drives one [`StackAlgorithm`] over every tile a scheduler hands it, and
/// writes the composite and, optionally, the Z-Map. Both writers are
/// `Mutex`-guarded, the `&self` + interior-mutability shape [`TileConsumer`]
/// requires, since `TiffWriter::write_tile` takes `&mut self` and parallel
/// tiles cannot write concurrently.
///
/// The algorithm itself still computes both outputs for *every* tile: the
/// algorithm never writes only one -- only the Z-Map *file* is skipped when
/// [`Self::create`] is given `None`.
///
/// **Every real run now passes `Some`**, because a stacking run writes a
/// project and a project carries its own Z-Map: the map is the document
/// itself, so a run that produced no map would produce nothing
/// worth keeping.
/// wants the composite alone can still ask for -- the library oracle in
/// `crates/cli/tests/stack_cli.rs` is one -- and it costs one 89.6 MB plane
/// per reference-scale run against a 268 MB composite when it is not used.
///
/// The Z-Map's path is explicit and never inside a frames directory
/// (`crate::zmap`'s own module docs): this type takes two separate paths,
/// never one directory to scan.
pub struct FusionConsumer {
    algorithm: &'static dyn StackAlgorithm,
    values: ParamValues,
    composite: Mutex<Option<TiffWriter<File>>>,
    zmap: Mutex<Option<ZMapWriter<File>>>,
}

/// The frame extent and count [`FusionConsumer::create`] needs to size both
/// output files -- bundled so `create` stays under clippy's argument-count
/// lint rather than growing a ninth positional parameter.
#[derive(Debug, Clone, Copy)]
pub struct FusionGeometry {
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    pub frame_count: u32,
}

impl FusionConsumer {
    /// Creates the composite output file, and the Z-Map output file when
    /// `zmap_path` is `Some`, writing every created file's header now, ready
    /// for [`TileConsumer::consume`] calls. `zmap_path: None` means no Z-Map
    /// file is written for this run at all -- never a scratch path created
    /// and deleted, since that would still pay the write cost. `values`
    /// must already be fully resolved (`crate::params::resolve` plus
    /// `crate::params::resolved_values` run by the caller).
    pub fn create(
        algorithm: &'static dyn StackAlgorithm,
        values: ParamValues,
        composite_path: &Path,
        zmap_path: Option<&Path>,
        geometry: FusionGeometry,
    ) -> Result<Self, AlgorithmError> {
        let FusionGeometry {
            width,
            height,
            samples_per_pixel,
            frame_count,
        } = geometry;
        let composite = TiffWriter::create(
            composite_path,
            TiffSpec {
                width,
                height,
                samples_per_pixel,
                description: None,
            },
        )?;
        let zmap = match zmap_path {
            Some(zmap_path) => {
                let zmap_info = ZMapInfo::new(width, height, frame_count).map_err(|source| {
                    AlgorithmError::Failed {
                        algorithm: algorithm.id().to_string(),
                        detail: source.to_string(),
                    }
                })?;
                let writer = ZMapWriter::create(zmap_path, zmap_info).map_err(|source| {
                    AlgorithmError::Failed {
                        algorithm: algorithm.id().to_string(),
                        detail: source.to_string(),
                    }
                })?;
                Some(writer)
            }
            None => None,
        };
        Ok(FusionConsumer {
            algorithm,
            values,
            composite: Mutex::new(Some(composite)),
            zmap: Mutex::new(zmap),
        })
    }

    /// Finishes the composite writer, and the Z-Map writer when one was
    /// created -- both already refuse an incompletely covered frame.
    pub fn finish(self) -> Result<(), AlgorithmError> {
        let composite = self
            .composite
            .lock()
            .unwrap()
            .take()
            .expect("composite writer present");
        composite.finish()?;
        if let Some(zmap) = self.zmap.lock().unwrap().take() {
            zmap.finish().map_err(|source| AlgorithmError::Failed {
                algorithm: self.algorithm.id().to_string(),
                detail: source.to_string(),
            })?;
        }
        Ok(())
    }
}

/// Copies the per-pixel window `interior` (in absolute frame coordinates)
/// out of a read-rect-sized Z-Map buffer into `out`. [`crate::tile::Tile::crop_into`]
/// cannot do this: it is `samples_per_pixel`-aware and a Z-Map has one
/// index per *pixel*, never per sample -- hence this hand-rolled row copy.
fn crop_zmap_into(zmap: &[u16], read_rect: Rect, interior: Rect, out: &mut [u16]) {
    debug_assert_eq!(out.len(), (interior.width * interior.height) as usize);
    let x_off = (interior.x - read_rect.x) as usize;
    let y_off = (interior.y - read_rect.y) as usize;
    let src_stride = read_rect.width as usize;
    let row_len = interior.width as usize;
    for row in 0..interior.height as usize {
        let src_start = (y_off + row) * src_stride + x_off;
        let dst_start = row * row_len;
        out[dst_start..dst_start + row_len].copy_from_slice(&zmap[src_start..src_start + row_len]);
    }
}

impl TileConsumer for FusionConsumer {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        let output = self.algorithm.fuse(job.ctx(), &self.values)?;
        let interior = job.interior();
        let spp = output.tile.samples_per_pixel;

        let mut cropped_composite =
            vec![0u16; interior.width as usize * interior.height as usize * spp as usize];
        output
            .tile
            .crop_into(interior, &mut cropped_composite)
            .map_err(|source| AlgorithmError::Failed {
                algorithm: self.algorithm.id().to_string(),
                detail: source.to_string(),
            })?;

        self.composite
            .lock()
            .unwrap()
            .as_mut()
            .expect("composite writer present")
            .write_tile(interior, &cropped_composite)?;

        if let Some(zmap_writer) = self.zmap.lock().unwrap().as_mut() {
            let mut cropped_zmap = vec![0u16; interior.width as usize * interior.height as usize];
            crop_zmap_into(&output.zmap, job.read_rect(), interior, &mut cropped_zmap);
            zmap_writer
                .write_tile(interior, &cropped_zmap)
                .map_err(|source| AlgorithmError::Failed {
                    algorithm: self.algorithm.id().to_string(),
                    detail: source.to_string(),
                })?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_zmap_into_extracts_the_interior_window() {
        // A 6x6 read rect (read origin (2,3) in absolute frame coordinates)
        // with a 2x2 interior starting at (5,6) -- exercising a nonzero
        // offset on both axes, the case a copy-paste from `crop_into`'s
        // spp-aware stride would get wrong.
        let read_rect = Rect {
            x: 2,
            y: 3,
            width: 6,
            height: 6,
        };
        let interior = Rect {
            x: 5,
            y: 6,
            width: 2,
            height: 2,
        };
        let zmap: Vec<u16> = (0..36).collect();
        let mut out = vec![0u16; 4];
        crop_zmap_into(&zmap, read_rect, interior, &mut out);
        // x_off = 5-2 = 3, y_off = 6-3 = 3, stride = 6.
        // row 0: idx (3*6+3)=21, 22 -> [21, 22]
        // row 1: idx (4*6+3)=27, 28 -> [27, 28]
        assert_eq!(out, vec![21, 22, 27, 28]);
    }
}
