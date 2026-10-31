// SPDX-License-Identifier: GPL-3.0-or-later

//! [`FusionConsumer`]: the [`TileConsumer`] that drives *any* registered
//! [`StackAlgorithm`] through the tile scheduler and writes both of its
//! outputs -- the composite and the Z-Map. Algorithm-agnostic on purpose:
//! the golden tests, the CLI and every fusion algorithm all need exactly
//! this, and a copy living in a test file would be rewritten twice.

use std::fs::File;
use std::path::Path;
use std::sync::Mutex;

use crate::algorithm::{AlgorithmError, StackAlgorithm, TileOutput};
use crate::fuse::error::algorithm_error;
use crate::io::{TiffError, TiffSpec, TiffWriter};
use crate::params::ParamValues;
use crate::tile::{Rect, SampleFormat, TileConsumer, TileJob, TilePlan};
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
/// project and a project carries its own Z-Map: the map is the document, so
/// a run that produced no map would produce nothing worth keeping. `None`
/// survives as the shape a caller that genuinely wants the composite alone
/// can still ask for -- the library oracle in `crates/cli/tests/stack_cli.rs`
/// is one -- and it costs one 89.6 MB plane per reference-scale run against
/// a 268 MB composite when it is not used.
///
/// The Z-Map's path is explicit and never inside a frames directory
/// (`crate::zmap`'s own module docs): this type takes two separate paths,
/// never one directory to scan.
pub struct FusionConsumer {
    algorithm: &'static dyn StackAlgorithm,
    values: ParamValues,
    composite: Mutex<Option<TiffWriter<File>>>,
    zmap: Mutex<Option<ZMapWriter<File>>>,
    /// The composite's sample format: `consume` fuses with `fuse` for `U16`
    /// and `fuse_f32` for `F32`, and the composite file is a 16-bit or a
    /// 32-bit float TIFF to match.
    format: SampleFormat,
    /// `None` means both outputs are declared at the full frame extent;
    /// `Some(rect)` means both are declared at `rect`'s dimensions, and
    /// every written tile is clipped to `rect` and translated by
    /// `-(rect.x, rect.y)` before it reaches either writer.
    crop: Option<Rect>,
}

/// The frame extent and count [`FusionConsumer::create`] needs to size both
/// output files -- bundled so `create` stays under clippy's argument-count
/// lint rather than growing a ninth positional parameter.
///
/// `crop: None` and `description: None` produce both outputs at the full
/// frame extent with `TiffWriter`'s own default description. Not `Copy`,
/// since `description` is an owned `Option<String>`.
#[derive(Debug, Clone)]
pub struct FusionGeometry {
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u32,
    pub frame_count: u32,
    pub crop: Option<Rect>,
    pub description: Option<String>,
}

