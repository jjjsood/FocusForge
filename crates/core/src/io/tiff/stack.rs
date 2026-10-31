// SPDX-License-Identifier: GPL-3.0-or-later

//! A directory of TIFF frames opened as one stack, and the mmap-backed
//! tile reader over it. No API here takes or returns a whole frame: a tile
//! is the unit in, and the unit out.

use std::fs::File;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use memmap2::Mmap;

use super::super::error::TiffError;
use super::super::frame::{FrameDesc, FrameReader};
use super::super::paths::scan_frames;
use super::super::{TIFF_EXTENSIONS, has_extension};
use super::header::{ByteOrder, TAG_STRIP_OFFSETS, TiffHeader};
use crate::tile::{Rect, Sample, SampleFormat, Tile};
use crate::timing::{TimingCounter, measurement_scope, record_timing};

/// A directory of TIFF frames, opened once, mapped lazily per frame.
///
/// Uniformity across frames is *lenient*: dimensions, bits per sample,
/// samples per pixel, photometric interpretation and planar configuration
/// must agree, naming the first frame that differs. Byte order and
/// `RowsPerStrip` are per-frame properties this reader normalizes and never
/// compares — little-endian, single-row-strip frames and a big-endian,
/// multi-row-strip reference result must open as one uniform stack.
#[derive(Debug)]
pub struct TiffStack {
    headers: Vec<TiffHeader>,
    descs: Vec<FrameDesc>,
    maps: Vec<OnceLock<Mmap>>,
    /// Uniform across frames: `check_uniform` compares bit depths.
    format: SampleFormat,
}

impl TiffStack {
    /// Open every `*.tif`/`*.tiff` file (case-insensitively) directly in
    /// `dir`, or in `dir/frames` when that subdirectory exists. Entries are
    /// taken non-recursively, sorted bytewise by file name, and anything
    /// that is not a TIFF by extension is skipped silently. Zero matches is
    /// `NoFrames`; a matching file that fails to parse is that file's own
    /// named error.
    ///
    /// **Trap:** a reference result named to sort after its own source
    /// frames (`'2' < 'M'` in a name like `Mantis_Stacked.tif`) would be
    /// picked up as an extra frame by this function. Callers that want the
    /// source frames and a reference result separately must use
    /// [`Self::open_files`] with an explicit list, never `open` on a
    /// directory that holds both.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, TiffError> {
        let paths = frame_paths(dir.as_ref())?;
        Self::open_files(&paths)
    }

    /// Open exactly the files given, in the order given. No directory scan,
    /// no sorting.
    pub fn open_files(paths: &[PathBuf]) -> Result<Self, TiffError> {
        Self::open_checked(paths, |header| {
            header.require_fast_path().map(|()| SampleFormat::U16)
        })
    }

    /// Open the staged composite at `path`: the 16-bit unsigned or 32-bit
    /// float uncompressed chunky layout [`TiffHeader::require_staged_path`]
    /// accepts. Source frames never come through here; [`Self::open_files`]
    /// keeps refusing a float file.
    pub fn open_staged(path: &Path) -> Result<Self, TiffError> {
        Self::open_checked(std::slice::from_ref(&path.to_path_buf()), |header| {
            header.require_staged_path()
        })
    }

    fn open_checked(
        paths: &[PathBuf],
        accept: impl Fn(&TiffHeader) -> Result<SampleFormat, TiffError>,
    ) -> Result<Self, TiffError> {
        if paths.is_empty() {
            return Err(TiffError::NoFrames {
                path: PathBuf::new(),
            });
        }
        let mut headers = Vec::with_capacity(paths.len());
        let mut format = SampleFormat::U16;
        for path in paths {
            let file = File::open(path).map_err(|source| TiffError::Io {
                path: path.clone(),
                source,
            })?;
            let header = TiffHeader::parse(path, &file)?;
            format = accept(&header)?;
            if let Some(first) = headers.first() {
                check_uniform(first, &header)?;
            }
            headers.push(header);
        }
        let descs = headers.iter().map(FrameDesc::from).collect();
        let maps = headers.iter().map(|_| OnceLock::new()).collect();
        Ok(TiffStack {
            headers,
            descs,
            maps,
            format,
        })
    }

    /// The sample type the frames hold. Reads in the other type convert: a
    /// `u16` read of a float frame narrows with [`Sample::narrow`], an `f32`
    /// read of a 16-bit frame widens.
    pub fn sample_format(&self) -> SampleFormat {
        self.format
    }

    pub fn len(&self) -> usize {
        self.headers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.headers.is_empty()
    }

    pub fn header(&self, index: usize) -> &TiffHeader {
        &self.headers[index]
    }

    /// Frame 0's `(width, height)`. Valid on any non-empty stack: uniformity
    /// guarantees every frame agrees on dimensions.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.headers[0].width, self.headers[0].height)
    }

    fn mapping(&self, index: usize) -> Result<&Mmap, TiffError> {
        if let Some(m) = self.maps[index].get() {
            return Ok(m);
        }
        let m = map_file(&self.headers[index].path)?;
        // Benign race: if two threads lose this race simultaneously, each
        // maps the file once and the loser's mapping is simply dropped. A
        // mutex would serialize every first-touch of every frame for no
        // benefit `read_tile` needs.
        Ok(self.maps[index].get_or_init(|| m))
    }

    /// Read the pixels of `rect` from `frame` into a freshly allocated
    /// [`Tile`].
    pub fn read_tile(&self, frame: usize, rect: Rect) -> Result<Tile, TiffError> {
        let spp = self.headers[frame].samples_per_pixel;
        let len = rect.width as usize * rect.height as usize * spp as usize;
        let mut samples = vec![0u16; len];
        self.read_tile_into(frame, rect, &mut samples)?;
        Ok(Tile {
            rect,
            samples_per_pixel: spp,
            samples,
        })
    }

    /// Read the pixels of `rect` from `frame` into `out`, reusing its
    /// allocation. `out.len()` must equal exactly
    /// `rect.width * rect.height * samples_per_pixel`; no partial write on
    /// a mismatch.
    ///
    /// "Zero-decode" means no decompression, never zero copy: mmap bytes
    /// are `u16`-unaligned, a big-endian frame needs a byte swap, and a
    /// sub-row rectangle is non-contiguous in the mapping, so a per-sample
    /// conversion into `out` is unavoidable.
    pub fn read_tile_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [u16],
    ) -> Result<(), TiffError> {
        self.read_tile_step_into(frame, rect, NonZeroU32::MIN, out)
    }

    /// Read every `step`-th row and column of `rect` from `frame` — rect rows
    /// and columns `0, step, 2*step, …` — into `out`, nearest-neighbour:
    /// every sample written is one the frame holds, none is averaged.
    /// `out.len()` must equal exactly
    /// `rect.width.div_ceil(step) * rect.height.div_ceil(step) * samples_per_pixel`;
    /// `rect` itself is bounds-checked whole, as [`Self::read_tile_into`] does.
    pub fn read_tile_step_into(
        &self,
        frame: usize,
        rect: Rect,
        step: NonZeroU32,
        out: &mut [u16],
    ) -> Result<(), TiffError> {
        match self.format {
            SampleFormat::U16 => self.read_step_as(frame, rect, step, out, |order, c| {
                order.u16_from([c[0], c[1]])
            }),
            SampleFormat::F32 => self.read_step_as(frame, rect, step, out, |order, c| {
                u16::narrow(order.f32_from([c[0], c[1], c[2], c[3]]))
            }),
        }
    }

    /// [`Self::read_tile_into`] in `f32` samples on the 16-bit scale: a float
    /// frame is read as stored, a 16-bit one widened.
    pub fn read_tile_f32_into(
        &self,
        frame: usize,
        rect: Rect,
        out: &mut [f32],
    ) -> Result<(), TiffError> {
        let step = NonZeroU32::MIN;
        match self.format {
            SampleFormat::U16 => self.read_step_as(frame, rect, step, out, |order, c| {
                f32::from(order.u16_from([c[0], c[1]]))
            }),
            SampleFormat::F32 => self.read_step_as(frame, rect, step, out, |order, c| {
                order.f32_from([c[0], c[1], c[2], c[3]])
            }),
        }
    }

    /// The strip walk behind every read: `decode` turns one stored sample (the
    /// frame's `bytes_per_sample` bytes) into the requested type.
    fn read_step_as<T: Copy>(
        &self,
        frame: usize,
        rect: Rect,
        step: NonZeroU32,
        out: &mut [T],
        decode: impl Fn(ByteOrder, &[u8]) -> T,
    ) -> Result<(), TiffError> {
        let _scope = measurement_scope("tiff_read");
        let header = &self.headers[frame];
        let path = &header.path;

        let in_bounds = rect.width > 0
            && rect.height > 0
            && rect
                .x
                .checked_add(rect.width)
                .is_some_and(|xw| xw <= header.width)
            && rect
                .y
                .checked_add(rect.height)
                .is_some_and(|yh| yh <= header.height);
        if !in_bounds {
            return Err(TiffError::OutOfBounds {
                path: path.clone(),
                requested: rect,
                frame: (header.width, header.height),
            });
        }

        let spp = header.samples_per_pixel as usize;
        let step = step.get();
        let out_row_len = rect.width.div_ceil(step) as usize * spp;
        let expected_len = out_row_len * rect.height.div_ceil(step) as usize;
        if out.len() != expected_len {
            return Err(TiffError::BufferSize {
                expected: expected_len,
                got: out.len(),
            });
        }

        let map = self.mapping(frame)?;
        let bytes_per_sample = header.bytes_per_sample() as u64;
        let stride = spp as u64 * bytes_per_sample;
        let row_bytes = header.row_bytes();
        let rect_row_bytes = rect.width as u64 * stride;
        let rps = u64::from(header.rows_per_strip.max(1));

        let mut out_off = 0usize;
        for r in (rect.y..(rect.y + rect.height)).step_by(step as usize) {
            let r = u64::from(r);
            let strip = (r / rps) as usize;
            let row_in_strip = r % rps;

            let strip_offset =
                *header
                    .strip_offsets
                    .get(strip)
                    .ok_or_else(|| TiffError::StripTableMismatch {
                        path: path.clone(),
                        found: header.strip_offsets.len(),
                        expected: strip + 1,
                    })?;

            if let Some(&byte_count) = header.strip_byte_counts.get(strip) {
                let need = (row_in_strip + 1)
                    .checked_mul(row_bytes)
                    .ok_or_else(|| past_eof(path))?;
                if need > byte_count {
                    return Err(TiffError::StripTooShort {
                        path: path.clone(),
                        strip,
                        have: byte_count,
                        need,
                    });
                }
            }

            let start = strip_offset
                .checked_add(
                    row_in_strip
                        .checked_mul(row_bytes)
                        .ok_or_else(|| past_eof(path))?,
                )
                .and_then(|v| v.checked_add((rect.x as u64).checked_mul(stride)?))
                .ok_or_else(|| past_eof(path))?;
            let end = start
                .checked_add(rect_row_bytes)
                .ok_or_else(|| past_eof(path))?;
            if end > map.len() as u64 {
                return Err(past_eof(path));
            }
            let row_slice = &map[start as usize..end as usize];
            let out_row = &mut out[out_off..out_off + out_row_len];

            let width = bytes_per_sample as usize;
            if step == 1 {
                // The CPU pipeline's hot path: one flat per-sample loop.
                for (i, chunk) in row_slice.chunks_exact(width).enumerate() {
                    out_row[i] = decode(header.byte_order, chunk);
                }
            } else {
                let pixels = row_slice
                    .chunks_exact(stride as usize)
                    .step_by(step as usize);
                for (dst, pixel) in out_row.chunks_exact_mut(spp).zip(pixels) {
                    for (d, chunk) in dst.iter_mut().zip(pixel.chunks_exact(width)) {
                        *d = decode(header.byte_order, chunk);
                    }
                }
            }
            out_off += out_row_len;
        }

        record_timing(
            TimingCounter::ConvertedBytes,
            std::mem::size_of_val(out) as u64,
        );
        Ok(())
    }
}