impl FusionGeometry {
    /// The rect both output files are declared at: the crop when there is
    /// one, otherwise the whole frame -- the one computation `create`,
    /// `reopen` and `resume` each need before doing anything else.
    fn out_rect(&self) -> Rect {
        self.crop.unwrap_or(Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        })
    }
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
        Self::create_as(
            algorithm,
            values,
            composite_path,
            zmap_path,
            geometry,
            SampleFormat::U16,
        )
    }

    /// [`Self::create`] with the composite in `format`: `F32` writes a float
    /// TIFF from [`StackAlgorithm::fuse_f32`], nothing clamped.
    pub fn create_as(
        algorithm: &'static dyn StackAlgorithm,
        values: ParamValues,
        composite_path: &Path,
        zmap_path: Option<&Path>,
        geometry: FusionGeometry,
        format: SampleFormat,
    ) -> Result<Self, AlgorithmError> {
        let out_rect = geometry.out_rect();
        let FusionGeometry {
            samples_per_pixel,
            frame_count,
            crop,
            description,
            ..
        } = geometry;
        let spec = TiffSpec {
            width: out_rect.width,
            height: out_rect.height,
            samples_per_pixel,
            description,
        };
        let composite = match format {
            SampleFormat::U16 => TiffWriter::create(composite_path, spec)?,
            SampleFormat::F32 => TiffWriter::create_f32(composite_path, spec)?,
        };
        let zmap = match zmap_path {
            Some(zmap_path) => {
                let zmap_info = ZMapInfo::new(out_rect.width, out_rect.height, frame_count)
                    .map_err(algorithm_error(algorithm.id()))?;
                let writer = ZMapWriter::create(zmap_path, zmap_info)
                    .map_err(algorithm_error(algorithm.id()))?;
                Some(writer)
            }
            None => None,
        };
        Ok(FusionConsumer {
            algorithm,
            values,
            composite: Mutex::new(Some(composite)),
            zmap: Mutex::new(zmap),
            format,
            crop,
        })
    }

    /// Reopens both output files a previous, interrupted [`Self::create`]
    /// run already wrote to -- never truncates, mirrors `create` in every
    /// argument except the trailing `covered` rects, which reseed both
    /// writers' in-memory coverage trackers so a later `finish()` accepts
    /// the file once the remaining tiles are written.
    ///
    /// `covered` must be exactly the **output-file-coordinate** rects
    /// `written_rect` produced for every unit a resume is keeping as
    /// already complete -- the same rects [`Self::consume`] itself writes
    /// at, via that one shared function, so a resumed run cannot end up
    /// with a stripe of zeros at a crop edge that a fresh run would not
    /// have had.
    pub fn reopen(
        algorithm: &'static dyn StackAlgorithm,
        values: ParamValues,
        composite_path: &Path,
        zmap_path: Option<&Path>,
        geometry: FusionGeometry,
        covered: &[Rect],
    ) -> Result<Self, AlgorithmError> {
        Self::reopen_as(
            algorithm,
            values,
            composite_path,
            zmap_path,
            geometry,
            covered,
            SampleFormat::U16,
        )
    }

    /// [`Self::reopen`] for a composite [`Self::create_as`] made in `format`.
    pub fn reopen_as(
        algorithm: &'static dyn StackAlgorithm,
        values: ParamValues,
        composite_path: &Path,
        zmap_path: Option<&Path>,
        geometry: FusionGeometry,
        covered: &[Rect],
        format: SampleFormat,
    ) -> Result<Self, AlgorithmError> {
        let out_rect = geometry.out_rect();
        let FusionGeometry {
            samples_per_pixel,
            frame_count,
            crop,
            description,
            ..
        } = geometry;
        let spec = TiffSpec {
            width: out_rect.width,
            height: out_rect.height,
            samples_per_pixel,
            description,
        };
        let mut composite = reopen_writer(composite_path, spec, format)?;
        for &rect in covered {
            composite.seed_coverage(rect);
        }
        let zmap = match zmap_path {
            Some(zmap_path) => {
                let zmap_info = ZMapInfo::new(out_rect.width, out_rect.height, frame_count)
                    .map_err(algorithm_error(algorithm.id()))?;
                let mut writer = ZMapWriter::reopen(zmap_path, zmap_info)
                    .map_err(algorithm_error(algorithm.id()))?;
                for &rect in covered {
                    writer.seed_coverage(rect);
                }
                Some(writer)
            }
            None => None,
        };
        Ok(FusionConsumer {
            algorithm,
            values,
            composite: Mutex::new(Some(composite)),
            zmap: Mutex::new(zmap),
            format,
            crop,
        })
    }

    /// The resume composition: reopens both output files, filtering
    /// `completed` (a ledger's recorded-complete unit indices) through the
    /// composite file's own length -- a recorded unit whose rows the file
    /// cannot hold is dropped back into the set that must be recomputed,
    /// never trusted as done (`crate::io::TiffWriter::holds_rows`;
    /// `crates/core/src/job/ledger.rs`'s own module docs, "The ordering
    /// rule"). `TiffWriter::holds_rows` and `seed_coverage` are
    /// `pub(crate)`, so this composition -- the only place both a resume's
    /// length check and its coverage reseed happen together -- has to live
    /// inside this crate; this method is the public surface a resume drives
    /// instead.
    ///
    /// Returns the built consumer plus the unit indices that genuinely
    /// survived the check -- the set [`crate::job::Job::resuming`] must be
    /// built from, never `completed` itself.
    pub fn resume(
        plan: &TilePlan,
        completed: &[usize],
        algorithm: &'static dyn StackAlgorithm,
        values: ParamValues,
        composite_path: &Path,
        zmap_path: Option<&Path>,
        geometry: FusionGeometry,
    ) -> Result<(Self, Vec<usize>), AlgorithmError> {
        Self::resume_as(
            plan,
            completed,
            algorithm,
            values,
            composite_path,
            zmap_path,
            geometry,
            SampleFormat::U16,
        )
    }

    /// [`Self::resume`] for a composite [`Self::create_as`] made in `format`;
    /// a recorded unit is kept only when the file holds its rows at that
    /// format's sample width.
    #[allow(clippy::too_many_arguments)]
    pub fn resume_as(
        plan: &TilePlan,
        completed: &[usize],
        algorithm: &'static dyn StackAlgorithm,
        values: ParamValues,
        composite_path: &Path,
        zmap_path: Option<&Path>,
        geometry: FusionGeometry,
        format: SampleFormat,
    ) -> Result<(Self, Vec<usize>), AlgorithmError> {
        let crop = geometry.crop;
        let out_rect = geometry.out_rect();
        let probe = reopen_writer(
            composite_path,
            TiffSpec {
                width: out_rect.width,
                height: out_rect.height,
                samples_per_pixel: geometry.samples_per_pixel,
                description: geometry.description.clone(),
            },
            format,
        )?;

        let mut surviving_indices = Vec::new();
        let mut surviving_rects = Vec::new();
        for &index in completed {
            let Some(tile) = plan.tiles().get(index) else {
                // An index outside this plan's geometry -- never trusted.
                // `ResumeStore::find` already refuses a changed tile
                // geometry before a caller ever reaches this method; this
                // is a defensive backstop, not a path a correct caller hits.
                continue;
            };
            match written_rect(tile.interior(), crop) {
                Some(rect) => {
                    if probe.holds_rows(rect) {
                        surviving_indices.push(index);
                        surviving_rects.push(rect);
                    }
                }
                None => {
                    // The tile wrote nothing at all (wholly outside the
                    // crop): trivially complete, nothing to seed.
                    surviving_indices.push(index);
                }
            }
        }
        drop(probe);

        let consumer = Self::reopen_as(
            algorithm,
            values,
            composite_path,
            zmap_path,
            geometry,
            &surviving_rects,
            format,
        )?;
        Ok((consumer, surviving_indices))
    }

    /// The write tail: clips `output` against this consumer's crop (if any)
    /// and writes both the composite and, when one was created, the Z-Map.
    /// Extracted so a consumer that supplies its own pixels (never calling
    /// `self.algorithm.fuse` itself) can still reuse this exact tail --
    /// touches only [`TileJob::interior`]/[`TileJob::read_rect`], never
    /// [`TileJob::ctx`], which is what makes that reuse possible.
    pub fn write_output(
        &self,
        job: &TileJob<'_>,
        output: &TileOutput,
    ) -> Result<(), AlgorithmError> {
        self.write_output_as(job, output, TiffWriter::write_tile)
    }

    /// [`Self::write_output`] for an `f32` output, into a float composite.
    pub fn write_output_f32(
        &self,
        job: &TileJob<'_>,
        output: &TileOutput<f32>,
    ) -> Result<(), AlgorithmError> {
        self.write_output_as(job, output, TiffWriter::write_tile_f32)
    }

    /// The composite's sample format.
    pub fn format(&self) -> SampleFormat {
        self.format
    }

    fn write_output_as<S: Copy + Default>(
        &self,
        job: &TileJob<'_>,
        output: &TileOutput<S>,
        write: impl FnOnce(&mut TiffWriter<File>, Rect, &[S]) -> Result<(), TiffError>,
    ) -> Result<(), AlgorithmError> {
        let interior = job.interior();
        let spp = output.tile.samples_per_pixel;

        // Clip this job's interior against the crop, in absolute frame
        // coordinates: a tile wholly outside the crop writes nothing (both
        // outputs were declared at the crop's dimensions in `create`, so
        // there is nowhere for it to go), and a tile straddling the crop's
        // edge writes only its inside part.
        let target = match self.crop {
            Some(crop) => match interior.intersect(crop) {
                Some(clipped) => clipped,
                None => return Ok(()),
            },
            None => interior,
        };

        let mut cropped_composite =
            vec![S::default(); target.width as usize * target.height as usize * spp as usize];
        output
            .tile
            .crop_into(target, &mut cropped_composite)
            .map_err(algorithm_error(self.algorithm.id()))?;

        // The same arithmetic `reopen`'s coverage-seeding uses, via the one
        // shared `written_rect` function -- never a second, independently
        // written `translate_for_crop` call here.
        let target_written = written_rect(interior, self.crop)
            .expect("interior already clipped to the crop above, target is non-empty");

        write(
            self.composite
                .lock()
                .unwrap()
                .as_mut()
                .expect("composite writer present"),
            target_written,
            &cropped_composite,
        )?;

        if let Some(zmap_writer) = self.zmap.lock().unwrap().as_mut() {
            let mut cropped_zmap = vec![0u16; target.width as usize * target.height as usize];
            crop_zmap_into(&output.zmap, job.read_rect(), target, &mut cropped_zmap);
            zmap_writer
                .write_tile(target_written, &cropped_zmap)
                .map_err(algorithm_error(self.algorithm.id()))?;
        }

        Ok(())
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
            zmap.finish()
                .map_err(algorithm_error(self.algorithm.id()))?;
        }
        Ok(())
    }
}