impl FrameReader for TiffStack {
    fn len(&self) -> usize {
        TiffStack::len(self)
    }

    fn dimensions(&self) -> (u32, u32) {
        TiffStack::dimensions(self)
    }

    fn frame(&self, index: usize) -> &FrameDesc {
        &self.descs[index]
    }
}

fn past_eof(path: &Path) -> TiffError {
    TiffError::ValuePastEof {
        path: path.to_path_buf(),
        tag: TAG_STRIP_OFFSETS,
    }
}

/// Enumerate the TIFF frames of `dir`: non-recursive, `*.tif`/`*.tiff`
/// case-insensitively, bytewise-sorted by file name, descending into
/// `dir/frames` when that subdirectory exists. Zero matches is `NoFrames`.
/// Shared by [`TiffStack::open`] and `focusforge-cli info` so a directory is
/// enumerated the same way by both, by construction rather than by a second
/// copy of the rule.
pub fn frame_paths(dir: &Path) -> Result<Vec<PathBuf>, TiffError> {
    scan_frames(dir, TIFF_EXTENSIONS, is_frame_path)
}

/// The composite plane every fuse writes beside its source frames.
pub const COMPOSITE_FILENAME: &str = "composite.tif";
/// The Z-Map plane every fuse writes beside its source frames.
pub const ZMAP_FILENAME: &str = "zmap.tif";

/// Whether `path` is a TIFF frame: `.tif`/`.tiff` by extension,
/// case-insensitively, and not one of a project's own output files sitting
/// beside its sources from an earlier fuse. The one place that filter is
/// spelled -- [`frame_paths`] calls it, and so does the GUI's classification
/// of a multi-file drop, rather than a second copy of the same check in
/// `crates/gui`.
pub fn is_frame_path(path: &Path) -> bool {
    if !has_extension(path, TIFF_EXTENSIONS) {
        return false;
    }
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name != COMPOSITE_FILENAME && name != ZMAP_FILENAME,
        None => true,
    }
}

/// Lenient uniformity: dimensions, bits per sample, samples per pixel,
/// photometric and planar configuration must agree with `first`. Byte order
/// and `RowsPerStrip` are deliberately not compared.
pub(crate) fn check_uniform(first: &TiffHeader, other: &TiffHeader) -> Result<(), TiffError> {
    let mut detail = Vec::new();
    if first.width != other.width || first.height != other.height {
        detail.push(format!(
            "{}x{} vs first frame's {}x{}",
            other.width, other.height, first.width, first.height
        ));
    }
    if first.bits_per_sample != other.bits_per_sample {
        detail.push(format!(
            "bits_per_sample {:?} vs first frame's {:?}",
            other.bits_per_sample, first.bits_per_sample
        ));
    }
    if first.samples_per_pixel != other.samples_per_pixel {
        detail.push(format!(
            "samples_per_pixel {} vs first frame's {}",
            other.samples_per_pixel, first.samples_per_pixel
        ));
    }
    if first.photometric != other.photometric {
        detail.push(format!(
            "photometric {} vs first frame's {}",
            other.photometric, first.photometric
        ));
    }
    if first.planar_config != other.planar_config {
        detail.push(format!(
            "planar_config {} vs first frame's {}",
            other.planar_config, first.planar_config
        ));
    }

    if detail.is_empty() {
        Ok(())
    } else {
        Err(TiffError::FrameMismatch {
            path: other.path.clone(),
            first: first.path.clone(),
            detail: detail.join("; "),
        })
    }
}

/// Map `path` read-only for the lifetime of the returned [`Mmap`].
///
/// # Safety
///
/// This is the one place in the crate that touches `unsafe`
/// (`memmap2::Mmap::map` is itself `unsafe fn`): mapping a file is
/// undefined behavior if the file is modified while the mapping is alive
/// (memmap2 0.9.11 docs, <https://docs.rs/memmap2/0.9.11/memmap2/struct.Mmap.html>).
/// This reader never writes through a mapping it holds, and every caller in
/// this crate opens a frame read-only for the mapping's whole lifetime;
/// nothing here mutates a TIFF frame while any process might be mapping it.
/// The `File`
/// used to create the mapping is dropped as soon as this function returns;
/// the mapping keeps the underlying pages alive on its own.
// SAFETY: contract above; the only `unsafe` call this crate makes.
#[allow(unsafe_code)]
pub(crate) fn map_file(path: &Path) -> Result<Mmap, TiffError> {
    let file = File::open(path).map_err(|source| TiffError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // SAFETY: see this function's doc comment above -- every workflow in
    // this repository treats a mapped TIFF file as read-only for as long as
    // any mapping of it might be alive.
    let mmap = unsafe { Mmap::map(&file) }.map_err(|source| TiffError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(mmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_frame_path_accepts_tif_and_tiff_case_insensitively() {
        assert!(is_frame_path(Path::new("a.tif")));
        assert!(is_frame_path(Path::new("a.TIF")));
        assert!(is_frame_path(Path::new("a.tiff")));
        assert!(is_frame_path(Path::new("a.TIFF")));
        assert!(!is_frame_path(Path::new("a.png")));
        assert!(!is_frame_path(Path::new("a")));
    }

    #[test]
    fn is_frame_path_excludes_a_projects_own_output_files() {
        assert!(!is_frame_path(Path::new(COMPOSITE_FILENAME)));
        assert!(!is_frame_path(Path::new(ZMAP_FILENAME)));
        assert!(!is_frame_path(Path::new("some/dir/composite.tif")));
        assert!(!is_frame_path(Path::new("some/dir/zmap.tif")));
    }

    #[test]
    fn frame_paths_excludes_a_prior_fuses_own_outputs() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["00.tif", "01.tif", "02.tif"] {
            std::fs::write(
                dir.path().join(name),
                b"not a real tiff, frame_paths never reads it",
            )
            .unwrap();
        }
        std::fs::write(
            dir.path().join(COMPOSITE_FILENAME),
            b"a prior fuse's own composite",
        )
        .unwrap();
        std::fs::write(dir.path().join(ZMAP_FILENAME), b"a prior fuse's own zmap").unwrap();

        let found = frame_paths(dir.path()).unwrap();
        assert_eq!(
            found.len(),
            3,
            "the project's own prior outputs must not count as source frames"
        );
        for path in &found {
            let name = path.file_name().unwrap().to_str().unwrap();
            assert_ne!(name, COMPOSITE_FILENAME);
            assert_ne!(name, ZMAP_FILENAME);
        }
    }
}