fn reopen_writer(
    path: &Path,
    spec: TiffSpec,
    format: SampleFormat,
) -> Result<TiffWriter<File>, TiffError> {
    match format {
        SampleFormat::U16 => TiffWriter::reopen(path, spec),
        SampleFormat::F32 => TiffWriter::reopen_f32(path, spec),
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

/// `rect`, translated by `-(crop.x, crop.y)` when `crop` is `Some` --
/// absolute frame coordinates in, output-file coordinates out. `rect` must
/// already lie inside `crop` (guaranteed by [`Rect::intersect`]), so the
/// subtraction never underflows.
fn translate_for_crop(rect: Rect, crop: Option<Rect>) -> Rect {
    match crop {
        Some(crop) => Rect {
            x: rect.x - crop.x,
            y: rect.y - crop.y,
            width: rect.width,
            height: rect.height,
        },
        None => rect,
    }
}

/// The rectangle one tile's `interior` is actually written at in the output
/// files, given an optional crop -- [`Rect::intersect`] then
/// [`translate_for_crop`], factored out of [`FusionConsumer::consume`] so a
/// resume's [`FusionConsumer::reopen`] seeds coverage with **exactly** the
/// same arithmetic `consume` used to write it in the first place. `None`
/// when `interior` lies wholly outside `crop` -- that tile wrote nothing,
/// so there is nothing to seed.
pub(crate) fn written_rect(interior: Rect, crop: Option<Rect>) -> Option<Rect> {
    let target = match crop {
        Some(crop) => interior.intersect(crop)?,
        None => interior,
    };
    Some(translate_for_crop(target, crop))
}

impl TileConsumer for FusionConsumer {
    fn consume(&self, job: &TileJob<'_>) -> Result<(), AlgorithmError> {
        match self.format {
            SampleFormat::U16 => {
                let output = self.algorithm.fuse(job.ctx(), &self.values)?;
                self.write_output(job, &output)
            }
            SampleFormat::F32 => {
                let output = self.algorithm.fuse_f32(job.ctx(), &self.values)?;
                self.write_output_f32(job, &output)
            }
        }
    }

    /// Takes both writers out of their mutexes and drops them: called only
    /// after a genuine error, never on pause or cancel, so no half-covered
    /// file stays open and a later `finish()` cannot be attempted against
    /// it. Removing the `.partial` files themselves stays with the caller
    /// -- a job neither creates nor deletes output files.
    fn abort(&self) {
        self.composite.lock().unwrap().take();
        self.zmap.lock().unwrap().take();
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

    #[test]
    fn written_rect_matches_intersect_then_translate_with_no_crop() {
        let interior = Rect {
            x: 10,
            y: 20,
            width: 5,
            height: 6,
        };
        assert_eq!(written_rect(interior, None), Some(interior));
    }

    #[test]
    fn written_rect_clips_and_translates_against_a_crop() {
        let interior = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };
        let crop = Rect {
            x: 5,
            y: 5,
            width: 10,
            height: 10,
        };
        // Overlap in absolute coordinates is [5,10) -- a 5x5 rect --
        // translated by -(5,5) to output-file coordinates.
        assert_eq!(
            written_rect(interior, Some(crop)),
            Some(Rect {
                x: 0,
                y: 0,
                width: 5,
                height: 5,
            })
        );
    }

    #[test]
    fn written_rect_is_none_when_wholly_outside_the_crop() {
        let interior = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        let crop = Rect {
            x: 100,
            y: 100,
            width: 10,
            height: 10,
        };
        assert_eq!(written_rect(interior, Some(crop)), None);
    }
}
